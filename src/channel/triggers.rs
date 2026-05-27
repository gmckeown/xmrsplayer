//! Note-trigger primitives: build the played pitch, allocate /
//! reuse a live voice in the pool, propagate finetune + envelope
//! reset flags, and the per-tick instrument tick.

use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{Panning, PitchDelta, Volume};
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
                    self.effect_vibrato.retrigger_q();
                }
                if self.tremolo_retrig_on_new_note {
                    self.effect_tremolo.retrigger_q();
                }

                instr.update_frequency(
                    self.period,
                    PitchDelta::ZERO,
                    self.effect_vibrato.value_pitch_delta(),
                    self.effect_semitone,
                );
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

    pub(super) fn tickn_update_instr(&mut self, pool: &mut VoicePool<'a>) {
        if let Some(instr) = self.live_mut(pool) {
            // Panning chain in pure Q1.15 raw arithmetic.
            //
            //   pan      = self.panning             (Q1.15 [0, 32768])
            //   spc      = 1 − 2·|pan − 0.5|        (Q1.15 [0, 32768])
            //              = 32768 − 2·|pan − 16384|
            //   env_off  = envelope.value − 0.5     (Q1.15 signed
            //                                       [-16384, +16384])
            //   panbrello = LFO output              (Q1.15 signed
            //                                       [-32768, +32767])
            //   panning  = pan
            //            + (env_off    × spc) >> 15
            //            + (panbrello  × spc) >> 15
            //   panning = clamp(panning, 0, 32767)
            const HALF: i32 = 16384; // 0.5 in Q1.15 raw
            const ONE: i32 = 32768; // 1.0 in Q1.15 raw (same as Volume::FULL+1)
            let pan_q15: i32 = self.panning.as_q15_i32();
            let dist = (pan_q15 - HALF).abs();
            let spc_q15: i32 = ONE - 2 * dist; // [0, 32768]
            let env_off_q15: i32 = instr.envelope_panning.value.as_q15_i32() - HALF;
            let panbrello_q15: i32 = self.effect_panbrello.value_q15().raw() as i32;

            // Q1.15 × Q1.15 = Q2.30, narrow >> 15 → Q1.15.
            // The intermediate fits in i32: max 32768 × 16384 = 5 × 10^8.
            let env_contrib = (env_off_q15 * spc_q15) >> 15;
            let panbrello_contrib = (panbrello_q15 * spc_q15) >> 15;
            let panning = Panning::from_q15(Q15::from_i32_unsigned_sat(
                pan_q15 + env_contrib + panbrello_contrib,
            ));

            // Volume chain: pure Q1.15 from `self.volume` to the
            // final `Volume`.
            //
            //   volume.with_tremolo(tremolo)             // Volume + Q15
            //         .scaled_by(instr.get_volume())     // × Volume
            //         .let cv.applied_to(...)            // × ChannelVolume
            let volume_q = if !self.effect_tremor {
                let v = self
                    .volume
                    .with_tremolo(self.effect_tremolo.value_q15())
                    .scaled_by(instr.get_volume());
                self.channel_volume.applied_to(v)
            } else {
                Volume::SILENT
            };

            // Tracker panning is linear: 0 → full left, FULL → full
            // right. `actual_volume[0]` multiplies the left sample
            // channel, `actual_volume[1]` the right (every
            // historical tracker — PT/ST3/FT2/IT — and schism's
            // `sndmix.c:544`).
            let (pan_left, pan_right) = panning.stereo_split_linear();
            self.actual_volume[0] = volume_q.scaled_by(pan_left);
            self.actual_volume[1] = volume_q.scaled_by(pan_right);

            // Frequency
            let arp_pitch = if self.current.has_arpeggio() {
                self.effect_arpeggio.value_pitch_delta()
            } else {
                PitchDelta::ZERO
            };

            instr.update_frequency(
                self.period,
                arp_pitch,
                self.effect_vibrato.value_pitch_delta(),
                self.effect_semitone,
            )
        }
    }
}
