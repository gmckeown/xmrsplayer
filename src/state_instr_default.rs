#[cfg(feature = "micromath")]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(feature = "libm")]
#[allow(unused_imports)]
use num_traits::float::Float;

use core::ops::Deref;

use crate::{
    state_auto_vibrato::StateAutoVibrato, state_envelope::StateEnvelope, state_sample::StateSample,
};
use xmrs::prelude::*;

impl<'a> Deref for StateInstrDefault<'a> {
    type Target = InstrDefault;
    fn deref(&self) -> &InstrDefault {
        &self.instr
    }
}

/// An InstrDefault State
#[derive(Clone)]
pub struct StateInstrDefault<'a> {
    instr: &'a InstrDefault,
    pub num: usize,
    /// Output frequency
    rate: f32,
    period_helper: PeriodHelper,
    /// Sample state
    pub state_sample: Option<StateSample<'a>>,
    /// Vibrato state
    pub state_vibrato: StateAutoVibrato<'a>,
    /// Volume Envelope state
    pub envelope_volume: StateEnvelope<'a>,
    /// Panning Envelope state
    pub envelope_panning: StateEnvelope<'a>,

    // Volume sustained?
    pub sustained: bool,
    /// Volume fadeout value
    pub volume_fadeout: f32,

    /// Current volume
    pub volume: f32,
    /// Original Sample volume
    volume_orig: f32,

    /// Current panning
    pub panning: f32,
}

impl<'a> StateInstrDefault<'a> {
    pub fn new(
        instr: &'a InstrDefault,
        num: usize,
        period_helper: PeriodHelper,
        rate: f32,
    ) -> Self {
        let v = &instr.vibrato;
        let ve = &instr.volume_envelope;
        let pe = &instr.pan_envelope;
        Self {
            instr,
            num,
            rate,
            period_helper: period_helper.clone(),
            state_sample: None,
            state_vibrato: StateAutoVibrato::new(v, period_helper),
            envelope_volume: StateEnvelope::new(ve, 1.0),
            envelope_panning: StateEnvelope::new(pe, 0.5),
            sustained: true,
            volume_fadeout: 1.0,
            volume: 1.0,
            volume_orig: 1.0,
            panning: 0.5,
        }
    }

    pub fn has_volume_envelope(&self) -> bool {
        self.envelope_volume.has_volume_envelope()
    }

    pub fn replace_instr(&mut self, instr: &'a InstrDefault) {
        self.instr = instr;
    }

    pub fn is_enabled(&self) -> bool {
        match &self.state_sample {
            Some(s) => s.is_enabled(),
            None => false,
        }
    }

    pub fn sample_reset(&mut self) {
        match &mut self.state_sample {
            Some(s) => s.reset(),
            None => {}
        }
    }

    pub fn envelopes_reset(&mut self) {
        self.sustained = true;
        self.volume_fadeout = 1.0;
        self.envelope_volume.reset();
        self.envelope_panning.reset();
    }

    pub fn volume_reset(&mut self) {
        self.volume = self.volume_orig;
        self.volume_fadeout = 1.0;
        self.sustained = true;
    }

    pub fn vibrato_reset(&mut self) {
        self.state_vibrato.reset();
    }

    pub fn cut_pitch(&mut self) {
        self.volume = 0.0;
    }

    pub fn key_off(&mut self) {
        /* Key Off */
        self.sustained = false;

        if let Some(ss) = &mut self.state_sample {
            ss.set_sustained(false);
        }

        if !self.envelope_volume.has_volume_envelope() {
            if self.instr.volume_fadeout == 0.0 {
                self.cut_pitch();
            }
        }
    }

    pub fn get_volume(&self) -> f32 {
        self.volume_fadeout * self.envelope_volume.value * self.volume
    }

    fn envelopes(&mut self) {
        // Volume
        if !self.sustained {
            self.volume_fadeout = (self.volume_fadeout - self.instr.volume_fadeout).max(0.0);
        }
        if self.instr.volume_envelope.enabled {
            self.envelope_volume.tick(self.sustained);
        }
        // Panning
        if self.instr.pan_envelope.enabled {
            self.envelope_panning.tick(self.sustained);
        }
    }

    pub fn get_finetuned_pitch(&self) -> f32 {
        match &self.state_sample {
            Some(s) if s.is_enabled() => s.get_finetuned_pitch(),
            _ => 0.0,
        }
    }

    pub fn set_finetune(&mut self, finetune: f32) {
        if let Some(s) = &mut self.state_sample {
            if s.is_enabled() {
                s.set_finetune(finetune);
            }
        }
    }

    pub fn update_frequency(&mut self, period: f32, arp_pitch: f32, finetune: f32, semitone: bool) {
        if let Some(s) = &mut self.state_sample {
            let f = self.period_helper.all_to_frequency_cached(
                period,
                arp_pitch,
                finetune + self.state_vibrato.current_modulation,
                semitone,
            );
            s.set_step(f);
        }
    }

    pub fn set_pitch(&mut self, note: Pitch) -> bool {
        if note.is_valid() {
            if let Some(num) = self.instr.sample_for_pitch[note.value() as usize] {
                return self.select_sample(num);
            }
        }
        return false;
    }

    fn select_sample(&mut self, num: usize) -> bool {
        if num < self.instr.sample.len() {
            if let Some(sample) = &self.instr.sample[num] {
                let state_sample = StateSample::new(sample, self.rate);
                self.panning = state_sample.get_panning();
                self.volume = state_sample.get_volume();
                self.volume_orig = self.volume;
                self.state_sample = Some(state_sample);
                return true;
            }
        }
        self.state_sample = None;
        self.panning = 0.5;
        self.volume = 0.0;
        return false;
    }

    pub fn tick(&mut self) {
        self.envelopes();
        self.state_vibrato.tick(self.sustained);
    }
}

impl<'a> Iterator for StateInstrDefault<'a> {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_enabled() {
            match &mut self.state_sample {
                Some(s) => s.next(),
                None => None,
            }
        } else {
            None
        }
    }
}
