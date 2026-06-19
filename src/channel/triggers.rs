//! Note-trigger primitives: build the played pitch, allocate /
//! reuse a live voice in the pool, propagate finetune + envelope
//! reset flags, and the per-tick instrument tick.

use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::PitchDelta;
use xmrs::prelude::*;

use crate::triggerkeep::{
    contains, TriggerKeep, TRIGGER_KEEP_ENVELOPE, TRIGGER_KEEP_PERIOD,
    TRIGGER_KEEP_SAMPLE_POSITION, TRIGGER_KEEP_VOLUME,
};
use crate::voice_pool::VoicePool;

use super::Channel;

impl<'a> Channel<'a> {
    pub(crate) fn trigger_pitch(&mut self, flags: TriggerKeep, pool: &mut VoicePool<'a>) {
        // A new note always starts un-muted: in both FT2 (resets
        // `realVol` from the instrument) and ST3 (`avol = aorgvol`
        // on trigger), any tremor mute from previous rows is
        // overridden by the fresh note's volume.
        //
        // For `tremor_state_persists` modules (ST3), the internal
        // counter state (`effect_tremor_counter_s3m` + `_silent_s3m`)
        // is intentionally NOT reset here — ST3's `atremor` / `atreon`
        // live across note triggers, so the next tick where Ixy runs
        // resumes the cycle from where it left off. Only the
        // immediate volume gate (`effect_tremor`) is cleared.
        self.effect_tremor = false;

        if let Some(instr) = self.live_mut(pool) {
            if !contains(flags, TRIGGER_KEEP_SAMPLE_POSITION) {
                instr.sample_reset();
                // Sample restarted from the head this tick = an audible
                // re-attack. Exposed via the snapshot's `struck` for the
                // DW oracle re-strike diff. Cleared at each tick start.
                self.struck_this_tick = true;
            }

            if !contains(flags, TRIGGER_KEEP_ENVELOPE) {
                instr.envelopes_reset();
            }

            instr.vibrato_reset();

            if !contains(flags, TRIGGER_KEEP_VOLUME) {
                instr.volume_reset();
                // Volume cascade at note-trigger time depends on
                // the format's `Sample.volume` semantics:
                //
                //   IT  : `Sample.volume` (GvL) keeps scaling the
                //         voice on every mixer tick inside
                //         `StateInstrDefault::get_volume`. At
                //         trigger time, only the per-sample Vol
                //         (`default_note_volume`) seeds the
                //         channel volume — GvL is already in the
                //         downstream chain. A V-column override
                //         later in effect processing replaces
                //         `self.volume` wholesale, at which
                //         point Vol is cleanly gone but GvL
                //         still scales the sample.
                //
                //   MOD/XM/S3M : `Sample.volume` is consumed once
                //         here as the channel's initial volume.
                //         `default_note_volume` is 1.0 for these
                //         formats, so this is effectively
                //         `self.volume = Sample.volume`. The
                //         downstream `get_volume` then skips its
                //         `× self.volume` step (see the
                //         `sample_volume_is_static_gain` branch
                //         there), avoiding the squaring that
                //         used to silently attenuate every
                //         reduced-volume sample.
                let dnv = instr.current_sample_default_note_volume();
                self.volume = if self.module.quirks.sample_volume_is_static_gain {
                    dnv
                } else {
                    instr.volume.scaled_by(dnv)
                };
            }

            // Panning reset: driven by the format's
            // [`PanResetPolicy`]. Pitch-pan separation lives inside
            // the same branch — it's a per-note offset relative to
            // the instrument's stored pan, so it makes sense only
            // when we just landed on that stored pan. A ghost
            // retrigger inherits whatever pps the previous note
            // baked in.
            let do_pan_reset = match self.module.quirks.pan_reset_policy {
                PanResetPolicy::Never => false,
                PanResetPolicy::OnInstrumentChange => self.current.has_instrument_column(),
                PanResetPolicy::Always => true,
            };
            if do_pan_reset {
                self.panning = instr.panning;

                let pps = instr.pitch_pan_separation();
                if pps != Q15::ZERO {
                    // `self.note` is a `Pitch` (Q8.8 semitones).
                    // Truncate to integer semitones for the
                    // pitch-pan separation centre comparison.
                    let note_i = self.note.as_q8_8_i32() / 256;
                    let ppc = instr.pitch_pan_center_semitones();
                    // Pure Q1.15:
                    //   note_factor = (note_i - ppc) / 60      (small ratio)
                    //   shift       = pps × note_factor        (Q15 sat mul)
                    //   panning'    = panning.shifted_by(shift)
                    //
                    // `Q15::from_ratio` saturates the rational form
                    // (note range up to ±71 semitones, /60 → can
                    // exceed +1 — saturation is benign because the
                    // pan output `shifted_by` clamps to `[0, 1]`).
                    let note_factor = Q15::from_ratio(note_i - ppc, 60);
                    let shift = pps.mul(note_factor);
                    self.panning = self.panning.shifted_by(shift);
                }
            }

            if !contains(flags, TRIGGER_KEEP_PERIOD) {
                // Pitch → Period direct (no f32 round-trip).
                self.period = self.period_helper.note_to_period(self.note);

                // Reset the effect vibrato / tremolo phase on a new-note
                // trigger, unless the channel flag explicitly says to
                // keep it. Tone portamento, ghost instruments and keyoffs
                // take the `TRIGGER_KEEP_PERIOD` path above and skip this
                // block entirely — they don't count as new-note triggers.
                if self.vibrato_retrig_on_new_note {
                    self.retrigger_vibrato_all();
                }
                if self.tremolo_retrig_on_new_note {
                    self.retrigger_tremolo_all();
                }

                let points_pitch = self.track_pitch_points_delta();
                let eff = instr.update_frequency(
                    self.period,
                    PitchDelta::ZERO,
                    self.vibrato_value_raw(),
                    points_pitch,
                    self.effect_semitone,
                );
                // Record the played period (post-modulation) for the
                // snapshot; default to the nominal period if no sample.
                self.effective_period = eff.unwrap_or(self.period);
            }
        }

        // IT humanisation (random_volume_variation /
        // random_pan_variation). Applied AFTER the main instr block
        // closes so we can freely use `&mut self` for the PRNG
        // without the borrow checker fighting the `instr` read. Done
        // only on fresh triggers (not when `TRIGGER_KEEP_VOLUME` is
        // set — ghost-note retrigs and tone-portamento should not
        // re-roll). Non-IT formats have both variations at zero, so
        // this path is a measured no-op there.
        if !contains(flags, TRIGGER_KEEP_VOLUME) {
            let (rvv, rpv) = match self.live(pool) {
                Some(instr) => (
                    instr.random_volume_variation(),
                    instr.random_pan_variation(),
                ),
                None => (Q15::ZERO, Q15::ZERO),
            };
            if rvv > Q15::ZERO {
                // Pure Q1.15:
                //   d  = next_q15_bipolar × rvv          (signed Q15)
                //   δ  = volume × d                      (signed Q15)
                //   v' = volume.with_tremolo(δ)          (sat add + clamp)
                //
                // Linearises `volume × (1 + d)` as
                // `volume + volume × d` (small multiplicative
                // perturbation around unity).
                let d = self.rng.next_q15_bipolar().mul(rvv);
                let delta = self.volume.raw().mul(d);
                self.volume = self.volume.with_tremolo(delta);
            }
            if rpv > Q15::ZERO {
                // Pure Q1.15: signed delta added to pan,
                // saturating with clamp via `Panning::shifted_by`.
                let d = self.rng.next_q15_bipolar().mul(rpv);
                self.panning = self.panning.shifted_by(d);
            }
        }
    }

