//! Row-start (tick 0) work: load the row's instrument, load its
//! pitch, dispatch the row's effects, and the public `tick0`
//! entry point called by the player.

use xmrs::prelude::*;

use crate::state_instr_default::StateInstrDefault;
use crate::triggerkeep::{
    TRIGGER_KEEP_ENVELOPE, TRIGGER_KEEP_NONE, TRIGGER_KEEP_PERIOD, TRIGGER_KEEP_SAMPLE_POSITION,
    TRIGGER_KEEP_VOLUME,
};
use crate::voice_pool::VoicePool;

use super::{compose_played_pitch, Channel};

impl<'a> Channel<'a> {
    fn tick0_change_instr(&mut self, sample_only: bool, pool: &mut VoicePool<'a>) -> bool {
        let instrnr = self.current_track_instrument.unwrap();

        // Arm / disarm Euclidean humanisation tracking. A trigger
        // coming from a [`Track::Euclidean`] arms the channel (the
        // NEXT pulse of the same track becomes eligible); a trigger
        // from a [`Track::Notes`] clears it. The track identity (not
        // the instrument identity) is the front key — two
        // Euclidean tracks on the same instrument stay independent.
        let euclidean_track = self
            .lookup_track_idx(self.current_abs_tick, self.current_song_idx)
            .and_then(|idx| match self.module.tracks.get(idx as usize) {
                Some(t) if t.is_euclidean() => Some(idx as usize),
                _ => None,
            });
        if let Some(track_idx) = euclidean_track {
            self.humanize_ekn.note_euclidian_triggered(track_idx);
        } else {
            self.humanize_ekn.clear_euclidian();
        }

        if let InstrumentType::Default(id) = &self.module.instrument[instrnr].instr_type {
            let was_same = self.live(pool).is_some_and(|i| i.num == instrnr);

            // Only proceed if the instrument has samples
            if !id.sample.is_empty() {
                if sample_only {
                    if let Some(i) = self.live_mut(pool) {
                        i.replace_instr(id);
                    }
                } else {
                    // IT DCT: the INCOMING instrument may ask the
                    // replayer to apply a Duplicate Check Action
                    // (cut / off / fade) to any already-playing
                    // voice whose identity clashes with the new
                    // trigger per its DCT axis (Note / Sample /
                    // Instrument). DCT runs BEFORE NNA: the new
                    // note's "clash preferences" are resolved first,
                    // then NNA decides what happens to whatever live
                    // voice is still standing afterwards.
                    let new_note = self.current.pitch();
                    let new_sample_index = new_note
                        .and_then(|n| {
                            id.keyboard
                                .sample_for_pitch
                                .get(n.value() as usize)
                                .copied()
                        })
                        .flatten();
                    self.apply_dct(pool, instrnr, new_note, new_sample_index);

                    // IT NNA: if the outgoing voice's NNA is anything
                    // but `Cut`, fork it into a ghost before we blow
                    // it away with the new trigger. On non-IT
                    // modules this is a cheap no-op because their
                    // importers leave NNA = NoteCut (the default).
                    self.spawn_ghost_for_outgoing(pool);
                    let state = StateInstrDefault::new(
                        id,
                        instrnr,
                        self.period_helper.clone(),
                        self.rate,
                        self.module.quirks.sample_volume_is_static_gain,
                    );
                    self.install_live(pool, state, id.midi_mute_computer);
                    // Consume any one-shot S73–S76 override — it
                    // only affects this one trigger.
                    self.nna_override = None;
                }
            }

            was_same
        } else {
            // Non-renderable instrument variant (InstrMidi / InstrOpl /
            // InstrSid / InstrRobSid, or an Euclidean whose `instr`
            // points to one of those). xmrsplayer's sample engine
            // only handles `InstrumentType::Default`; the specialised
            // synth / MIDI-out paths are the responsibility of
            // downstream code that wraps the player.
            false
        }
    }

    /// Return true if it was the same instrument
    fn tick0_load_instrument(&mut self, pool: &mut VoicePool<'a>) -> bool {
        // Determine whether this cell carries an instrument-column
        // load (mirror of the legacy `cell.instrument.is_some()`
        // semantic gate). The value of the instrument index lives in
        // `self.current_track_instrument` now.
        if self.current.has_instrument_column() {
            if let Some(instr) = self.current_track_instrument {
                if instr >= self.module.instrument.len() {
                    // Invalid instrument, cut current note
                    self.cut_pitch();
                    self.drop_live(pool);
                    return false;
                }
            }
        } else {
            // No instrument to load
            return true;
        }

        if self.row_has_tone_portamento_lane() {
            self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_SAMPLE_POSITION, pool);
            return self.tick0_change_instr(true, pool);
        }

