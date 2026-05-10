/// An Instrument Envelope State
use xmrs::fixed::units::EnvValue;
use xmrs::prelude::*;

#[derive(Clone)]
pub struct StateEnvelope<'a> {
    env: &'a Envelope,
    default_value: EnvValue,
    pub enabled: bool,
    pub value: EnvValue,
    pub counter: usize,
}

impl<'a> StateEnvelope<'a> {
    /// `default_value` is what the envelope sits at when there
    /// are no points (or before the first point fires). Volume
    /// envelopes pass [`EnvValue::VOLUME_DEFAULT`] (≈ 1.0),
    /// panning / pitch envelopes pass [`EnvValue::PAN_DEFAULT`]
    /// (= 0.5 = centre / no pitch deviation).
    pub fn new(env: &'a Envelope, default_value: EnvValue) -> Self {
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

    /// `true` when the envelope's counter has walked past the last
    /// point's frame. Combined with the fact that the counter is
    /// only allowed to grow past the last point when neither sustain
    /// hold nor regular loop has wrapped it, this is precisely the
    /// condition under which schism's `_process_envelope`
    /// (`sndmix.c:493-499`) sets the fadeout-engage flag for the
    /// volume envelope.
    ///
    /// Used by [`crate::state_instr_default::StateInstrDefault::envelopes`]
    /// to defer the fadeout register's decay until the volume
    /// envelope's release section is genuinely over — the IT
    /// "release plays out, then fade" behaviour. The previous code
    /// started fadeout the moment `sustained` flipped, which made
    /// any keyed-off voice with a release section get cut while
    /// still in release.
    pub fn is_past_last_point(&self) -> bool {
        let n = self.env.point.len();
        n > 0 && self.counter > self.env.point[n - 1].frame
    }

    pub fn reset(&mut self) {
        self.enabled = self.env.enabled;
        self.value = self.default_value;
        self.counter = 0;
    }

    pub fn tick(&mut self, sustained: bool) {
        let num_points = self.env.point.len();

        if num_points == 0 {
            // No envelope points: hold the caller-provided default
            // (`VOLUME_DEFAULT` = 1.0 for volume envelopes,
            // `PAN_DEFAULT` = 0.5 for pan / pitch envelopes), not
            // `EnvValue::default()` which is the Q1.15 zero —
            // that would silence volume and hard-pan-left the
            // pan envelope. Defensive: importers normally replace
            // invalid envelopes with `Envelope::default()` whose
            // `enabled = false`, so `tick()` shouldn't fire here in
            // practice, but a malformed module slipping through
            // with `enabled = true, point = []` would otherwise go
            // silent.
            self.value = self.default_value;
            return;
        }

        if num_points == 1 {
            // The previous code did `.min(1.0)` here as a
            // defensive cap. With Q1.15 storage the value
            // cannot exceed `Q15::ONE` by construction, so the
            // cap is no longer necessary.
            self.value = self.env.point[0].value;
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
    use xmrs::fixed::fixed::Q15;

    /// Helper: build an `EnvValue` from an integer ratio so tests
    /// stay readable. `ev(0, 1)` = 0.0, `ev(1, 1)` = 1.0,
    /// `ev(3, 10)` = 0.3, etc.
    fn ev(num: i32, den: i32) -> EnvValue {
        EnvValue::from_q15(Q15::from_ratio(num, den))
    }

    fn make_envelope(points: Vec<(usize, EnvValue)>) -> Envelope {
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
        let env = make_envelope(vec![(0, ev(1, 1)), (10, ev(1, 2)), (20, ev(0, 1))]);
        let mut state = StateEnvelope::new(&env, EnvValue::VOLUME_DEFAULT);
        state.counter = 50; // well past the final point
        state.tick(false);
        assert_eq!(state.value, ev(0, 1), "should clamp to last point value");
    }

    #[test]
    fn envelope_clamps_to_last_point_nonzero_tail() {
        // Envelope that ends at a non-zero value: the held value must be
        // that of the last point, not the default. Q1.15 quantisation
        // means a small drift around the target is expected; we tolerate
        // a few raw LSBs (≪ 1 dB).
        let env = make_envelope(vec![(0, ev(0, 1)), (10, ev(3, 10))]);
        let mut state = StateEnvelope::new(&env, EnvValue::VOLUME_DEFAULT);
        state.counter = 100;
        state.tick(false);
        let target = ev(3, 10);
        let drift = (state.value.raw_q15().raw() - target.raw_q15().raw()).abs();
        assert!(drift <= 4, "expected ≈ 0.3, drift {} LSB", drift);
    }

    #[test]
    fn envelope_position_set_past_end_does_not_stay_at_default() {
        // Repro for the InstrumentVolumeEnvelopePosition case: if L command
        // sets counter beyond the last point, the value must reflect the
        // last point, not the default (1.0).
        let env = make_envelope(vec![(0, ev(1, 1)), (50, ev(0, 1))]);
        let mut state = StateEnvelope::new(&env, EnvValue::VOLUME_DEFAULT);
        // Simulate setting the counter past the tail
        state.counter = 999;
        state.tick(false);
        assert_eq!(state.value, ev(0, 1));
    }

    #[test]
    fn envelope_counter_at_last_point_exactly() {
        // Exactly at the last point frame: value should be the last point.
        let env = make_envelope(vec![(0, ev(1, 1)), (10, ev(1, 2)), (20, ev(1, 4))]);
        let mut state = StateEnvelope::new(&env, EnvValue::VOLUME_DEFAULT);
        state.counter = 20;
        state.tick(false);
        let target = ev(1, 4);
        let drift = (state.value.raw_q15().raw() - target.raw_q15().raw()).abs();
        assert!(drift <= 4, "expected ≈ 0.25, drift {} LSB", drift);
    }
}
