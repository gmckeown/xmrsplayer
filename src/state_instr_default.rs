use core::ops::Deref;

use crate::{
    state_auto_vibrato::StateAutoVibrato, state_envelope::StateEnvelope, state_filter::StateFilter,
    state_sample::StateSample,
};
use xmrs::core::cell::NoteExpression;
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{Amp, EnvValue, Finetune, Panning, PitchDelta, SampleRate, Volume};
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
    /// Output sample-rate, as integer Hz ([`SampleRate`]).
    /// Forwarded to [`StateSample::set_step`] (which divides
    /// frequency by it for the sample-clock step) and used by
    /// [`StateFilter`] for biquad coefficient sizing. The whole
    /// chain runs without `f32` from this point on.
    rate: SampleRate,
    period_helper: PeriodHelper,
    /// Sample state
    pub state_sample: Option<StateSample<'a>>,
    /// Sample interpolation mode
    pub interpolation: crate::interpolation::Interpolation,
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
    /// When `instr.voice.pitch_envelope_as_low_pass_filter` is set (IT's
    /// "use pitch envelope as filter" flag), the same envelope state
    /// is read by the filter engine instead.
    pub envelope_pitch: StateEnvelope<'a>,

    // Volume sustained?
    pub sustained: bool,
    /// Whether the voice is in its "fadeout" phase (schism's
    /// `CHN_NOTEFADE`). Independent from [`Self::sustained`] because IT
    /// distinguishes two stages after a key-off:
    ///   1. release — sustain is gone but the envelope is still
    ///      walking through its release section. Fadeout has NOT
    ///      started yet; the voice plays at whatever level the
    ///      envelope dictates.
    ///   2. fadeout — either the envelope reached its last point
    ///      with no loop to wrap it back, or the instrument has no
    ///      envelope at all, or NNA = NoteFade was applied. The
    ///      `volume_fadeout` register starts decaying.
    ///
    /// Set by [`Self::envelopes`] when the volume envelope walks
    /// past its last point, by [`Self::key_off`] in the no-envelope
    /// / loop+fadeout branches (mirroring schism's `fx_key_off`),
    /// and by [`Self::start_fadeout`] for NNA NoteFade. Cleared by
    /// [`Self::envelopes_reset`] / [`Self::volume_reset`] on a fresh
    /// trigger.
    pub volume_fading_out: bool,
    /// Volume fadeout register, Q1.15 in `[0, 1]`. Was `f32`.
    /// Decremented by `voice.volume_fadeout` per tick once
    /// `volume_fading_out` is true; held at `Volume::FULL`
    /// otherwise.
    pub volume_fadeout: Volume,

    /// Current voice volume, Q1.15 in `[0, 1]`. Was `f32`.
    /// Initialised at note-trigger time from the sample's
    /// channel volume (see `volume_orig`); modulated
    /// thereafter by Vxx, tremolo, retrigger, etc.
    pub volume: Volume,
    /// Snapshot of [`Self::volume`] at the most recent
    /// note-trigger, used by `volume_reset`.
    volume_orig: Volume,

    /// Whether `Sample.volume` keeps acting as a static gain in
    /// `get_volume` (IT semantics — "GvL"), or whether it has
    /// already been consumed by the channel at trigger time
    /// (MOD/XM/S3M). Mirrors
    /// `PlaybackQuirks::sample_volume_is_static_gain` from
    /// the owning `Module` and is set by the constructor caller.
    sample_volume_is_static_gain: bool,

    /// Current panning, Q1.15 in `[0, 1]`. Was `f32`.
    pub panning: Panning,

    /// Per-voice resonant low-pass filter. Engaged at note-trigger
    /// time from the instrument's `initial_filter_cutoff` /
    /// `initial_filter_resonance` pair (each register's bit 7 is
    /// the enable flag); otherwise remains in pass-through. Applied
    /// on every output sample in `Iterator::next`.
    pub(crate) filter: StateFilter,

    /// RFC §2C — this voice's per-note expression (MPE), snapshotted
    /// from the triggering cell. Folded into the voice's own pitch
    /// (`update_frequency`) and volume / pan (`apply_voice_mod`) so two
    /// voices on the same channel (a held ghost + a fresh note) carry
    /// **independent** modulation. [`NoteExpression::NEUTRAL`] for every
    /// tracker note ⇒ the fold is a measured no-op ⇒ bit-identical.
    pub expression: NoteExpression,
}