        // Dispatch on the cell's event. Mirrors the legacy `cell.note`
        // / `cell.instrument` cross-product:
        // * `InstrReset` ↔ Empty + instr column → ghost trigger +
        //   defaults reload.
        // * `NoteOff { retrig: true }` ↔ KeyOff + instr column → plain
        //   KEEP_PERIOD trigger (the actual key_off fires inside
        //   `tick0_load_pitch`).
        // * `NoteOn { .. }` ↔ Play + instr column → standard
        //   instrument-change path (fall through).
        // * `NoteCut`, `NoteFade` ↔ note column variants with instr
        //   column set: behaviour identical to the legacy
        //   `CellNote::NoteCut` / `CellNote::NoteFade` arms
        //   (no-op load path), see comments below.
        // * `None`, `NoteOnGhost`, `NoteOff { retrig: false }` ↔
        //   no instr column — the early return at the top already
        //   handled them.
        match self.current.event {
            CellEvent::InstrReset => {
                /* Ghost instrument, trigger note */
                let trigger_flags = if self.row_has_volume_slide_lane() {
                    TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_PERIOD
                } else {
                    /* Sample position is kept, but envelopes are reset */
                    TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_PERIOD
                };
                self.trigger_pitch(trigger_flags, pool);
                return self.tick0_change_instr(true, pool);
            }
            CellEvent::NoteOff { retrig: true } => {
                self.trigger_pitch(TRIGGER_KEEP_PERIOD, pool);
                return true; // Keyoff does not change instrument
            }
            CellEvent::NoteFade => {
                // IT note-fade `~~~`: same instrument-handling rule
                // as KeyOff — the fade cell is not a "new note" and
                // must not load a different instrument or retrigger.
                // The actual fadeout work is done in
                // `tick0_load_pitch::note_fade`.
                return true;
            }
            CellEvent::NoteCut | CellEvent::NoteOn { .. } => {
                // Cut and Play both go through the normal path: a
                // Cut row will be handled inside
                // `tick0_change_instr` like an empty note (no
                // instrument change), and Play loads its instrument
                // and pitch.
            }
            // Unreachable in practice (has_instrument_column above
            // already short-circuited these), but kept as a no-op
            // arm so the match is exhaustive without a wildcard.
            CellEvent::None
            | CellEvent::NoteOnGhost { .. }
            | CellEvent::NoteOff { retrig: false } => {}
        }

