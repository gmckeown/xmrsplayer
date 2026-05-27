//! NNA (New Note Action) + DCT (Duplicate Check Type) + past-note
//! effects. Manages the channel's ghost voices spawned when a new
//! note displaces a still-singing voice.

use xmrs::core::fixed::units::Volume;
use xmrs::prelude::*;

use crate::voice::Voice;
use crate::voice_pool::VoicePool;

use super::Channel;

/// Simplified DCT tag, extracted once from the nested
/// [`DuplicateCheckType`] enum so the per-voice match loop can test
/// a plain three-way variant without re-pattern-matching on every
/// iteration.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Dct {
    Note,
    Sample,
    Instrument,
}

impl<'a> Channel<'a> {
    /// Resolve which NNA to apply when a new note displaces the
    /// currently-held voice. `S73`–`S76` override via
    /// `self.nna_override`; otherwise the NNA comes from the
    /// *outgoing* instrument (not the incoming one — IT's replay
    /// reads the old voice's NNA when deciding what happens to it).
    /// Returns `NoteCut` if no voice is currently held (nothing to
    /// ghost).
    pub(super) fn effective_nna(&self, pool: &VoicePool<'a>) -> NewNoteAction {
        if let Some(nna) = self.nna_override {
            return nna;
        }
        let Some(instr) = self.live(pool) else {
            return NewNoteAction::NoteCut;
        };
        match &instr.behavior.duplicate_check {
            DuplicateCheckType::Off(nna) => *nna,
            DuplicateCheckType::Note(dca)
            | DuplicateCheckType::Sample(dca)
            | DuplicateCheckType::Instrument(dca) => match dca {
                DuplicateCheckAction::NoteCut(nna)
                | DuplicateCheckAction::NoteOff(nna)
                | DuplicateCheckAction::NoteFadeOut(nna) => *nna,
            },
        }
    }

    /// Handle the NNA dispatch for a note that's about to be
    /// displaced. Called from the replace-instrument path just
    /// before `self.instr` is overwritten with a fresh voice.
    ///
    /// The fork snapshots the channel-level gain / pan at the
    /// moment of displacement so subsequent tremolo / slides on the
    /// live channel don't perturb the ghost.
    pub(super) fn spawn_ghost_for_outgoing(&mut self, pool: &mut VoicePool<'a>) {
        let nna = self.effective_nna(pool);
        if matches!(nna, NewNoteAction::NoteCut) {
            // Legacy / XM / MOD / S3M path: the fresh note cuts the
            // old one (today's behaviour). No ghost.
            return;
        }
        // Live-voice → ghost promotion. The voice itself stays in
        // its pool slot; only the channel-side bookkeeping changes
        // (live → ghosts list). Before parking, snapshot the
        // channel's current gain / pan / note / period into the
        // voice's frozen-* fields, since the channel won't update
        // them after detachment.
        let Some(id) = self.promote_live_to_ghost() else {
            return;
        };
        if let Some(ghost) = pool.get_mut(id) {
            ghost.vol_frozen = self.volume;
            ghost.channel_volume_frozen = self.channel_volume;
            ghost.panning_frozen = self.panning;
            ghost.note = self.current_note;
            ghost.sample_num = ghost.instr.current_sample_num;
            ghost.period_at_fork = self.period;
            ghost.apply_nna(nna);
        }
    }

