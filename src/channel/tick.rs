//! Per-tick cycle: top-level `tick`, the big `TrackEffect` match
//! in `tickn_effects`, and the three long-running effect handlers
//! (NoteDelay / NoteRetrig / Tremor).

use xmrs::prelude::*;

use crate::triggerkeep::{
    TRIGGER_KEEP_ENVELOPE, TRIGGER_KEEP_NONE, TRIGGER_KEEP_PERIOD, TRIGGER_KEEP_SAMPLE_POSITION,
    TRIGGER_KEEP_VOLUME,
};
use crate::voice_pool::VoicePool;

use super::{compose_played_pitch, Channel};

#[derive(Clone, PartialEq, Default)]
pub(super) struct NoteRetrigState {
    pub event: CellEvent,
    /// Track-level instrument of the cell that armed this retrig.
    /// Instruments live per-track, not per-cell, so the retrig
    /// identity carries both the typed event (pitch + velocity +
    /// ghost/fresh distinction) and the resolved track instrument
    /// captured at row-start time.
    pub track_instr: Option<usize>,
    pub speed: usize,
    pub volume_modifier: NoteRetrigOperator,
}

impl<'a> Channel<'a> {
    pub(crate) fn tick(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
        // Record the within-row tick so the per-frame `TrackPitch`
        // Points lane (DW arpeggio) reads its value at the absolute
        // per-frame tick, not just the row start.
        self.current_tick_in_row = current_tick;
        // Clear the per-tick re-attack flag before any retrigger effect
        // (e.g. Rxy) this tick can set it (see `struck_this_tick`).
        self.struck_this_tick = false;
        if let Some(instr) = self.live_mut(pool) {
            instr.tick();
            self.tickn_effects(current_tick, pool);
            self.tickn_update_instr(pool);
        } else if self.current.has_delay() {
            self.tickn_effects(current_tick, pool);
            self.tickn_update_instr(pool);
        }
        // Advance ghost voices even if the live instrument is gone —
        // ghosts have independent envelope state and must keep
        // decaying regardless of what the pattern is doing.
        self.tick_ghosts(pool);
    }