    /// RFC §2A — assemble this channel's per-tick [`VoiceMod`] bus: a
    /// pure read of the channel-side modulation (period, volume, pan,
    /// channel-volume, the §1B-summed LFO outputs, arpeggio, Points
    /// transpose, glissando/tremor flags). The voice-internal envelope
    /// is deliberately *absent* — a voice folds that in itself at
    /// [`StateInstrDefault::apply_voice_mod`] time.
    pub(super) fn voice_mod(&self) -> crate::voice_mod::VoiceMod {
        let arp_pitch = if self.current.has_arpeggio() {
            self.effect_arpeggio.value_pitch_delta()
        } else {
            PitchDelta::ZERO
        };
        crate::voice_mod::VoiceMod {
            period: self.period,
            volume: self.volume,
            panning: self.panning,
            channel_volume: self.channel_volume,
            arp_pitch,
            vibrato_raw: self.vibrato_value_raw(),
            points_pitch: self.track_pitch_points_delta(),
            semitone: self.effect_semitone,
            tremolo: self.tremolo_value_q15(),
            panbrello: self.panbrello_value_q15(),
            tremor: self.effect_tremor,
        }
    }

    pub(super) fn tickn_update_instr(&mut self, pool: &mut VoicePool<'a>) {
        // RFC §2A: fill the modulation bus from the channel, then let
        // the live voice consume it (combining with its own envelope).
        // One consumer today → bit-identical to the pre-2A inline path;
        // §2B re-points `apply_voice_mod` across every voice.
        let m = self.voice_mod();
        if let Some(instr) = self.live_mut(pool) {
            let (actual_volume, effective_period) = instr.apply_voice_mod(&m);
            self.actual_volume = actual_volume;
            self.effective_period = effective_period;
        }
    }
}