impl<'a> StateInstrDefault<'a> {
    pub fn new(
        instr: &'a InstrDefault,
        num: usize,
        period_helper: PeriodHelper,
        rate: SampleRate,
        sample_volume_is_static_gain: bool,
    ) -> Self {
        let v = &instr.voice.vibrato;
        let ve = &instr.voice.volume_envelope;
        let pe = &instr.voice.pan_envelope;
        let pie = &instr.voice.pitch_envelope;
        // Q-format filter — takes integer Hz directly.
        let mut filter = StateFilter::new(rate.hz());
        filter.configure_from_it_registers(
            instr.voice.initial_filter_cutoff,
            instr.voice.initial_filter_resonance,
        );
        Self {
            instr,
            num,
            rate,
            interpolation: crate::interpolation::Interpolation::default(),
            period_helper: period_helper.clone(),
            state_sample: None,
            current_sample_num: None,
            state_vibrato: StateAutoVibrato::new(v, period_helper),
            envelope_volume: StateEnvelope::new(ve, EnvValue::VOLUME_DEFAULT),
            envelope_panning: StateEnvelope::new(pe, EnvValue::PAN_DEFAULT),
            envelope_pitch: StateEnvelope::new(pie, EnvValue::PAN_DEFAULT),
            sustained: true,
            volume_fading_out: false,
            volume_fadeout: Volume::FULL,
            volume: Volume::FULL,
            volume_orig: Volume::FULL,
            sample_volume_is_static_gain,
            panning: Panning::CENTER,
            filter,
            expression: NoteExpression::NEUTRAL,
        }
    }

    pub fn has_volume_envelope(&self) -> bool {
        self.envelope_volume.has_volume_envelope()
    }

    pub fn set_interpolation(&mut self, interpolation: crate::interpolation::Interpolation) {
        self.interpolation = interpolation;
        if let Some(s) = &mut self.state_sample {
            s.interpolation = interpolation;
        }
    }

    pub fn replace_instr(&mut self, instr: &'a InstrDefault) {
        self.instr = instr;
        // Filter registers live on the instrument header; reapply
        // them so a `replace_instr` (ghost-instrument path) picks
        // up the new sample's filter settings too.
        self.filter.configure_from_it_registers(
            instr.voice.initial_filter_cutoff,
            instr.voice.initial_filter_resonance,
        );
    }