    pub(super) fn tickn_effects(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
        if current_tick == 0 {
            // At every row-load tick, FT2 clears the tremor mute flag:
            // its Tremor effect never runs at tickZero (only in the
            // TickNonZero jump table), so the row always begins
            // "playing". If the current row also carries a Tremor
            // effect, its handler below will overwrite this; if it
            // doesn't, the previous row's last tremor state (which
            // might have left us on the "off" half) must not mute the
            // whole next row.
            //
            // ST3 does NOT clear at row load: `atreon` and `atremor`
            // persist across rows, so the cycle continues uninter-
            // rupted when consecutive rows carry Ixy. For S3M we
            // therefore skip the clear; the mute flag is driven
            // exclusively from the state machine in the Tremor arm
            // below.
            if !self.module.quirks.tremor_state_persists {
                self.effect_tremor = false;
            }
        }
        let len = self.current.effects.len();
        for i in 0..len {
            match self.current.effects[i].clone() {
                TrackEffect::Arpeggio {
                    half1: n1,
                    half2: n2,
                } => {
                    if current_tick == 0 {
                        self.effect_arpeggio.tick0_semitones(n1, n2);
                    } else if n1 != 0 || n2 != 0 {
                        self.effect_arpeggio.tick();
                    }
                }
                TrackEffect::ChannelVolume(v) => {
                    // Both sides typed: direct assignment, no bridge.
                    self.channel_volume = v;
                }
                TrackEffect::Glissando(glissando) => {
                    if current_tick == 0 {
                        self.effect_semitone = glissando;
                    }
                }
                TrackEffect::InstrumentFineTune(finetune) => {
                    if current_tick == 0 {
                        if let Some(pitch) = self.current.pitch() {
                            if let Some(instr) = self.live_mut(pool) {
                                // `Finetune` is now end-to-end Q-typed.
                                instr.set_finetune(finetune);
                                // Recomputing the playback frequency
                                // after a finetune change must use the
                                // remapped output note, same as the
                                // initial trigger. Otherwise a finetune
                                // event on a drum-kit voice would jump
                                // to the input-note pitch and break the
                                // remap.
                                let played = instr.played_pitch_for(pitch);
                                self.note =
                                    compose_played_pitch(played, instr.get_finetuned_pitch());
                                self.period = self.period_helper.note_to_period(self.note);
                            }
                        }
                    }
                }
                TrackEffect::InstrumentNewNoteAction(nna) => {
                    // S73 / S74 / S75 / S76 — override the NNA that
                    // will govern the NEXT trigger on this channel.
                    // Applied at tick 0 only; the override is single-
                    // shot and cleared inside `tick0_change_instr`
                    // the first time a new instrument is loaded.
                    if current_tick == 0 {
                        self.nna_override = Some(nna);
                    }
                }
                TrackEffect::InstrumentPanningEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_panning.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentPanningEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_panning.enabled = pe;
                        }
                    }
                }
                TrackEffect::InstrumentPitchEnvelope(pe) => {
                    // S7B / S7C: enable / disable the instrument's
                    // pitch envelope on this channel. Same tick-0
                    // scoping as the volume / panning envelope
                    // toggles above. Takes effect on the next tick
                    // via `StateInstrDefault::envelopes()`.
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_pitch.enabled = pe;
                        }
                    }
                }
                TrackEffect::InstrumentSampleOffset(seek) => {
                    if current_tick == 0 && self.current.pitch().is_some() {
                        if let Some(instr) = self.live_mut(pool) {
                            if let Some(sample) = &mut instr.state_sample {
                                let sample_len = sample.sample_len();
                                if seek >= sample_len {
                                    // Past-end behaviour branches on
                                    // the IT "Old Effects" flag
                                    // (ITTECH: "Oxx past the sample
                                    // end will be ignored, unless
                                    // 'Old Effects' is ON, in which
                                    // case the Oxx will play from
                                    // the end of the sample."). The
                                    // XM / MOD / S3M paths behave
                                    // like IT-default (ignore); a
                                    // proper-IT module with
                                    // old_effects ON will clamp.
                                    if self.module.quirks.it_old_effects && sample_len > 0 {
                                        sample.set_position(sample_len - 1);
                                    }
                                    // else: ignore the offset; leave
                                    // the position where the natural
                                    // note trigger put it.
                                } else {
                                    sample.set_position(seek);
                                }
                            }
                        }
                    }
                }
                TrackEffect::InstrumentSurround(surround) => {
                    // S91 (surround) / S90 (off). Pseudo-stereo
                    // "surround" in IT is classically implemented by
                    // inverting the phase of the right channel's
                    // output — the inverted pair folds to silence in
                    // mono (a characteristic IT fingerprint) and
                    // sounds spatially wider than pure stereo in a
                    // two-speaker field. The actual inversion lives
                    // in `Channel::next`; this arm just toggles the
                    // flag. Persists until explicitly toggled;
                    // note triggers do NOT reset it.
                    if current_tick == 0 {
                        self.surround = surround;
                    }
                }
                TrackEffect::InstrumentVolumeEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_volume.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentVolumeEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_volume.enabled = pe;
                        }
                    }
                }
                TrackEffect::NoteCut { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S70: cut all ghost voices on this
                            // channel. Live voice is untouched per
                            // ITTECH (S7x targets only detached
                            // notes).
                            self.past_note_cut_all(pool);
                        } else {
                            self.cut_pitch();
                        }
                    }
                }
                TrackEffect::NoteDelay(delay) => self.handle_note_delay(delay, current_tick, pool),
                TrackEffect::NoteFadeOut { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S72: fade all ghost voices.
                            self.past_note_fade_all(pool);
                        } else if let Some(i) = self.live_mut(pool) {
                            // Engage the fadeout register without
                            // releasing sustain. The previous
                            // implementation called `key_off`,
                            // which on IT pads/sustains made the
                            // envelope walk into its release
                            // section AND kicked off the fadeout
                            // in parallel — voices ended up
                            // shorter than schism plays them.
                            // `start_fadeout` matches schism's
                            // NOTE_FADE path (just CHN_NOTEFADE,
                            // no CHN_KEYOFF).
                            i.start_fadeout();
                        }
                    }
                }
                TrackEffect::NoteOff { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S71: key-off all ghost voices.
                            self.past_note_off_all(pool);
                        } else {
                            self.key_off(pool);
                        }
                    }
                }
                TrackEffect::NoteRetrig {
                    speed,
                    volume_modifier,
                } => self.handle_note_retrig(speed, volume_modifier, current_tick, pool),
                TrackEffect::Panning(p) => {
                    if current_tick == 0 {
                        // Both sides typed — direct assignment.
                        self.panning = p;
                    }
                }
                TrackEffect::Tremor { on_time, off_time } => {
                    self.handle_tremor(on_time, off_time, current_tick)
                }
                TrackEffect::Volume { value: v, tick: t } => {
                    if current_tick == t {
                        // Both sides typed — direct assignment.
                        self.volume = v;
                    }
                }
                TrackEffect::MidiMacro(_) => {
                    // The macro is invoked from `cell.effects` by
                    // `Voices::apply_row_per_channel_effects`. The
                    // channel arm itself is a no-op — the macro
                    // bytes mutate channel state through the voices
                    // path, not here.
                }
            }
        }

        // Slide + LFO per-tick application driven by
        // `AutomationLane`s — nothing in the cell-side match-loop
        // above advances them.
        self.apply_slides_from_lanes(current_tick);
        self.advance_lfos_from_lanes(current_tick);
    }

    /// `SDx` — fire the row's events `delay` ticks late. On
    /// tick 0 the channel still reacts to the *previous* state
    /// (instrument-reset / key-off variants), the actual trigger
    /// happens when `current_tick == delay`.
    fn handle_note_delay(&mut self, delay: usize, current_tick: usize, pool: &mut VoicePool<'a>) {
        if current_tick == 0 {
            // Match the legacy `cell.note` dispatch via the
            // [`CellEvent`] projection. `Empty` covered
            // (None / InstrReset); `KeyOff` covered
            // (NoteOff{retrig:false/true}). The `retrig`
            // flag implies "instrument column present" —
            // mirror of the legacy `instrument.is_some()`
            // check below.
            match self.current.event {
                CellEvent::None | CellEvent::InstrReset => {
                    self.trigger_pitch(
                        TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_PERIOD,
                        pool,
                    );
                }
                CellEvent::NoteOff { retrig: false } => {
                    self.key_off(pool);
                }
                CellEvent::NoteOff { retrig: true } => {
                    self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE, pool);
                }
                _ => {}
            }
        } else if current_tick == delay {
            self.tick0_load_instrument_and_pitch(pool);
            self.tickn_effects(0, pool);

            /* Special KeyOff cases */
            match self.current.event {
                CellEvent::NoteOff { retrig: false } => {
                    if let Some(i) = self.live_mut(pool) {
                        i.volume_reset();
                    }
                }
                CellEvent::NoteOff { retrig: true } => {
                    self.trigger_pitch(TRIGGER_KEEP_NONE, pool);
                }
                _ => {}
            }
        }
    }

    /// `Qxy` — periodic retrigger with optional volume modulation.
    /// Cadence is measured from the row's effective trigger tick
    /// (which is `effect_note_delay`, 0 on undelayed rows).
    fn handle_note_retrig(
        &mut self,
        speed: usize,
        volume_modifier: NoteRetrigOperator,
        current_tick: usize,
        pool: &mut VoicePool<'a>,
    ) {
        // NoteDelay interaction: when a row carries both `SDx`
        // (NoteDelay) and `Qxy` (NoteRetrig), the retrig cadence
        // is measured from the delayed trigger, not from tick 0.
        // Before the delay fires, we simply don't count — the
        // voice hasn't actually started yet.
        if current_tick < self.effect_note_delay {
            return;
        }

        let current_state = NoteRetrigState {
            event: self.current.event,
            track_instr: self.current_track_instrument,
            speed,
            volume_modifier: volume_modifier.clone(),
        };

        // Reset the retrig counter at the moment the effect
        // becomes active on a new row — for a plain row that's
        // tick 0, for a SDx-delayed row it's the tick the delay
        // fires.
        let counter_reset_tick = self.effect_note_delay;
        if current_tick == counter_reset_tick && self.effect_note_retrig_backup != current_state {
            self.effect_note_retrig_counter = 0;
            self.effect_note_retrig_backup = current_state;
        }

        // Increment the counter FIRST, then test.
        self.effect_note_retrig_counter += 1;

        // If `speed` is 0, retrig is effectively disabled.
        if speed != 0 && self.effect_note_retrig_counter.is_multiple_of(speed) {
            self.trigger_pitch(TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_ENVELOPE, pool);
            match volume_modifier {
                NoteRetrigOperator::None => {}
                NoteRetrigOperator::Sum(delta) => {
                    // Pure Q1.15: saturating add of a signed Q15
                    // modulation, clamped to `[0, 1]` by
                    // `with_tremolo`.
                    self.volume = self.volume.with_tremolo(delta);
                }
                NoteRetrigOperator::Mul(factor) => {
                    // Pure Q1.15: Q3.13 × Q1.15 with saturation,
                    // all in `RetrigMul::applied_to`.
                    self.volume = factor.applied_to(self.volume);
                }
            }
        }
    }

    /// `Ixy` — tremor (volume gating). Two flavours selected by
    /// `module.quirks.tremor_state_persists`:
    ///
    /// * ST3 (digcmd.c:s_tremor line 803): a count-down +
    ///   on/off toggle, both persistent across rows. Consecutive
    ///   Ixy rows form a continuous cycle.
    /// * FT2/XM Txy: per-row retrigger with modular formula.
    fn handle_tremor(&mut self, on: usize, off: usize, current_tick: usize) {
        if self.module.quirks.tremor_state_persists {
            // The on/off nibble *cache* is refreshed at tick 0
            // (so memory via GET_LAST_NFO is in sync) but the
            // state machine is NOT reset — consecutive Ixy rows
            // form a continuous cycle, matching ST3.
            //
            // Initial state: counter == -1 (inactive). On the
            // first Ixy tick, we treat -1 as "toggle now" and
            // the first toggle lands on "playing" (silent=false,
            // matching ST3's first hit where atreon flips from
            // false to true).
            if current_tick == 0 {
                self.effect_tremor_on = on;
                self.effect_tremor_off = off;
            }
            if self.effect_tremor_counter_s3m > 0 {
                self.effect_tremor_counter_s3m -= 1;
            } else {
                // Toggle on/off (or start "on" from the initial
                // inactive state). ITTECH: the counter reloads
                // with nibble + 1 so that `Ix0` still produces a
                // one-tick on phase (reload = 1, fires once,
                // toggles next tick). Parser stores raw nibble;
                // we apply the `+1` here to keep import
                // conventions simple.
                self.effect_tremor_silent_s3m = !self.effect_tremor_silent_s3m;
                let reload = if self.effect_tremor_silent_s3m {
                    self.effect_tremor_off + 1
                } else {
                    self.effect_tremor_on + 1
                } as i32;
                self.effect_tremor_counter_s3m = reload;
            }
            self.effect_tremor = self.effect_tremor_silent_s3m;
        } else {
            // FT2/XM: `current_tick - 1` gives 0 on the first
            // effect tick; `(on + off + 2)` is the cycle length
            // (FT2 plays for `on + 1` ticks then silences for
            // `off + 1`).
            if current_tick == 0 {
                self.effect_tremor_on = on;
                self.effect_tremor_off = off;
                self.effect_tremor = false;
            } else {
                let on = self.effect_tremor_on;
                let off = self.effect_tremor_off;
                self.effect_tremor = (current_tick - 1) % (on + 1 + off + 1) > on;
            }
        }
    }
}
