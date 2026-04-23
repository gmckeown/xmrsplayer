//! A `Voice` is a still-sounding note that the `Channel` has spawned
//! and then let go of. In MOD/XM/S3M a channel has exactly one live
//! voice at a time — a fresh note cuts the old one. In Impulse Tracker
//! the instrument's `NewNoteAction` decides what happens to the old
//! sound when a new note fires on the same channel: `Cut` matches the
//! legacy behaviour, while `Continue`, `NoteOff` and `NoteFadeOut` all
//! leave the old voice running alongside the fresh one, fading in
//! their own way.
//!
//! This module implements the "ghost" side of that story. The live
//! (pattern-controlled) note still lives in `Channel::instr` — ghosts
//! are accumulated into `Channel::ghosts` and ticked / mixed alongside
//! it every frame. Each `Voice` owns an independent copy of the
//! instrument's `StateInstrDefault` (envelopes, fadeout, sample
//! position) plus the channel-level gain / pan that were in effect at
//! the moment of the fork. Voices do not receive further pattern
//! effects once detached; they simply run to silence via their
//! envelope + fadeout state and then self-terminate.
//!
//! This is a deliberate mid-point between the current single-voice-
//! per-channel architecture and the fully-separated `Voices` pool
//! sketched in `IT_ROADMAP.md`. A real pool with DCT / voice-stealing
//! is Phase 2/3 of that roadmap; what we do here is Phase 3's audible
//! behaviour (NNA + S7x past-note effects) without the full
//! architectural refactor. The API on `Voice` is shaped so that
//! promoting these to a flat pool later is mechanical.

// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

use crate::state_instr_default::StateInstrDefault;
use xmrs::prelude::Pitch;

/// The maximum number of concurrent ghost voices per pattern channel.
///
/// Impulse Tracker itself allocates voices from a global 64-deep pool
/// shared across all channels. In this simpler per-channel model we
/// cap each channel at 8 — enough for even aggressive NNA=Continue
/// tremolo stabs, tight enough to keep the `Vec<Voice>` hot path
/// bounded. When the cap is reached the oldest (front-most) ghost is
/// evicted; that is the conventional voice-steal heuristic (see
/// `Channel::push_ghost`).
pub(crate) const MAX_GHOST_VOICES_PER_CHANNEL: usize = 8;

/// A detached, still-sounding note.
#[derive(Clone)]
pub(crate) struct Voice<'a> {
    /// The instrument-state fork. Owns its own envelopes, fadeout
    /// counter, sample cursor and auto-vibrato.
    pub instr: StateInstrDefault<'a>,

    /// Channel-level `self.volume` at the moment of detachment. The
    /// live channel continues to update its own `self.volume`
    /// (tremolo, slides); ghosts do not. Freezing this value means
    /// the ghost's amplitude only changes through its envelope /
    /// fadeout, which is exactly what IT's "the note runs to its
    /// envelope end on its own" model calls for.
    pub vol_frozen: f32,
    /// Channel-level `self.channel_volume` at the moment of
    /// detachment. Same rationale as `vol_frozen`.
    pub channel_volume_frozen: f32,
    /// Panning at the moment of detachment.
    pub panning_frozen: f32,

    /// The note (integer semitone, ignoring finetune) this voice
    /// was triggered on. Used by DCT::Note matching. `None` when
    /// the originating trigger was a keyoff / cut / invalid note —
    /// such voices never match a DCT::Note check.
    pub note: Option<Pitch>,
    /// The sample index within the current instrument that this
    /// voice is playing. Used by DCT::Sample matching. `None` when
    /// no sample is currently selected.
    pub sample_num: Option<usize>,

    /// The channel's `period` value at the moment of detachment.
    /// Used by [`Voice::tick`] to re-run `update_frequency` each
    /// tick with the current pitch-envelope contribution folded in
    /// — so a ghost voice with `NNA = Continue` and an active pitch
    /// envelope gets the right pitch modulation even though the
    /// envelope's live channel has moved on. Without this,
    /// envelope-driven pitch drift stops at the moment the voice
    /// becomes a ghost.
    pub period_at_fork: f32,
}