    #[inline(always)]
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
        self.volume_fading_out = false;
        self.volume_fadeout = Volume::FULL;
        self.envelope_volume.reset();
        self.envelope_panning.reset();
        self.envelope_pitch.reset();
    }

    pub fn volume_reset(&mut self) {
        self.volume = self.volume_orig;
        self.volume_fadeout = Volume::FULL;
        self.volume_fading_out = false;
        self.sustained = true;
    }

    pub fn vibrato_reset(&mut self) {
        self.state_vibrato.retrig();
    }

    pub fn cut_pitch(&mut self) {
        self.volume = Volume::SILENT;
    }

    pub fn key_off(&mut self) {
        /* Key Off — the user's "===" cell, or NNA NoteOff. Releases
        sustain (the envelope's sustain loop stops wrapping; the
        release section starts walking through). Whether fadeout
        ALSO starts now or waits for the envelope to finish its
        release is decided here, mirroring schism's `fx_key_off`
        (`effects.c:140-167`). */
        self.sustained = false;

        if let Some(ss) = &mut self.state_sample {
            ss.set_sustained(false);
        }

        let has_env = self.envelope_volume.has_volume_envelope();
        let env = &self.instr.voice.volume_envelope;

        if !has_env {
            // No volume envelope: schism takes the alternate render
            // path (`sndmix.c:1158-1164`) where CHN_NOTEFADE is the
            // sole control on whether the voice keeps playing. To
            // get the same behaviour we engage fadeout immediately.
            // Schism's `fx_key_off` line 142-144 sets CHN_NOTEFADE
            // for the same reason in the same case.
            self.volume_fading_out = true;
            if self.instr.voice.volume_fadeout == Volume::SILENT {
                // No envelope AND no fadeout amount: nothing keeps
                // the voice alive past key-off. Cut the pitch (silence
                // the sample) and force the fadeout register to zero
                // so `Voice::is_alive()` reports the voice as dead on
                // the next pool sweep.
                self.cut_pitch();
                self.volume_fadeout = Volume::SILENT;
            }
        } else if env.loop_enabled && self.instr.voice.volume_fadeout != Volume::SILENT {
            // Looped envelope with non-zero fadeout: schism's
            // `fx_key_off` line 166-167 also engages CHN_NOTEFADE
            // here. Without this trigger the looped envelope would
            // hold the voice forever after key-off; with it, fadeout
            // runs in parallel and the voice eventually dies.
            //
            // FT2/XM modules reach this branch by construction —
            // the XM importer emulates schism's `xm.c:703-712`
            // trick (loop pinned to last node, ENV_VOLLOOP forced
            // on) so any XM voice with a volume envelope takes this
            // path on key-off, matching FT2's "fadeout starts
            // immediately, in parallel with the release section"
            // behaviour.
            self.volume_fading_out = true;
        }
        // else: envelope has sustain and a real release section but
        // no loop. This is the IT-typical pad / lead shape. We
        // deliberately leave `volume_fading_out` unchanged; the
        // envelope tick in `envelopes()` will set it the moment the
        // counter walks past the last point. That's IT's "release
        // section plays out, THEN fade" semantics.
        //
        // The previous unconditional `volume_fadeout` decrement on
        // every `!sustained` tick — i.e. as soon as key-off fired —
        // is what made any IT pad/sustain "stop too early": the
        // fade was running while the release section was still
        // playing, so the voice went silent before the envelope
        // finished its decay.
    }

    /// Engage the fadeout register without releasing sustain.
    /// Mirrors schism's NNA_NOTEFADE branch (`effects.c:1791-1792`):
    /// the voice keeps wrapping in its sustain loop while the
    /// fadeout register decays it to silence. Distinct from
    /// [`Self::key_off`] (NNA_NOTEOFF), which releases sustain so
    /// the envelope walks through its release section.
    pub fn start_fadeout(&mut self) {
        self.volume_fading_out = true;
    }

    #[inline(always)]
    pub fn get_volume(&self) -> Volume {
        // Pure Q1.15 gain chain. Each multiplication is a
        // saturating Q15 mul (`Volume::scaled_by`); no float
        // anywhere.
        //
        //   IT:        final = fadeout × env × per_instrument × per_voice(GvL)
        //   MOD/XM/S3M: final = fadeout × env × per_instrument
        //
        // `EnvValue::applied_to` returns a `Volume` (the
        // envelope acts as a gain multiplier on the fadeout).
        // From there it's straight `Volume × Volume → Volume`.
        // `instr.voice.volume` (per-instrument scalar) is IT's
        // global-volume byte — full unity for non-IT formats.
        //
        // For IT, `self.volume` carries `Sample.volume` (GvL) as
        // a static gain that keeps scaling the voice on every
        // tick. For MOD/XM/S3M, `Sample.volume` is consumed at
        // trigger time as the channel's initial volume (see
        // `Channel::trigger_pitch`) and `self.volume` here would
        // be the same `Sample.volume` again — applying it would
        // square it. The `sample_volume_is_static_gain` flag,
        // forwarded from the module's
        // `PlaybackQuirks::sample_volume_is_static_gain`, selects
        // which path runs.
        let v = self
            .envelope_volume
            .value
            .applied_to(self.volume_fadeout)
            .scaled_by(self.instr.voice.volume);

        if self.sample_volume_is_static_gain {
            v.scaled_by(self.volume)
        } else {
            v
        }
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
    pub fn current_sample_default_note_volume(&self) -> Volume {
        self.current_sample_num
            .and_then(|n| self.instr.sample.get(n))
            .and_then(|s| s.as_ref())
            .map(|s| s.default_note_volume)
            .unwrap_or(Volume::FULL)
    }

    /// Instrument-level random volume variation (IT humanisation).
    /// IT stores this as 0..100 percent of the starting volume; the
    /// importer normalises to 0..1. Zero for non-IT formats.
    pub fn random_volume_variation(&self) -> Q15 {
        self.instr.voice.random_volume_variation
    }

    /// Instrument-level random pan variation (IT humanisation).
    /// Same range / normalisation as `random_volume_variation`.
    pub fn random_pan_variation(&self) -> Q15 {
        self.instr.voice.random_pan_variation
    }

    /// Instrument-level pitch-pan separation (IT). Positive values
    /// spread the pan as the played note moves away from
    /// `pitch_pan_center`. The importer normalises IT's signed
    /// -32..+32 byte to -1..+1. Zero for non-IT formats.
    pub fn pitch_pan_separation(&self) -> Q15 {
        self.instr.voice.pitch_pan_separation
    }

    /// The reference note for `pitch_pan_separation`, returned as a
    /// semitone index (0 = C0). Non-IT: `Pitch::C4` (= 48).
    pub fn pitch_pan_center_semitones(&self) -> i32 {
        self.instr.voice.pitch_pan_center as i32
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
    /// envelope value directly.
    /// Pitch envelope contribution as a [`PitchDelta`] (Q8.8
    /// semitones). Q-format throughout — no f32 round trip.
    ///
    /// Mapping: `EnvValue` is Q1.15 in `[0, 1]` with `0.5` as
    /// the neutral point. Offset in semitones is `(v - 0.5) ×
    /// 32`. In raw bits: `(v_q15 - 16384) × 32 / 128 =
    /// (v_q15 - 16384) / 4` lands in Q8.8 raw directly.
    /// Range: `±16` semitones.
    pub fn get_pitch_envelope_offset(&self) -> PitchDelta {
        if !self.instr.voice.pitch_envelope.enabled
            || !self.envelope_pitch.enabled
            || self.instr.voice.pitch_envelope_as_low_pass_filter
        {
            return PitchDelta::ZERO;
        }
        // Pitch envelope value is `EnvValue` ∈ `[0, ONE]`; centre
        // it to a signed `[-HALF, +HALF]` Q1.15 swing, then narrow
        // to Q8.8 (the `/ 4` scales `±0.5` cycle to `±0.125 cycle`
        // = `±32 semitones`, the XM pitch-envelope range).
        let centred = self.envelope_pitch.value.raw_q15() - Q15::HALF;
        PitchDelta::from_q8_8_i32_sat(centred.raw() as i32 / 4)
    }

    fn envelopes(&mut self) {
        // Volume envelope tick. Run BEFORE the fadeout decrement so
        // a tick that walks the envelope past its last point can
        // arm `volume_fading_out` for this same tick — matches the
        // ordering schism uses in `sndmix.c:1156-1157`
        // (`rn_increment_env_pos` then `rn_process_envelope`, with
        // the fadeout register decay happening immediately after as
        // part of the same processing pass).
        if self.instr.voice.volume_envelope.enabled {
            self.envelope_volume.tick(self.sustained);

            // Schism's `_process_envelope` (sndmix.c:493-499) sets
            // the volume envelope's `fade_flag` (i.e. CHN_NOTEFADE)
            // exactly when the counter has walked past the last
            // point. By construction the counter only gets there
            // when neither sustain hold nor a real loop has
            // wrapped it back — i.e. we're in the "release section
            // ran out" state. `is_past_last_point` captures that
            // condition directly.
            if self.envelope_volume.is_past_last_point() {
                self.volume_fading_out = true;
            }
        }

        // Fadeout decrement. Gated on `volume_fading_out`, NOT on
        // `!sustained`. The two diverge in the IT-typical case
        // where sustain has been released but the envelope is
        // still walking through its release section: at that point
        // the voice should play at envelope-controlled volume, not
        // get attenuated by a parallel fadeout. The fadeout only
        // kicks in once the envelope itself runs out — set above —
        // or when `key_off` / `start_fadeout` decided up-front
        // that the case warrants immediate fade.
        //
        // Schism's matching code is `rn_process_ins_fade`
        // (sndmix.c:326-345): only runs while CHN_NOTEFADE is set.
        if self.volume_fading_out {
            // Volume::faded_by saturates at zero, replacing the
            // previous `(fadeout - step).max(0.0)` pattern.
            self.volume_fadeout = self
                .volume_fadeout
                .faded_by(self.instr.voice.volume_fadeout.raw());
        }

        // Panning
        if self.instr.voice.pan_envelope.enabled {
            self.envelope_panning.tick(self.sustained);
        }
        // Pitch (also drives the low-pass filter when flagged —
        // see `get_pitch_envelope_offset_semitones`)
        if self.instr.voice.pitch_envelope.enabled {
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
            //              = base * value * 2          (= schism register S, 0..255)
            //
            // Cases (schism register S):
            //   value = 0.0 → S = 0      (filter fully closed)
            //   value = 0.5 → S = base   (neutral)
            //   value = 1.0 → S = 2*base
            //
            // Two corrections vs. the original code:
            //
            // (1) **Base default.** Schism initialises every voice's
            //     cutoff to 0x7F and only overwrites it when the
            //     instrument's IFC *enable* bit (0x80) is set
            //     (`effects.c`: `if (penv->ifc & 0x80) chan->cutoff =
            //     penv->ifc & 0x7F`). A filter envelope with no
            //     explicit IFC therefore sweeps from a *fully open*
            //     127 base — not 0. The old code used `ifc & 0x7F`
            //     unconditionally, so an envelope-only filter (IFC=0,
            //     the common case — e.g. `xemogasa_-_azure.it`) was
            //     pinned shut at ~130 Hz, gutting the midrange.
            //
            // (2) **Register scale.** `base*value*2` is schism's 0..255
            //     register `S`. xmrs's cutoff LUT is the *half-scale*
            //     0..127 register (LUT exponent = reg/24 = S/48), and
            //     the static path already feeds it `base` (≡ S/2). The
            //     envelope path must match: feed `S/2 = base*value`,
            //     i.e. `>> 15` (not `>> 14`). The old `>> 14` doubled
            //     the exponent — too bright by an octave-plus and
            //     clipping at the 127 ceiling.
            if self.instr.voice.pitch_envelope_as_low_pass_filter {
                let ifc = self.instr.voice.initial_filter_cutoff;
                let base = if ifc & 0x80 != 0 {
                    (ifc & 0x7F) as i32
                } else {
                    0x7F
                };
                let env_q15 = self.envelope_pitch.value.as_q15_i32().max(0);
                // modulated = base * value  (= S/2, the xmrs-scale register)
                let modulated = ((base * env_q15) >> 15).min(127) as u8;
                self.filter.set_cutoff_reg(modulated);
            }
        }
    }

    pub fn get_finetuned_pitch(&self) -> PitchDelta {
        match &self.state_sample {
            Some(s) if s.is_enabled() => s.get_finetuned_pitch(),
            _ => PitchDelta::ZERO,
        }
    }

    pub fn set_finetune(&mut self, finetune: Finetune) {
        if let Some(s) = &mut self.state_sample {
            if s.is_enabled() {
                s.set_finetune(finetune);
            }
        }
    }

    /// Recompute the per-sample frequency from the channel's
    /// current period and the LFO / arpeggio / pitch-envelope
    /// contributions. Q-typed inputs:
    ///
    /// * `period` — current channel period.
    /// * `arp_pitch` — arpeggio offset (semitones).
    /// * `vibrato` — vibrato modulation from the channel's main
    ///   vibrato LFO (semitones).
    /// * `semitone` — XM glissando: round the base pitch to the
    ///   nearest integer semitone before adding modulation.
    ///
    /// Pitch envelope and instrument auto-vibrato are read from
    /// `self` directly (not parameters) — they're voice-internal
    /// state.
    ///
    /// Returns the **effective period** actually sounding this tick —
    /// the base `period` after every pitch modulation (arpeggio,
    /// vibrato LFO, pitch envelope, instrument auto-vibrato) has been
    /// folded in, i.e. the period that maps 1:1 to what Paula's
    /// `AUDxPER` would hold. The caller ([`crate::channel::Channel`])
    /// stashes it in `effective_period` so observers / the DW Tier-2
    /// oracle can read the *played* pitch, not just the nominal note
    /// (`Channel::period`, which stays the un-modulated base). Returns
    /// `None` when no sample is loaded (nothing is sounding).
    pub fn update_frequency(
        &mut self,
        period: xmrs::core::fixed::units::Period,
        arp_pitch: PitchDelta,
        vibrato_raw: i16,
        points_pitch: PitchDelta,
        semitone: bool,
    ) -> Option<xmrs::core::fixed::units::Period> {
        self.state_sample.as_ref()?;
        let pitch_env_offset = self.get_pitch_envelope_offset();
        let vibrato_mod = self.state_vibrato.current_modulation;

        let is_amiga = matches!(
            self.period_helper.freq_type,
            FrequencyType::AmigaFrequencies
        );

        // The EFFECT vibrato is applied differently per frequency mode,
        // matching FT2/Protracker `doVibrato` (`outPeriod = realPeriod ±
        // tmpVib`):
        //   * Amiga — a direct *period* delta added at the very end (the
        //     Amiga period is `∝ 2^-pitch`, so a constant period nudge is
        //     NOT a constant pitch nudge). Excluded from the semitone sum
        //     here and folded in below; the lane stores `depth` in period
        //     units. MOD, Amiga-XM and DW take this path.
        //   * Linear — reinterpreted as Q8.8 semitones and summed with the
        //     other deltas; on a pitch-affine linear period this is the
        //     same operation, so the result is byte-identical to the old
        //     `value_pitch_delta()` path.
        // Arpeggio, pitch envelope and instrument auto-vibrato stay in
        // semitone space in both modes (they are note/semitone effects in
        // FT2 too).
        let vibrato_semitone = if is_amiga {
            PitchDelta::ZERO
        } else {
            PitchDelta::from_q8_8_i16(vibrato_raw)
        };
        // `points_pitch` is the held transpose from a `TrackPitch`
        // `Points` lane (`AutomationValue::Pitch`) — an absolute,
        // time-varying semitone shift of the note's base (e.g. a
        // tracker global/channel transpose). It is summed like the
        // other semitone deltas but, like the pitch envelope, is NOT a
        // reason to engage the FT2 arpeggio LUT/clamp, so it stays out
        // of `arp_active`.
        // RFC §2C: this voice's own per-note pitch bend (MPE) joins the
        // semitone-space delta sum, just like the Points transpose. It
        // is `ZERO` for every tracker note ⇒ no change. Like the pitch
        // envelope it does NOT engage the FT2 arpeggio LUT/clamp, so it
        // stays out of `arp_active`.
        let delta_q88 = (arp_pitch
            + pitch_env_offset
            + vibrato_semitone
            + vibrato_mod
            + points_pitch
            + self.expression.pitch_bend)
            .as_q8_8_i32();
        let arp_active = (arp_pitch + pitch_env_offset) != PitchDelta::ZERO;

        // Period → base pitch (Q8.8 semitones). Both branches
        // are integer-only:
        //   - Linear: exact `(7680 - period) << 2` raw — `(1/64)`
        //     in Q8.8 raw is `256/64 = 4`.
        //   - Amiga: binary-search through the Amiga period
        //     table (1/8-semitone grid, Q8.8 raw step 32) — 12.5
        //     cent precision, same as FT2-era trackers.
        let base_pitch_q88 = match self.period_helper.freq_type {
            FrequencyType::LinearFrequencies => (7680_i32 - period.raw() as i32) * 4,
            FrequencyType::AmigaFrequencies => {
                xmrs::core::fixed::tables::amiga_period_to_pitch(period).as_q8_8_i32()
            }
        };
        let pitch_q = self.finalize_pitch(base_pitch_q88, delta_q88, arp_active, semitone);

        // Q-format end-to-end: Pitch → Period → Frequency → step.
        let mut period_q = self.period_helper.note_to_period(pitch_q);

        // Amiga period-space vibrato: add the LFO output straight to the
        // Paula period (FT2 `outPeriod = realPeriod ± tmpVib`). Saturates
        // at the `u16` period bounds, like FT2's period clamp.
        if is_amiga && vibrato_raw != 0 {
            period_q = period_q.saturating_add_signed(vibrato_raw);
        }

        let f = self.period_helper.period_to_frequency(period_q);

        if let Some(s) = &mut self.state_sample {
            s.set_step(f);
        }
        // `period_q` is the post-modulation period that produced the
        // step just set — hand it back so the channel can expose the
        // played pitch (see the doc above).
        Some(period_q)
    }

    /// RFC §2A — apply a channel's per-tick [`VoiceMod`] bus to **this**
    /// voice: combine the channel-side modulation with the voice's own
    /// envelope state to produce the final stereo gains and set the
    /// per-sample step. Returns `(actual_volume[L, R], effective_period)`.
    ///
    /// This is the verbatim relocation of the panning / volume /
    /// frequency arithmetic that lived inline in
    /// `Channel::tickn_update_instr` before 2A — same Q1.15 constants,
    /// same order. The only change is *who owns it*: the per-voice apply
    /// point that §2B re-points across every voice (live + ghosts, MPE).
    pub fn apply_voice_mod(
        &mut self,
        m: &crate::voice_mod::VoiceMod,
    ) -> ([Volume; 2], xmrs::core::fixed::units::Period) {
        // Panning chain in pure Q1.15 raw arithmetic (see the original
        // derivation in `tickn_update_instr`).
        // `HALF` is the canonical Q1.15 0.5 (`0x4000`). `ONE` is the
        // arithmetic unity *scale* `2^15`, deliberately **not**
        // `Q15::ONE` (`0x7FFF`): the `>> 15` math below needs the true
        // power-of-two so the `spc_q15` triangle reaches full scale.
        const HALF: i32 = Q15::HALF.raw() as i32;
        const ONE: i32 = 1 << 15;
        let pan_q15: i32 = m.panning.as_q15_i32();
        let dist = (pan_q15 - HALF).abs();
        let spc_q15: i32 = ONE - 2 * dist; // [0, 32768]
        let env_off_q15: i32 = self.envelope_panning.value.as_q15_i32() - HALF;
        let panbrello_q15: i32 = m.panbrello.raw() as i32;
        let env_contrib = (env_off_q15 * spc_q15) >> 15;
        let panbrello_contrib = (panbrello_q15 * spc_q15) >> 15;
        // RFC §2C: this voice's per-note pan offset (MPE), `ZERO` for
        // tracker notes. Added on top of the channel pan + panbrello.
        let panning = Panning::from_q15(Q15::from_i32_unsigned_sat(
            pan_q15 + env_contrib + panbrello_contrib,
        ))
        .shifted_by(self.expression.pan);

        // Volume chain. RFC §2C: the per-note volume delta (MPE
        // pressure) folds in like a second tremolo — `ZERO` for tracker
        // notes ⇒ unchanged.
        let volume_q = if !m.tremor {
            let v = m
                .volume
                .with_tremolo(m.tremolo)
                .with_tremolo(self.expression.volume)
                .scaled_by(self.get_volume());
            m.channel_volume.applied_to(v)
        } else {
            Volume::SILENT
        };

        let (pan_left, pan_right) = panning.stereo_split_linear();
        let actual_volume = [volume_q.scaled_by(pan_left), volume_q.scaled_by(pan_right)];

        let eff = self.update_frequency(
            m.period,
            m.arp_pitch,
            m.vibrato_raw,
            m.points_pitch,
            m.semitone,
        );
        (actual_volume, eff.unwrap_or(m.period))
    }

    /// Snap + clamp + sum + encode shared between the two
    /// frequency branches of [`Self::update_frequency`].
    ///
    /// * `semitone` (FT2 glissando) snaps to the nearest integer
    ///   semitone (Q8.8 raw multiple of 256).
    /// * The FT2 arpeggio quirk clamps the base note to ≤ 95
    ///   when arpeggio or pitch-envelope contributes anything.
    /// * Sum then `.max(0)` (negative pitch is meaningless);
    ///   `from_q8_8_i32_sat` handles the i16 ceiling.
    fn finalize_pitch(
        &self,
        mut base_pitch_q88: i32,
        delta_q88: i32,
        arp_active: bool,
        semitone: bool,
    ) -> xmrs::core::fixed::units::Pitch {
        if semitone {
            base_pitch_q88 = round_to_nearest_semitone_q88(base_pitch_q88);
        }
        if self.period_helper.legacy_arpeggio_clamp_enabled() && arp_active {
            base_pitch_q88 = base_pitch_q88.min(95 * 256);
        }
        let final_q88 = base_pitch_q88.saturating_add(delta_q88).max(0);
        xmrs::core::fixed::units::Pitch::from_q8_8_i32_sat(final_q88)
    }
}

impl<'a> StateInstrDefault<'a> {
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
        match self.instr.keyboard.note_for_pitch[input.value() as usize] {
            Some(out) => Pitch::try_from(out).unwrap_or(input),
            None => input,
        }
    }

    pub fn set_pitch(&mut self, note: Pitch) -> bool {
        if let Some(num) = self.instr.keyboard.sample_for_pitch[note.value() as usize] {
            return self.select_sample(num);
        }
        false
    }

    fn select_sample(&mut self, num: usize) -> bool {
        if num < self.instr.sample.len() {
            if let Some(sample) = &self.instr.sample[num] {
                let mut state_sample = StateSample::new(sample, self.rate);
                state_sample.interpolation = self.interpolation;
                // IT's pan cascade: sample override → instrument
                // default_pan → channel initial pan. The IT
                // importer stores exactly `0.5` in Sample.panning
                // when the sample's own bit-7-"use-pan" flag is
                // clear (i.e., "not set, defer to instrument"). We
                // can't distinguish that from an author's explicit
                // centre-pan, but treating exact-0.5 as "unset" is
                // the right heuristic — authors rarely place a
                // deliberate centre override, and the fallback to
                // `instr.voice.default_pan` is what the module expects.
                let sample_pan = state_sample.get_panning();
                self.panning = if sample_pan == Panning::CENTER {
                    // IT semantics: exact-centre = "unset, defer
                    // to instrument default_pan".
                    self.instr.voice.default_pan
                } else {
                    sample_pan
                };
                // ChannelVolume and Volume are both Q1.15 in
                // `[0, 1]`. The newtype distinction reflects
                // semantics (always-applied vs running per-voice
                // gain), not range. Sharing the raw Q15 is
                // lossless.
                self.volume = Volume::from_q15(state_sample.get_volume().raw());
                self.volume_orig = self.volume;
                self.state_sample = Some(state_sample);
                self.current_sample_num = Some(num);
                return true;
            }
        }
        self.state_sample = None;
        self.current_sample_num = None;
        self.panning = Panning::CENTER;
        self.volume = Volume::SILENT;
        false
    }

    pub fn tick(&mut self) {
        self.envelopes();
        self.state_vibrato.tick();
    }
}

