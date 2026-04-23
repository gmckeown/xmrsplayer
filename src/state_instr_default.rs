// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

use core::ops::Deref;

use crate::{
    state_auto_vibrato::StateAutoVibrato, state_envelope::StateEnvelope, state_filter::StateFilter,
    state_sample::StateSample,
};
use xmrs::prelude::*;

impl<'a> Deref for StateInstrDefault<'a> {
    type Target = InstrDefault;
    fn deref(&self) -> &InstrDefault {
        self.instr
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
    /// Index into `instr.sample` for the currently-selected sample.
    /// `None` when no sample is loaded. Parallels `state_sample`'s
    /// lifecycle but carries the numeric id; DCT::Sample matching
    /// reads this without having to cross-reference the `&Sample`
    /// pointer back to a slot index.
    pub current_sample_num: Option<usize>,
    /// Vibrato state
    pub state_vibrato: StateAutoVibrato<'a>,
    /// Volume Envelope state
    pub envelope_volume: StateEnvelope<'a>,
    /// Panning Envelope state
    pub envelope_panning: StateEnvelope<'a>,
    /// Pitch Envelope state. IT-specific: each envelope value equates
    /// to half a semitone (ITTECH), applied additively to the voice's
    /// current pitch. Default value 0.5 = centre (no offset) because
    /// the IT importer normalises the signed -32..+32 node magnitudes
    /// into 0..1 via `to_envelope_struct_signed`, with 0.5 = 0-offset.
    /// When `instr.pitch_envelope_as_low_pass_filter` is set (IT's
    /// "use pitch envelope as filter" flag), the same envelope state
    /// is read by the filter engine instead — that path lands with
    /// the filter work in IT_ROADMAP §4.
    pub envelope_pitch: StateEnvelope<'a>,

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

    /// Per-voice resonant low-pass filter. Engaged at note-trigger
    /// time from the instrument's `initial_filter_cutoff` /
    /// `initial_filter_resonance` pair (each register's bit 7 is
    /// the enable flag); otherwise remains in pass-through. Applied
    /// on every output sample in `Iterator::next`.
    pub(crate) filter: StateFilter,
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
        let pie = &instr.pitch_envelope;
        let mut filter = StateFilter::new(rate);
        filter.configure_from_it_registers(
            instr.initial_filter_cutoff,
            instr.initial_filter_resonance,
        );
        Self {
            instr,
            num,
            rate,
            period_helper: period_helper.clone(),
            state_sample: None,
            current_sample_num: None,
            state_vibrato: StateAutoVibrato::new(v, period_helper),
            envelope_volume: StateEnvelope::new(ve, 1.0),
            envelope_panning: StateEnvelope::new(pe, 0.5),
            envelope_pitch: StateEnvelope::new(pie, 0.5),
            sustained: true,
            volume_fadeout: 1.0,
            volume: 1.0,
            volume_orig: 1.0,
            panning: 0.5,
            filter,
        }
    }

    pub fn has_volume_envelope(&self) -> bool {
        self.envelope_volume.has_volume_envelope()
    }

    pub fn replace_instr(&mut self, instr: &'a InstrDefault) {
        self.instr = instr;
        // Filter registers live on the instrument header; reapply
        // them so a `replace_instr` (ghost-instrument path) picks
        // up the new sample's filter settings too.
        self.filter.configure_from_it_registers(
            instr.initial_filter_cutoff,
            instr.initial_filter_resonance,
        );
    }

    pub fn is_enabled(&self) -> bool {
        match &self.state_sample {
            Some(s) => s.is_enabled(),
            None => false,
        }
    }

    pub fn sample_reset(&mut self) {
        if let Some(s) = &mut self.state_sample {
            s.reset()
        }
        // Zero the filter's delay line alongside the sample cursor
        // so a fresh trigger doesn't hear the tail of the previous
        // note's filter ring-down as a click.
        self.filter.reset_history();
    }

    pub fn envelopes_reset(&mut self) {
        self.sustained = true;
        self.volume_fadeout = 1.0;
        self.envelope_volume.reset();
        self.envelope_panning.reset();
        self.envelope_pitch.reset();
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

        if !self.envelope_volume.has_volume_envelope() && self.instr.volume_fadeout == 0.0 {
            self.cut_pitch();
        }
    }

