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
        // `instr.global_volume` is IT's per-instrument scalar (0..1,
        // populated from the IT instrument header's global-volume
        // byte / 128). For formats without the concept (XM / S3M)
        // it stays at 1.0, making this a no-op. Previously ignored —
        // which threw off any IT module where the author balanced
        // instruments via the per-instrument GV (common on drums vs
        // leads, or for pad voices dialled down to sit under the
        // melody).
        self.volume_fadeout * self.envelope_volume.value * self.volume * self.instr.global_volume
    }

    /// The current sample's `default_note_volume` — IT's per-sample
    /// Vol register normalised to 0..1. For non-IT formats this is
    /// always 1.0 (identity). Returns 1.0 when no sample is currently
    /// selected.
    ///
    /// Used by the channel-level trigger path to apply Vol at note-
    /// on time on top of the always-applied `Sample.volume` (GvL).
    /// A V-column override later in effect processing replaces the
    /// resulting `self.volume` wholesale, cleanly superseding Vol
    /// without also discarding the GvL scaling.
    pub fn current_sample_default_note_volume(&self) -> f32 {
        self.current_sample_num
            .and_then(|n| self.instr.sample.get(n))
            .and_then(|s| s.as_ref())
            .map(|s| s.default_note_volume)
            .unwrap_or(1.0)
    }

    /// Instrument-level random volume variation (IT humanisation).
    /// IT stores this as 0..100 percent of the starting volume; the
    /// importer normalises to 0..1. Zero for non-IT formats.
    pub fn random_volume_variation(&self) -> f32 {
        self.instr.random_volume_variation
    }

    /// Instrument-level random pan variation (IT humanisation).
    /// Same range / normalisation as `random_volume_variation`.
    pub fn random_pan_variation(&self) -> f32 {
        self.instr.random_pan_variation
    }

    /// Instrument-level pitch-pan separation (IT). Positive values
    /// spread the pan as the played note moves away from
    /// `pitch_pan_center`. The importer normalises IT's signed
    /// -32..+32 byte to -1..+1. Zero for non-IT formats.
    pub fn pitch_pan_separation(&self) -> f32 {
        self.instr.pitch_pan_separation
    }

    /// The reference note for `pitch_pan_separation`, returned as a
    /// semitone index (0 = C0). Non-IT: `Pitch::C4` (= 48).
    pub fn pitch_pan_center_semitones(&self) -> i32 {
        self.instr.pitch_pan_center as i32
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
            // Schism's reference implementation
            // (`sndmix.c::rn_pitch_filter_envelope` +
            // `filters.c::setup_channel_filter`) treats the pitch
            // envelope as a *modulator* of the cutoff posed by the
            // instrument header, not a substitute:
            //
            //   modifier  = (env_value_signed - 32) * 8     // -256..+256
            //   cutoff    = base_cutoff * (modifier + 256) / 256
            //
            // In xmrs, `envelope_pitch.value` arrives already
            // normalised to 0..1 by `to_envelope_points_signed`,
            // with 0.5 corresponding to the IT-signed 0 (neutral).
            // Substituting:
            //
            //   modifier   = (value - 0.5) * 512
            //   new_cutoff = base * (modifier + 256) / 256
            //              = base * (value * 512) / 256
            //              = base * value * 2
            //
            // Cases:
            //   value = 0.0 → cutoff = 0       (filter fully closed)
            //   value = 0.5 → cutoff = base    (no change)
            //   value = 1.0 → cutoff = 2*base  (clamped at 127)
            //
            // The base cutoff comes from the instrument header byte
            // — the bottom 7 bits of `initial_filter_cutoff` (bit 7
            // is the enable flag and is consumed elsewhere). xmrs's
            // cutoff register is clamped to 0..=127 by
            // `set_cutoff_reg`, so the modulated value clamps there
            // even though schism allows 0..255. That's a property
            // of xmrs's filter coefficient table, unrelated to the
            // modulation logic.
            //
            // The previous code substituted `value * 127` as the
            // cutoff outright, which: (a) ignored the instrument's
            // base cutoff entirely, and (b) silenced any sample at
            // envelope-value 0.5 (true neutral) by setting cutoff
            // to ~63 regardless of intent.
            if self.instr.pitch_envelope_as_low_pass_filter {
                let base = (self.instr.initial_filter_cutoff & 0x7F) as f32;
                let v = self.envelope_pitch.value.clamp(0.0, 1.0);
                let modulated = (base * v * 2.0).min(127.0) as u8;
                self.filter.set_cutoff_reg(modulated);
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

    /// Resolve the *output* pitch for a given input note, applying
    /// the instrument's keyboard remap if any.
    ///
    /// IT drum-kit instruments wire each input key to an absolute
    /// output note via `note_for_pitch`. When the entry is `Some`,
    /// the sample is played at that output pitch regardless of which
    /// key the user pressed. When it's `None` (the identity case),
    /// or for any other format that doesn't populate the array, the
    /// played pitch is the input note itself.
    ///
    /// Used by the channel at trigger time to compute the frequency
    /// the sample should play at, while the input note is preserved
    /// for DCT / NNA / portamento-target comparisons.
    /// Resolves the IT keyboard-table remap for a given input pitch.
    /// When the instrument's drum-kit table maps `input` to a
    /// different output note, returns that output. Otherwise returns
    /// `input` unchanged.
    ///
    /// Used by the channel at trigger time to compute the frequency
    /// the sample should play at, while the input note is preserved
    /// for DCT / NNA / portamento-target comparisons.
    pub fn played_pitch_for(&self, input: Pitch) -> Pitch {
        match self.instr.note_for_pitch[input.value() as usize] {
            Some(out) => Pitch::try_from(out).unwrap_or(input),
            None => input,
        }
    }

    pub fn set_pitch(&mut self, note: Pitch) -> bool {
        if let Some(num) = self.instr.sample_for_pitch[note.value() as usize] {
            return self.select_sample(num);
        }
        false
    }

    fn select_sample(&mut self, num: usize) -> bool {
        if num < self.instr.sample.len() {
            if let Some(sample) = &self.instr.sample[num] {
                let state_sample = StateSample::new(sample, self.rate);
                // IT's pan cascade: sample override → instrument
                // default_pan → channel initial pan. The IT
                // importer stores exactly `0.5` in Sample.panning
                // when the sample's own bit-7-"use-pan" flag is
                // clear (i.e., "not set, defer to instrument"). We
                // can't distinguish that from an author's explicit
                // centre-pan, but treating exact-0.5 as "unset" is
                // the right heuristic — authors rarely place a
                // deliberate centre override, and the fallback to
                // `instr.default_pan` is what the module expects.
                let sample_pan = state_sample.get_panning();
                self.panning = if sample_pan == 0.5 {
                    self.instr.default_pan
                } else {
                    sample_pan
                };
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

#[cfg(test)]
mod tests {
    use super::*;
    use xmrs::prelude::*;

    fn make_instr_with_remap(remap: &[(usize, u8)]) -> InstrDefault {
        let mut instr = InstrDefault::default();
        for (input, output) in remap {
            instr.note_for_pitch[*input] = Some(*output);
        }
        instr
    }

    fn state_for(instr: &InstrDefault) -> StateInstrDefault<'_> {
        // Use linear-frequency period helper; played_pitch_for
        // doesn't actually consult the period helper, but
        // StateInstrDefault::new requires one.
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        StateInstrDefault::new(instr, 0, ph, 44100.0)
    }

    #[test]
    fn played_pitch_identity_when_no_remap() {
        // XM/MOD/S3M-style instruments: every note_for_pitch entry
        // is None. played_pitch_for must echo the input untouched.
        let instr = InstrDefault::default();
        let state = state_for(&instr);
        for raw in 0u8..=119 {
            if let Ok(p) = Pitch::try_from(raw) {
                assert_eq!(state.played_pitch_for(p), p);
            }
        }
    }

    #[test]
    fn played_pitch_applies_remap() {
        // Drum-kit style: input D-5 (62) is wired to output C-5
        // (60, transposed down 2 semitones); input E-5 (64) is
        // wired to output G-5 (67).
        let instr = make_instr_with_remap(&[(62, 60), (64, 67)]);
        let state = state_for(&instr);

        assert_eq!(
            state.played_pitch_for(Pitch::D5),
            Pitch::C5,
            "input D-5 transposes to output C-5"
        );
        assert_eq!(
            state.played_pitch_for(Pitch::E5),
            Pitch::G5,
            "input E-5 transposes to output G-5"
        );
        // Untouched keys: identity.
        assert_eq!(state.played_pitch_for(Pitch::C5), Pitch::C5);
        assert_eq!(state.played_pitch_for(Pitch::F5), Pitch::F5);
    }
}