impl<'a> Voice<'a> {
    /// Fork a live instrument state into a ghost voice. Callers must
    /// then dispatch the NNA-specific action on the returned voice —
    /// see [`Voice::apply_nna_continue`] (no-op), [`apply_nna_note_off`],
    /// [`apply_nna_note_fade_out`]. Apply-functions live on this type
    /// so the NNA dispatch table is co-located with the voice it
    /// affects.
    ///
    /// `note` / `sample_num` carry the identity tags used by DCT
    /// matching on subsequent triggers.
    pub fn fork(
        instr: StateInstrDefault<'a>,
        vol_frozen: f32,
        channel_volume_frozen: f32,
        panning_frozen: f32,
        note: Option<Pitch>,
        sample_num: Option<usize>,
        period_at_fork: f32,
    ) -> Self {
        Self {
            instr,
            vol_frozen,
            channel_volume_frozen,
            panning_frozen,
            note,
            sample_num,
            period_at_fork,
        }
    }

    /// NNA = Continue. Explicit no-op kept for symmetry with the
    /// other NNA arms so the dispatch site reads straightforwardly.
    #[inline]
    pub fn apply_nna_continue(&mut self) {}

    /// NNA = NoteOff (enters release segment of the volume envelope;
    /// if the instrument has no envelope, the internal `key_off`
    /// cuts the voice).
    #[inline]
    pub fn apply_nna_note_off(&mut self) {
        self.instr.key_off();
    }

    /// NNA = NoteFadeOut (unsustain so `volume_fadeout` begins
    /// ticking down to zero on each envelope update).
    #[inline]
    pub fn apply_nna_note_fade_out(&mut self) {
        // `key_off` both clears the sustained flag AND enters the
        // release segment. For a pure fade-out we just want
        // `sustained = false` so the fadeout counter ticks; the
        // envelope's release behaviour is a subset of that. Reusing
        // `key_off` is the simplest path and produces the right
        // audible shape on instruments whose envelope has no loop.
        self.instr.key_off();
    }

    /// S7x past-note: immediate cut.
    #[inline]
    pub fn past_cut(&mut self) {
        self.instr.cut_pitch();
    }

    /// S7x past-note: key-off (release envelope).
    #[inline]
    pub fn past_off(&mut self) {
        self.instr.key_off();
    }

    /// S7x past-note: start fadeout.
    #[inline]
    pub fn past_fade_out(&mut self) {
        self.instr.key_off();
    }

    /// Advance envelopes / fadeout / auto-vibrato by one tick. Called
    /// from both `Channel::tick0` and `Channel::tick` so ghost voices
    /// progress at the same cadence the live channel's envelopes do.
    #[inline]
    pub fn tick(&mut self) {
        self.instr.tick();
        // Re-run frequency computation with the stored period so
        // the pitch envelope's contribution reaches the sample step
        // each tick. Without this the envelope would tick internally
        // but the voice's playback rate would stay frozen at fork.
        // `arp_pitch = 0` / `finetune = 0` / `semitone = false`:
        // arpeggio and the channel's vibrato belong to the live
        // voice, not to detached ghosts — those stop at detachment.
        // Pitch envelope is folded in by `update_frequency` itself
        // via `get_pitch_envelope_offset_semitones`.
        self.instr
            .update_frequency(self.period_at_fork, 0.0, 0.0, false);
    }

    /// `true` while the voice is still audible. A voice is considered
    /// "dead" when either its sample stream has run out (no enabled
    /// state_sample) or the combined envelope*fadeout*inner-volume
    /// product has fallen to (effectively) zero — at which point
    /// further samples would be silent anyway, so the caller can
    /// drop this voice and reclaim the slot.
    #[inline]
    pub fn is_alive(&self) -> bool {
        if !self.instr.is_enabled() {
            return false;
        }
        self.instr.get_volume() > 1e-6
    }

    /// Produce one stereo sample. Mirrors the gain formula used in
    /// `Channel::tickn_update_instr` + `Channel::next` but with the
    /// frozen pattern-side gains. Returns `None` when the underlying
    /// sample has reached end-of-stream.
    pub fn next(&mut self) -> Option<(f32, f32)> {
        if !self.instr.is_enabled() {
            return None;
        }
        // Mirror the live-channel formula:
        //   v = volume * instr.get_volume() * channel_volume
        //   actual_left  = v * sqrt(1 - panning)
        //   actual_right = v * sqrt(panning)
        // with the channel-side terms (`volume`, `channel_volume`,
        // `panning`) captured at detachment time. `instr.get_volume()`
        // continues to evolve because that's the fadeout / envelope
        // side of the gain — which IS what should still be moving on
        // a ghost voice.
        let v = self.vol_frozen * self.instr.get_volume() * self.channel_volume_frozen;
        let v = v.clamp(0.0, 1.0);
        let l_gain = v * (1.0 - self.panning_frozen).sqrt();
        let r_gain = v * self.panning_frozen.sqrt();

        match self.instr.next() {
            Some((l, r)) => Some((l * l_gain, r * r_gain)),
            None => None,
        }
    }
}