    /// Ghost-note variant: clone the live voice into a ghost
    /// WITHOUT taking ownership, because `self.instr` is about to
    /// be retriggered in place (note column carries a fresh pitch
    /// with no accompanying instrument column, so the same
    /// `StateInstrDefault` is reused).
    ///
    /// Called from `tick0_load_pitch` on the note-only path.
    /// Default NNA = Cut → no-op (matches XM/MOD/S3M ghost-note
    /// semantics: retrigger cuts the old sound).
    pub(super) fn spawn_ghost_clone(&mut self, pool: &mut VoicePool<'a>) {
        let nna = self.effective_nna(pool);
        if matches!(nna, NewNoteAction::NoteCut) {
            return;
        }
        // Read the live state via the pool, clone it, drop the
        // borrow, then allocate a fresh ghost holding the clone.
        // Two-step pattern because `pool.allocate` re-borrows the
        // pool mutably and would conflict with the `pool.get`.
        let (cloned, sample_num) = {
            let Some(live) = self.live(pool) else {
                return;
            };
            (live.clone(), live.current_sample_num)
        };
        let mut ghost = Voice::new_ghost(
            cloned,
            self.volume,
            self.channel_volume,
            self.panning,
            self.current_note,
            sample_num,
            self.period,
        );
        ghost.apply_nna(nna);
        self.push_ghost(pool, ghost);
    }

    /// Add a voice to the channel's ghost list. Goes through the
    /// pool's `allocate_ghost` which can refuse the allocation when
    /// every existing voice is sustained-and-audible — in that
    /// case we drop the new ghost on the floor (it would just have
    /// been unaudible noise added to an already-saturated mix), and
    /// the channel keeps playing whatever live note triggered the
    /// NNA. Mirrors schism's `csf_get_nna_channel` returning 0:
    /// the NNA spawn is silently abandoned.
    fn push_ghost(&mut self, pool: &mut VoicePool<'a>, v: Voice<'a>) {
        if let Some(id) = pool.allocate_ghost(v) {
            self.ghosts.push(id);
        }
    }

