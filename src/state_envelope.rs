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