        self.tick0_change_instr(false, pool)
    }

    fn tick0_load_pitch(&mut self, was_same_instr: bool, pool: &mut VoicePool<'a>) {
        // Extract the actual pitch from the cell's event. Non-trigger
        // variants dispatch their own behaviour and return; only the
        // two `NoteOn` flavours carry on to instrument/pitch loading.
        let pitch = match self.current.event {
            CellEvent::NoteOn { pitch, .. } | CellEvent::NoteOnGhost { pitch, .. } => pitch,
            CellEvent::NoteOff { retrig } => {
                // Mirror the legacy KeyOff branch:
                // * no instr column (`retrig=false`) OR same instr
                //   already held → plain key_off.
                // * instr column present (`retrig=true`) and a
                //   different instrument → KEEP_PERIOD trigger
                //   followed by the channel's natural release.
                if !retrig || was_same_instr {
                    self.key_off(pool);
                } else {
                    self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE, pool);
                }
                return;
            }
            CellEvent::NoteFade => {
                // IT note-column `~~~`: start fadeout without
                // releasing the envelope. Falls through any
                // instrument change because fade has the same
                // semantics whether the instrument cell is set or
                // not — schism doesn't gate on `row_instr` for
                // NOTE_FADE either.
                self.note_fade(pool);
                return;
            }
            CellEvent::NoteCut | CellEvent::None | CellEvent::InstrReset => {
                // NoteCut handled via `TrackEffect::NoteCut`;
                // None means no note event on this row;
                // InstrReset is processed entirely in
                // `tick0_load_instrument` (it never reaches here
                // because the early return above handles the "no
                // instrument to load" path *and* the InstrReset
                // path completes via `tick0_change_instr`).
                return;
            }
        };

        // Instr?
        let has_tone_porta = self.row_has_tone_portamento_lane();
        if let Some(instr) = self.live_mut(pool) {
            // Portamento?
            if has_tone_porta {
                if let Some(s) = &instr.state_sample {
                    if s.is_enabled() {
                        // Same input/output split as in the SetNote
                        // branch below: the porta target's input
                        // note is what `current_note` records for
                        // DCT comparisons, but the period the slide
                        // aims at lives in frequency space, so it
                        // must use the remapped output pitch.
                        // Without this, a Gxx targeting a remapped
                        // key on a drum kit would slide to the
                        // wrong pitch.
                        let played = instr.played_pitch_for(pitch);
                        self.note = compose_played_pitch(played, s.get_finetuned_pitch());
                        self.current_note = Some(pitch);
                        return;
                    }
                }
                self.cut_pitch();
                return;
            }

            // IT ghost-note NNA. When a note column fires without an
            // instrument column, the live voice gets retriggered in
            // place with `TRIGGER_KEEP_VOLUME` — sample reset +
            // envelope reset happens inside `trigger_pitch`. For
            // NNA != Cut we need to snapshot the pre-retrigger state
            // now, before `set_pitch` below calls `select_sample`
            // (which can overwrite `state_sample`). XM/MOD/S3M reach
            // this path with NNA=Cut (the default) so `spawn_ghost_
            // clone` is an inert early return there.
            //
            // Gate on `!has_instrument_column()` so the
            // instrument-change path (which already ran
            // `spawn_ghost_for_outgoing` in `tick0_change_instr`)
            // doesn't double-ghost.
            if !self.current.has_instrument_column() {
                self.spawn_ghost_clone(pool);
            }

            // SetNote
            if let Some(instr) = self.live_mut(pool) {
                if instr.set_pitch(pitch) {
                    if let Some(s) = &instr.state_sample {
                        // Two distinct notes are in play here:
                        //
                        //  - `pitch` is the **input note** the
                        //    pattern asked for. It stays in
                        //    `self.current_note` because that's
                        //    what DCT, NNA and porta-target lookups
                        //    compare against — pressing C-5 and D-5
                        //    on the same drum kit must produce
                        //    distinct voices even if both transpose
                        //    to the same output pitch.
                        //
                        //  - `played_pitch_for(input)` resolves the
                        //    IT keyboard-table remap to the **output
                        //    note** — the pitch the sample is
                        //    actually played at. That's what feeds
                        //    the period / frequency calculation, so
                        //    it goes into `self.note`.
                        //
                        // For non-IT formats, and for IT
                        // instruments without a remap on this key,
                        // `played_pitch_for` returns the input note
                        // unchanged and this matches the previous
                        // behaviour byte-for-byte.
                        let played = instr.played_pitch_for(pitch);
                        self.note = compose_played_pitch(played, s.get_finetuned_pitch());
                    }
                    self.current_note = Some(pitch);

                    let trigger_flag = if self.current.has_instrument_column() {
                        TRIGGER_KEEP_NONE
                    } else {
                        /* Ghost note: keep old volume */
                        TRIGGER_KEEP_VOLUME
                    };
                    self.trigger_pitch(trigger_flag, pool);
                    return;
                }
            }
        }

        self.cut_pitch();
    }

    pub(super) fn tick0_load_instrument_and_pitch(&mut self, pool: &mut VoicePool<'a>) {
        // FT2 "K00 eats note" quirk. In FT2, K00 is handled specially at
        // tickZero via getNewNote (ft2_replayer.c:1418): it fires a keyoff
        // and then RETURNS, never running triggerNote — so the note column
        // of that row is effectively dropped.
        //
        // Key constraints:
        //   - This is canonical FT2/XM behaviour, gated on
        //     `module.quirks.k00_eats_note`. The FT2
        //     `CompatibilityProfile` sets it; modern / IT / S3M /
        //     MOD profiles leave it off.
        //   - Only K00 (param 0) short-circuits. For Kxy with y > 0 FT2
        //     takes the normal getNewNote path and then fires keyOffCmd at
        //     tick y, which means the note DOES play.
        //   - Note-column `===` (`CellNote::KeyOff`) has a different
        //     semantic (see `key_off()`) and belongs on the normal
        //     tick-0 path via `tick0_load_pitch`.
        if self.module.quirks.k00_eats_note && self.current.has_note_off_at_tick_zero() {
            return;
        }

        // First, load instr. `was_same_instr` is true when the instrument
        // slot on this row matches the one currently held by the channel
        // (or when there's no instrument to load — see tick0_load_instrument
        // for the exact contract). This flag drives the keyoff branch in
        // tick0_load_pitch: a keyoff with a *different* instrument must
        // retrigger rather than just cut.
        let was_same_instr: bool = self.tick0_load_instrument(pool);
        // Next, choose sample from note
        self.tick0_load_pitch(was_same_instr, pool);
    }

    pub(crate) fn tick0(
        &mut self,
        pattern_slot: &Cell,
        track_instr: Option<usize>,
        abs_tick: u32,
        song: u16,
        pool: &mut VoicePool<'a>,
    ) {
        // Per-lane loop fold. A Whittaker-style voice loops its own
        // sequence independently of the others; fold the global
        // playhead tick into this lane's loop body so the per-tick
        // automation lane queries (vibrato / slide arm + advance, via
        // `current_abs_tick`) replay each loop. The note content is
        // folded symmetrically in `Module::row_at`. No `ChannelLoop`
        // for this lane — every tracker format — leaves `abs_tick`
        // unchanged.
        let abs_tick = match self.module.channel_loop(song, self.track_index as u8) {
            Some(l) => l.fold_tick(abs_tick),
            None => abs_tick,
        };
        self.current = pattern_slot.clone();
        self.current_track_instrument = track_instr;
        // Cache abs_tick / song for the row's tick run.
        // `tickn_effects` and the per-tick lane queries read
        // these without re-threading from sequencer state.
        self.current_abs_tick = abs_tick;
        self.current_song_idx = song;
        // Row-start tick: per-frame Points lanes read at exactly the
        // row's abs_tick.
        self.current_tick_in_row = 0;
        // Clear the per-tick re-attack flag before any note trigger this
        // row-load tick can set it (see `struck_this_tick`).
        self.struck_this_tick = false;

        let delay = self.current.get_delay();
        // Always write `effect_note_delay` — whether 0 or the row's
        // SDx value. Without this, a delay latched on a previous
        // row would linger and bias downstream code that keys off
        // "is this row delayed" (see `TrackEffect::NoteRetrig`'s
        // pre-delay skip).
        self.effect_note_delay = delay;
        if delay == 0 {
            /* load instrument then note */
            self.tick0_load_instrument_and_pitch(pool);
            if let Some(instr) = self.live_mut(pool) {
                instr.tick();
            }
            // Lane is the source of truth for LFO params
            // (Vibrato/Tremolo/Panbrello). Runs before
            // `tickn_effects` so the cell-side arms (which only
            // drive per-tick advance + quirks now) see the
            // correct `effect_*.data` state.
            self.arm_lfos_from_lanes(abs_tick, song);
            self.tickn_effects(0, pool);

            if self.effect_arpeggio.in_progress() && !self.current.has_arpeggio() {
                // Arpeggio state is row-local (it's always restarted from
                // tick=0 whenever the effect reappears via `tick0`), so a
                // full retrigger is correct here.
                self.effect_arpeggio.retrigger();
            }

            if self.effect_vibrato.in_progress() && !self.row_has_vibrato_lane() {
                // When leaving a Vibrato (main-effect 4xx/6xx) row, clear
                // the lingering period offset so the next row plays at
                // `realPeriod`.
                //
                // Exception: XM vol-column Bx produces a standalone
                // `VibratoDepth` without a `Vibrato`. FT2 keeps the LFO
                // alive in that case (see the VibratoDepth arm above), so
                // we must NOT clear the output — otherwise the vibrato
                // would audibly cut in and out on every row that only
                // carries vol-col B.
                let xm_volcol_b_alone = self.module.quirks.volcol_b_advances_vibrato
                    && self.row_has_vibrato_depth_only_lane();
                if !xm_volcol_b_alone {
                    // Only clear the output modulation.
                    self.effect_vibrato.clear_output();
                }
            }

            self.tickn_update_instr(pool);
        }
        // Delayed row (`delay != 0`): tick0 loads nothing; the delay
        // fires in tickn when current_tick reaches `delay`.
        //
        // Tick ghosts every frame, whether the live voice is present,
        // delayed, or absent. Ghosts are driven purely by their own
        // envelope / fadeout state and must not stall just because
        // the live note is muted or dormant.
        self.tick_ghosts(pool);
    }
}