    /// Advance every ghost's envelopes / fadeout by one tick, and
    /// drop any that have decayed to silence. Voices that go silent
    /// are released back to the pool here, freeing their slot for
    /// future forks.
    pub(super) fn tick_ghosts(&mut self, pool: &mut VoicePool<'a>) {
        // First pass: tick every live handle. Stale handles (from
        // double-release races, which 1.3 shouldn't produce but the
        // pool tolerates) are silently skipped via `get_mut`.
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.tick();
            }
        }
        // Second pass: drop dead voices from the channel's list and
        // release them in the pool. Done as a `retain` over the
        // local list so the order of survivors is preserved.
        self.ghosts.retain(|id| {
            let alive = pool.get(*id).is_some_and(|v| v.is_alive());
            if !alive {
                pool.release(*id);
            }
            alive
        });
    }

    /// S70 / S71 / S72: apply a past-note effect to every ghost on
    /// this channel. The live note is unaffected (S7x targets only
    /// detached voices, per ITTECH).
    fn for_each_ghost(&mut self, pool: &mut VoicePool<'a>, mut f: impl FnMut(&mut Voice<'a>)) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                f(v);
            }
        }
    }
    pub(super) fn past_note_cut_all(&mut self, pool: &mut VoicePool<'a>) {
        self.for_each_ghost(pool, Voice::past_cut);
    }
    pub(super) fn past_note_off_all(&mut self, pool: &mut VoicePool<'a>) {
        self.for_each_ghost(pool, Voice::past_off);
    }
    pub(super) fn past_note_fade_all(&mut self, pool: &mut VoicePool<'a>) {
        self.for_each_ghost(pool, Voice::past_fade_out);
    }

    /// Apply the Duplicate Check Type / Action pair from the
    /// *incoming* instrument's header to existing voices on this
    /// channel (live + ghosts). Called at trigger time, **before**
    /// the NNA ghost-spawn path — so a DCA that cuts / off / fades
    /// a duplicate runs first, and NNA then makes its own decision
    /// about the (now potentially already-affected) outgoing voice.
    ///
    /// The new-note identity tuple `(note, new_instrument_index,
    /// new_sample_index)` is taken from the pattern cell being
    /// processed; it is what DCT tests existing voices against.
    pub(super) fn apply_dct(
        &mut self,
        pool: &mut VoicePool<'a>,
        new_instrument_index: usize,
        new_note: Option<Pitch>,
        new_sample_index: Option<usize>,
    ) {
        // Resolve the incoming instrument's DCT/DCA pair. `Off` is
        // the early-exit (most common) case — no duplicate check.
        let Some(incoming) = self.module.instrument.get(new_instrument_index) else {
            return;
        };
        let InstrumentType::Default(id) = &incoming.instr_type else {
            return;
        };
        let (dct, dca) = match &id.behavior.duplicate_check {
            DuplicateCheckType::Off(_) => return,
            DuplicateCheckType::Note(dca) => (Dct::Note, dca.clone()),
            DuplicateCheckType::Sample(dca) => (Dct::Sample, dca.clone()),
            DuplicateCheckType::Instrument(dca) => (Dct::Instrument, dca.clone()),
        };

        // Voice-identity match against the incoming trigger.
        //
        // DCT::Note matches when BOTH the note identity and the
        // instrument match. Schism (`effects.c:1729`) writes:
        //   apply_dna = (NOTE_IS_NOTE(note)
        //                && (int) p->note == note
        //                && ptr_instrument == p->ptr_instrument);
        // — the instrument check is essential, otherwise two
        // unrelated instruments that happened to play the same
        // note would clobber each other's voices on every
        // retrigger. Pre-1.5 xmrs missed the instrument check
        // entirely and was over-aggressive on this DCT axis.
        //
        // The note compared is the *input* note (pattern column),
        // not a remapped output — drum kits compare on input.
        let dct_matches =
            |instr_num: usize, note: Option<Pitch>, sample_num: Option<usize>| -> bool {
                match dct {
                    Dct::Note => {
                        instr_num == new_instrument_index
                            && matches!((note, new_note), (Some(a), Some(b)) if a == b)
                    }
                    Dct::Sample => {
                        instr_num == new_instrument_index
                            && sample_num == new_sample_index
                            && new_sample_index.is_some()
                    }
                    Dct::Instrument => instr_num == new_instrument_index,
                }
            };

        // --- Live voice check. ---
        // Applying DCA to the live voice is equivalent to the usual
        // note-cut / key-off / fade semantics — just triggered by
        // DCT match rather than by effect column.
        let live_matches = self
            .live(pool)
            .is_some_and(|live| dct_matches(live.num, self.current_note, live.current_sample_num));
        if live_matches {
            match &dca {
                DuplicateCheckAction::NoteCut(_) => {
                    // Match schism's DCA_NOTECUT (effects.c:1742-1745
                    // + the safety net at 1761-1764). Both
                    // channel-side `cut_pitch` AND voice-side
                    // `key_off + cut_pitch + fadeout = 0` are needed:
                    // the channel-only version left the displaced
                    // voice's predicates in their pre-cut state, so
                    // a subsequent ghost promotion captured a
                    // permanently silent `vol_frozen = 0` ghost.
                    self.cut_pitch();
                    if let Some(i) = self.live_mut(pool) {
                        i.key_off();
                        i.cut_pitch();
                        i.volume_fading_out = true;
                        i.volume_fadeout = Volume::SILENT;
                    }
                }
                DuplicateCheckAction::NoteOff(_) => {
                    if let Some(i) = self.live_mut(pool) {
                        i.key_off();
                    }
                }
                DuplicateCheckAction::NoteFadeOut(_) => {
                    if let Some(i) = self.live_mut(pool) {
                        // Schism's DCA_NOTEFADE (`effects.c:1757-1759`)
                        // sets only CHN_NOTEFADE on the displaced
                        // voice — sustain stays held while the
                        // fadeout register decays it.
                        i.start_fadeout();
                    }
                }
            }
        }

        // --- Ghost voices. ---
        for id in &self.ghosts {
            let Some(ghost) = pool.get_mut(*id) else {
                continue;
            };
            if dct_matches(ghost.instr.num, ghost.note, ghost.sample_num) {
                match &dca {
                    DuplicateCheckAction::NoteCut(_) => ghost.past_cut(),
                    DuplicateCheckAction::NoteOff(_) => ghost.past_off(),
                    DuplicateCheckAction::NoteFadeOut(_) => ghost.past_fade_out(),
                }
            }
        }
    }
}
