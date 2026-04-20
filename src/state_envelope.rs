/// An Instrument Envelope State
use xmrs::prelude::*;

#[derive(Clone)]
pub struct StateEnvelope<'a> {
    env: &'a Envelope,
    default_value: f32,
    pub enabled: bool,
    pub value: f32,
    pub counter: usize,
}

impl<'a> StateEnvelope<'a> {
    // value is volume_envelope_volume=1.0 or volume_envelope_panning=0.5
    pub fn new(env: &'a Envelope, default_value: f32) -> Self {
        let enabled = env.enabled;
        Self {
            env,
            default_value,
            enabled,
            value: default_value,
            counter: 0,
        }
    }

    pub fn has_volume_envelope(&self) -> bool {
        self.env.enabled || self.enabled
    }

    pub fn reset(&mut self) {
        self.enabled = self.env.enabled;
        self.value = self.default_value;
        self.counter = 0;
    }

    pub fn tick(&mut self, sustained: bool) {
        let num_points = self.env.point.len();

        if num_points == 0 {
            self.value = 0.0;
            return;
        }

        if num_points == 1 {
            self.value = self.env.point[0].value.min(1.0);
            return;
        }

        if sustained {
            self.counter = self.env.loop_in_sustain(self.counter);
        } else {
            self.counter = self.env.loop_in_loop(self.counter);
        }

        // Hold the last point's value once the counter reaches or exceeds
        // it. Without this, an envelope without a loop whose tail sits at
        // a given value (typically 0) never applies that value: the
        // segment loop below exits silently and `self.value` keeps its
        // default (1.0 volume / 0.5 panning), so a sample that should
        // have faded to silence keeps playing.
        let last_idx = num_points - 1;
        if self.counter >= self.env.point[last_idx].frame {
            self.value = self.env.point[last_idx].value;
            self.counter = self.counter.saturating_add(1);
            return;
        }

        for i in 1..num_points {
            let prev_point = &self.env.point[i - 1];
            let curr_point = &self.env.point[i];

            if self.counter == prev_point.frame {
                self.value = prev_point.value;
                break;
            }

            if self.counter <= curr_point.frame {
                self.value = EnvelopePoint::lerp(prev_point, curr_point, self.counter);
                break;
            }

            if prev_point.frame >= curr_point.frame {
                self.value = prev_point.value;
                break;
            }
        }

        self.counter += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn make_envelope(points: Vec<(usize, f32)>) -> Envelope {
        Envelope {
            enabled: true,
            point: points
                .into_iter()
                .map(|(frame, value)| EnvelopePoint { frame, value })
                .collect(),
            ..Default::default()
        }
    }

    #[test]
    fn envelope_clamps_to_last_point_when_counter_exceeds_end() {
        // Non-looping envelope that decays to zero: after the last point,
        // tick() must return the last value (0.0), not the default 1.0.
        let env = make_envelope(vec![(0, 1.0), (10, 0.5), (20, 0.0)]);
        let mut state = StateEnvelope::new(&env, 1.0);
        state.counter = 50; // well past the final point
        state.tick(false);
        assert_eq!(state.value, 0.0, "should clamp to last point value");
    }

    #[test]
    fn envelope_clamps_to_last_point_nonzero_tail() {
        // Envelope that ends at a non-zero value: the held value must be
        // that of the last point, not the default.
        let env = make_envelope(vec![(0, 0.0), (10, 0.3)]);
        let mut state = StateEnvelope::new(&env, 1.0);
        state.counter = 100;
        state.tick(false);
        assert!(
            (state.value - 0.3).abs() < 1e-6,
            "expected 0.3, got {}",
            state.value
        );
    }

    #[test]
    fn envelope_position_set_past_end_does_not_stay_at_default() {
        // Repro for the InstrumentVolumeEnvelopePosition case: if L command
        // sets counter beyond the last point, the value must reflect the
        // last point, not the default (1.0).
        let env = make_envelope(vec![(0, 1.0), (50, 0.0)]);
        let mut state = StateEnvelope::new(&env, 1.0);
        // Simulate setting the counter past the tail
        state.counter = 999;
        state.tick(false);
        assert_eq!(state.value, 0.0);
    }

    #[test]
    fn envelope_counter_at_last_point_exactly() {
        // Exactly at the last point frame: value should be the last point.
        let env = make_envelope(vec![(0, 1.0), (10, 0.5), (20, 0.25)]);
        let mut state = StateEnvelope::new(&env, 1.0);
        state.counter = 20;
        state.tick(false);
        assert!(
            (state.value - 0.25).abs() < 1e-6,
            "expected 0.25, got {}",
            state.value
        );
    }
}