impl<'a> Iterator for StateInstrDefault<'a> {
    type Item = (Amp, Amp);

    fn next(&mut self) -> Option<Self::Item> {
        if !self.is_enabled() {
            return None;
        }
        // Voice is silent and awaiting reap — its fadeout register
        // has reached zero (DCA NoteCut on the live voice, S70 past-
        // cut on a now-promoted ghost, or the natural end of an IT
        // fadeout). Mirrors the same gate in `Voice::next` (the
        // ghost path) and matches schism's per-render kill check at
        // `sndmix.c:1112-1118`. Without this, a DCA-cut live voice
        // would keep paying the full sample / filter / lerp work
        // every output sample until it's replaced by a fresh trigger
        // — a bounded waste (one such voice per channel at most),
        // but a nicely free win that aligns with the ghost path's
        // semantics.
        if self.volume_fading_out && self.volume_fadeout.raw() == Q15::ZERO {
            return None;
        }
        let raw = match &mut self.state_sample {
            Some(s) => s.next(),
            None => None,
        };
        // Filter is Q-format end-to-end: `process` takes
        // `(Amp, Amp)` and returns `(Amp, Amp)` directly. The
        // never-engaged short-circuit lives inside `process`
        // itself (`was_ever_enabled` check) — no `f32` touched
        // anywhere in the audio chain. For XM / MOD / S3M and
        // IT samples without filter bit-7 set, the function
        // returns its input unchanged.
        raw.map(|(l, r)| self.filter.process(l, r))
    }
}

