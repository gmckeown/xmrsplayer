//! A `Voice` is an instance of an instrument actively producing
//! sound. The same type represents both the **live** voice that the
//! track is currently driving via the pattern (note triggers,
//! tremolo, slides, vibrato) and the **detached** voices spawned by
//! IT's `NewNoteAction` mechanism, which keep ringing on their own
//! envelope + fadeout after a new note has displaced them.
//!
//! Both kinds live in the [`crate::voice_pool::VoicePool`]; channels
//! hold only handles. [`Voice::new_live`] builds the live case
//! (channel pattern column drives it), [`Voice::new_ghost`] builds
//! the detached case (envelope continues, channel-level gain /
//! pan / channel-volume captured at detach time). Voices self-
//! terminate when their envelope × fadeout product reaches silence.

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

// (1.5) The former `MAX_GHOST_VOICES_PER_CHANNEL` cap has been
// removed. Per-channel ghost lists now grow freely; the only limit
// is the shared pool's capacity (see `voice_pool::DEFAULT_VOICE_
// POOL_CAPACITY`). When the pool is full, allocation steals the
// lowest-audible-volume voice across the whole population, the way
// schismtracker's `csf_get_nna_channel` does. This eliminates the
// pathological case where a busy channel evicted its own quiet
// ghosts under FIFO while loud ghosts on idle channels survived.

/// A voice owned by the `VoicePool` (live or detached).
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

    /// The track that engendered this voice. Used by past-note
    /// effects (S7x) and DCT matching to find voices belonging to
    /// a given track, and by ghost-list maintenance routines.
    /// `Some(track_index)` for both live and ghost voices.
    #[allow(dead_code)]
    pub master_track: Option<usize>,
}

impl<'a> Voice<'a> {
    /// Build a live voice (driven by a track's pattern column).
    /// Used by `Channel::install_live` when a fresh trigger needs
    /// a slot in the pool. The frozen-gain fields are populated to
    /// neutral defaults; the channel applies its own `actual_volume`
    /// in the mixer, so these are unread for live voices but kept
    /// at sensible values in case the voice is later promoted to a
    /// ghost without an explicit re-snapshot.
    pub fn new_live(instr: StateInstrDefault<'a>, master_track: usize) -> Self {
        Self {
            instr,
            vol_frozen: 1.0,
            channel_volume_frozen: 1.0,
            panning_frozen: 0.5,
            note: None,
            sample_num: None,
            period_at_fork: 0.0,
            master_track: Some(master_track),
        }
    }

    /// Build a detached (ghost) voice from a live instrument state.
    /// Callers must then dispatch the NNA-specific action on the
    /// returned voice — see [`Voice::apply_nna_continue`] (no-op),
    /// [`apply_nna_note_off`], [`apply_nna_note_fade_out`].
    /// Apply-functions live on this type so the NNA dispatch table
    /// is co-located with the voice it affects.
    ///
    /// `master_track` records which pattern track engendered the
    /// voice. For ghosts spawned by NNA != Cut, that's the track
    /// whose live note was just displaced.
    ///
    /// `note` / `sample_num` carry the identity tags used by DCT
    /// matching on subsequent triggers.
    pub fn new_ghost(
        instr: StateInstrDefault<'a>,
        master_track: usize,
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
            master_track: Some(master_track),
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

    /// NNA = NoteFadeOut: engage the fadeout register without
    /// releasing sustain. Mirrors schism's `NNA_NOTEFADE` branch
    /// in `csf_check_nna` (`effects.c:1791-1792`), which sets
    /// `CHN_NOTEFADE` but NOT `CHN_KEYOFF`. The envelope keeps
    /// wrapping in its sustain loop while the fadeout register
    /// decays the voice to silence — that's how IT's NNA Fade
    /// differs from NNA NoteOff, which DOES release sustain so
    /// the envelope walks through its release section before the
    /// fade engages.
    ///
    /// The previous implementation called `instr.key_off()` here,
    /// conflating the two NNAs and making every NNA Fade ghost
    /// behave like an NNA NoteOff ghost.
    #[inline]
    pub fn apply_nna_note_fade_out(&mut self) {
        self.instr.start_fadeout();
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

    /// S72 past-note: start fadeout. Mirrors schism's `case 2` of
    /// the S70 dispatch in `effects.c:734-735`: only `CHN_NOTEFADE`
    /// gets set, sustain stays held. Distinct from `past_off` (S71)
    /// which routes through `fx_key_off`.
    #[inline]
    pub fn past_fade_out(&mut self) {
        self.instr.start_fadeout();
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

    /// `true` while the voice still owns its pool slot. A voice is
    /// considered "dead" — and therefore reclaimable — only when:
    ///
    ///   1. its sample stream has run out (`state_sample.step` is
    ///      `None`, surfaced as `!instr.is_enabled()`), OR
    ///   2. it is in its fadeout phase (`volume_fading_out`) AND its
    ///      monotonic fadeout register has reached zero.
    ///
    /// The current envelope value deliberately does NOT participate.
    /// IT envelopes legitimately pass through zero — V-shapes,
    /// sustain points authored at zero for swells, release segments
    /// that cross zero — and a sustained NNA = Continue ghost must
    /// keep its slot through those silent moments. Schism's
    /// `sndmix.c:1112` codifies the same policy:
    ///
    /// ```c
    /// if ((chan->flags & CHN_NOTEFADE) &&
    ///     !(chan->fadeout_volume | chan->right_volume | chan->left_volume))
    ///     chan->length = 0;
    /// ```
    ///
    /// `volume_fading_out` is the xmrsplayer equivalent of
    /// `CHN_NOTEFADE`; it's set independently from `sustained`,
    /// which lets a voice be "still sustained, but already fading"
    /// (the NNA = NoteFade case — `apply_nna_note_fade_out` calls
    /// `start_fadeout` without releasing sustain). A predicate
    /// based on `sustained || volume_fadeout > 0.0` would have
    /// missed that case and held the voice alive forever after
    /// its fadeout register reached zero.
    #[inline]
    pub fn is_alive(&self) -> bool {
        if !self.instr.is_enabled() {
            return false;
        }
        // Only voices that are explicitly fading AND have run their
        // fadeout register to zero are dead. Everything else — held
        // sustain, in-release with no fade armed yet, mid-fade —
        // stays alive.
        !(self.instr.volume_fading_out && self.instr.volume_fadeout <= 0.0)
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
        //   actual_left  = v * (1 - panning)
        //   actual_right = v * panning
        // with the channel-side terms (`volume`, `channel_volume`,
        // `panning`) captured at detachment time. `instr.get_volume()`
        // continues to evolve because that's the fadeout / envelope
        // side of the gain — which IS what should still be moving on
        // a ghost voice.
        let v = self.vol_frozen * self.instr.get_volume() * self.channel_volume_frozen;
        let v = v.clamp(0.0, 1.0);
        let l_gain = v * (1.0 - self.panning_frozen);
        let r_gain = v * self.panning_frozen;

        match self.instr.next() {
            Some((l, r)) => Some((l * l_gain, r * r_gain)),
            None => None,
        }
    }
}