    pub fn get_volume(&self) -> f32 {
        self.volume_fadeout * self.envelope_volume.value * self.volume
    }

    /// Current pitch-envelope contribution, in semitones.
    ///
    /// The xmrs signed-normalised envelope stores the IT node
    /// magnitude (range −32..+32 half-semitones) as a 0..1 float
    /// with 0.5 = centre (no pitch change). Each envelope unit
    /// represents half a semitone per ITTECH, so the full range is
    /// ±32·½ = ±16 semitones. Mapping: offset = (value − 0.5) × 32.
    ///
    /// Returns 0 when either the envelope is disabled OR the
    /// instrument's "pitch envelope as low-pass filter" flag is
    /// set — in the latter case the envelope drives the filter
    /// cutoff rather than pitch, and the filter engine reads the
    /// envelope value directly (Phase 4 of IT_ROADMAP).
    pub fn get_pitch_envelope_offset_semitones(&self) -> f32 {
        if !self.instr.pitch_envelope.enabled
            || !self.envelope_pitch.enabled
            || self.instr.pitch_envelope_as_low_pass_filter
        {
            return 0.0;
        }
        (self.envelope_pitch.value - 0.5) * 32.0
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
        // Pitch (also drives the low-pass filter when flagged —
        // see `get_pitch_envelope_offset_semitones`)
        if self.instr.pitch_envelope.enabled {
            self.envelope_pitch.tick(self.sustained);

            // IT "pitch envelope as low-pass filter" routing.
            // When the instrument flag is set, the pitch envelope's
            // current value drives the filter cutoff instead of
            // modulating pitch. `get_pitch_envelope_offset_
            // semitones` already returns 0 in that mode so pitch
            // isn't double-modulated; here we complete the routing
            // by pushing the envelope value through to the filter.
            //
            // Value mapping: the envelope's 0..1 range (0.5 = no
            // offset) is remapped linearly to 0..127 cutoff-register
            // units. The instrument's static cutoff setting (set
            // via the header bytes at trigger time) is overwritten
            // on every tick the envelope is alive — matching the
            // intent of the flag, which is "let the envelope run
            // the filter".
            if self.instr.pitch_envelope_as_low_pass_filter {
                let cutoff = (self.envelope_pitch.value.clamp(0.0, 1.0) * 127.0) as u8;
                self.filter.set_cutoff_reg(cutoff);
            }
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
        // Compute the pitch-envelope contribution BEFORE the `&mut
        // self.state_sample` borrow below — `get_pitch_envelope_
        // offset_semitones` reads `self.envelope_pitch` /
        // `self.instr` immutably, which overlaps the later mutable
        // sample borrow otherwise. `pitch_env_offset` is in semitones,
        // independent of vibrato finetune, and goes into the
        // `arp_pitch` slot (also in semitones).
        let pitch_env_offset = self.get_pitch_envelope_offset_semitones();
        let vibrato_mod = self.state_vibrato.current_modulation;
        if let Some(s) = &mut self.state_sample {
            let f = self.period_helper.all_to_frequency_cached(
                period,
                arp_pitch + pitch_env_offset,
                finetune + vibrato_mod,
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
        false
    }

    fn select_sample(&mut self, num: usize) -> bool {
        if num < self.instr.sample.len() {
            if let Some(sample) = &self.instr.sample[num] {
                let state_sample = StateSample::new(sample, self.rate);
                self.panning = state_sample.get_panning();
                self.volume = state_sample.get_volume();
                self.volume_orig = self.volume;
                self.state_sample = Some(state_sample);
                self.current_sample_num = Some(num);
                return true;
            }
        }
        self.state_sample = None;
        self.current_sample_num = None;
        self.panning = 0.5;
        self.volume = 0.0;
        false
    }

    pub fn tick(&mut self) {
        self.envelopes();
        self.state_vibrato.tick(self.sustained);
    }
}

impl<'a> Iterator for StateInstrDefault<'a> {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<Self::Item> {
        if !self.is_enabled() {
            return None;
        }
        let raw = match &mut self.state_sample {
            Some(s) => s.next(),
            None => None,
        };
        // Route through the per-voice filter. When the filter's
        // `enabled` flag is clear (XM / MOD / S3M and IT samples
        // without either register's bit-7 set), this is a no-op
        // pass-through — checked once at the top of `process`.
        raw.map(|(l, r)| self.filter.process(l, r))
    }
}