/// Round a Q8.8 pitch to the nearest integer semitone (a Q8.8
/// raw multiple of `256`), half-away-from-zero. Used by the
/// FT2 glissando snap on both linear and Amiga period flavours.
const fn round_to_nearest_semitone_q88(pitch_q88: i32) -> i32 {
    if pitch_q88 >= 0 {
        ((pitch_q88 + 128) / 256) * 256
    } else {
        -(((-pitch_q88) + 128) / 256) * 256
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    // `xmrs::prelude::*` is already in scope through the file-
    // level `use` at the top, re-exported here by `super::*`.

    fn make_instr_with_remap(remap: &[(usize, u8)]) -> InstrDefault {
        let mut instr = InstrDefault::default();
        for (input, output) in remap {
            instr.keyboard.note_for_pitch[*input] = Some(*output);
        }
        instr
    }

    fn state_for(instr: &InstrDefault) -> StateInstrDefault<'_> {
        // Use linear-frequency period helper; played_pitch_for
        // doesn't actually consult the period helper, but
        // StateInstrDefault::new requires one.
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        // `true` mirrors the historical default and keeps these
        // pitch-only tests unaffected by the volume routing change.
        StateInstrDefault::new(instr, 0, ph, SampleRate::from_hz(44100), true)
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

    // -------- regression tests: voices stopping too early in IT --------
    //
    // The 0.10.x player started decrementing `volume_fadeout` the
    // instant `sustained` flipped to false (i.e. on every key-off),
    // which is wrong for IT instruments with a release section: the
    // fade was running in parallel with the release segment, so
    // pads / sustains went silent before the envelope had finished
    // its decay. Schism's policy (`fx_key_off`, `_process_envelope`,
    // `rn_process_ins_fade`) is to gate fadeout decay on a separate
    // CHN_NOTEFADE flag, set on key-off only in specific cases
    // (no envelope, looped envelope with fadeout > 0) and otherwise
    // deferred until the volume envelope walks past its last point.
    //
    // The xmrsplayer equivalent is `volume_fading_out`, exercised
    // by the tests below.

    /// Build an envelope in the IT-typical pad/sustain shape:
    /// rises from 0 → 1 over 10 ticks, sustains at frame 10, then
    /// decays linearly to 0 at frame 50. No loop, just sustain +
    /// release.
    fn pad_envelope() -> Envelope {
        Envelope {
            enabled: true,
            point: vec![
                EnvelopePoint {
                    frame: 0,
                    value: EnvValue::default(),
                },
                EnvelopePoint {
                    frame: 10,
                    value: EnvValue::VOLUME_DEFAULT,
                },
                EnvelopePoint {
                    frame: 50,
                    value: EnvValue::default(),
                },
            ],
            sustain_enabled: true,
            sustain_start_point: 1,
            sustain_end_point: 1,
            loop_enabled: false,
            loop_start_point: 0,
            loop_end_point: 0,
        }
    }

    fn instr_with_envelope(env: Envelope, fadeout: Volume) -> InstrDefault {
        let mut instr = InstrDefault::default();
        instr.voice.volume_envelope = env;
        instr.voice.volume_fadeout = fadeout;
        instr
    }

    #[test]
    fn it_pad_release_section_plays_before_fadeout() {
        // The bug: after key-off on an IT pad (sustain + release, no
        // loop), `volume_fadeout` was decrementing every tick as the
        // envelope walked through its release section. Voices died
        // before the envelope's natural decay finished.
        //
        // The fix: `volume_fading_out` (CHN_NOTEFADE-equivalent)
        // stays false through the release, so `volume_fadeout` is
        // held at 1.0. It only flips true after the envelope has
        // walked past its last point — at which point the fadeout
        // register starts decaying.
        let instr = instr_with_envelope(pad_envelope(), Volume::from_ratio(1, 100));
        let mut state = state_for(&instr);
        assert!(state.sustained);
        assert!(!state.volume_fading_out);
        assert_eq!(state.volume_fadeout, Volume::FULL);

        state.key_off();
        // Sustain released, but fading not yet armed: envelope has
        // a real release section to play through first.
        assert!(!state.sustained, "key-off must release sustain");
        assert!(
            !state.volume_fading_out,
            "fading must NOT engage on key-off when the envelope has a release section"
        );

        // Tick the envelope from sustain (counter starts at 0 in
        // a fresh state) toward the last point at frame 50. All
        // through that walk, the fadeout register must stay at 1.0
        // — the release section is still playing.
        for _ in 0..49 {
            state.tick();
            assert_eq!(
                state.volume_fadeout,
                Volume::FULL,
                "fadeout must not decay while the release section is still playing"
            );
            assert!(
                !state.volume_fading_out,
                "fading must not arm before the envelope has walked past its last point"
            );
        }

        // Push past the last point. The exact tick at which
        // `is_past_last_point` flips depends on the inner branch
        // ordering of `StateEnvelope::tick`, but a handful of
        // extra ticks comfortably brackets it.
        for _ in 0..5 {
            state.tick();
        }
        assert!(
            state.volume_fading_out,
            "envelope walking past its last point must arm fadeout"
        );

        // From now on, fadeout decays. The next tick brings the
        // register strictly below its previous value.
        let pre = state.volume_fadeout;
        state.tick();
        assert!(
            state.volume_fadeout.raw() < pre.raw(),
            "fadeout register must decrement once the envelope is past its end"
        );
    }

    #[test]
    fn no_envelope_fades_immediately_on_key_off() {
        // No volume envelope at all (typical MOD/S3M sample, or an
        // IT instrument without an envelope): schism's `fx_key_off`
        // sets CHN_NOTEFADE right away because the envelope isn't
        // there to defer the decision.
        let instr = instr_with_envelope(Envelope::default(), Volume::from_ratio(1, 100));
        let mut state = state_for(&instr);
        state.key_off();
        assert!(
            state.volume_fading_out,
            "key-off on a no-envelope instrument must engage fadeout immediately"
        );
        // And the fadeout register must actually start moving.
        let pre = state.volume_fadeout;
        state.tick();
        assert!(state.volume_fadeout.raw() < pre.raw());
    }

    #[test]
    fn looped_envelope_with_fadeout_fades_immediately_on_key_off() {
        // Envelope with a loop AND a non-zero fadeout: schism's
        // `fx_key_off` last branch (`effects.c:166-167`) engages
        // CHN_NOTEFADE right away — without it, the looped envelope
        // would hold the voice forever past key-off.
        //
        // FT2/XM modules reach this branch for *every* enveloped
        // voice: the XM importer pins a degenerate loop on the
        // last point so that this exact branch fires, matching
        // FT2's "fadeout is parallel with the release section"
        // semantics.
        let mut env = pad_envelope();
        env.loop_enabled = true;
        env.loop_start_point = 2; // last point
        env.loop_end_point = 2;
        let instr = instr_with_envelope(env, Volume::from_ratio(1, 100));
        let mut state = state_for(&instr);
        state.key_off();
        assert!(
            state.volume_fading_out,
            "key-off on a looped envelope with non-zero fadeout must engage fadeout immediately"
        );
    }

    #[test]
    fn start_fadeout_does_not_release_sustain() {
        // NNA NoteFade / DCA NoteFadeOut / S72 / TrackEffect::
        // NoteFadeOut all funnel into `start_fadeout`. Schism's
        // matching code only sets CHN_NOTEFADE — sustain stays
        // held. The previous implementation went through `key_off`
        // here, conflating fade with note-off and making the
        // envelope walk into its release section instead of
        // continuing to wrap in the sustain loop.
        let instr = instr_with_envelope(pad_envelope(), Volume::from_ratio(1, 100));
        let mut state = state_for(&instr);
        assert!(state.sustained);
        state.start_fadeout();
        assert!(state.sustained, "start_fadeout must NOT release sustain");
        assert!(state.volume_fading_out, "start_fadeout must engage fadeout");
        // And fadeout starts decaying on the next tick.
        let pre = state.volume_fadeout;
        state.tick();
        assert!(state.volume_fadeout.raw() < pre.raw());
    }
}
