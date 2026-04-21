// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

use crate::effect::*;

use xmrs::{prelude::Waveform, waveform::WaveformState};

#[derive(Default, Clone, Copy, Debug)]
pub struct VibratoTremolo {
    pub waveform: WaveformState,
    pub speed: f32,
    pub depth: f32,
}

impl VibratoTremolo {
    /// return depth * waveform value position
    fn waveform(&mut self, pos: f32) -> f32 {
        self.depth * self.waveform.value(pos)
    }
}

#[derive(Default, Clone, Copy, Debug)]
pub struct EffectVibratoTremolo {
    pub data: VibratoTremolo,
    in_progress: bool,
    pos: f32,
    value: f32,
}

impl EffectVibratoTremolo {
    pub fn new(wf: Waveform) -> Self {
        let mut evt = Self::default();
        evt.data.waveform = WaveformState::new(wf);
        evt
    }

    /// Zero the output modulation while preserving the LFO phase.
    ///
    /// Use this in preference to `retrigger()` when the effect merely
    /// disappears from the next row — `retrigger()` additionally resets the
    /// phase and should only be called on new-note triggers.
    pub fn clear_output(&mut self) {
        self.in_progress = false;
        self.value = 0.0;
        // `self.pos` intentionally preserved.
    }
}

impl EffectPlugin for EffectVibratoTremolo {
    /* param1: speed, param2:depth */
    fn tick0(&mut self, speed: f32, depth: f32) -> f32 {
        self.data.speed = speed;
        self.data.depth = depth;
        self.retrigger()
    }

    fn tick(&mut self) -> f32 {
        self.in_progress = true;
        self.value = self.data.waveform(self.pos);
        self.pos += self.data.speed;
        self.pos %= 1.0;
        self.value()
    }

    fn in_progress(&self) -> bool {
        self.in_progress
    }

    fn retrigger(&mut self) -> f32 {
        self.in_progress = false;
        self.pos = 0.0;
        self.value = 0.0;
        self.value
    }

    fn value(&self) -> f32 {
        self.value
    }
}
