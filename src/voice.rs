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

use crate::state_instr_default::StateInstrDefault;
use crate::voice_mod::VoiceMod;
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{Amp, ChannelVolume, Panning, Period, PitchDelta, Volume};
use xmrs::core::instrument_behavior::NewNoteAction;
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
    pub vol_frozen: Volume,
    /// Channel-level `self.channel_volume` at the moment of
    /// detachment. Same rationale as `vol_frozen`.
    pub channel_volume_frozen: ChannelVolume,
    /// Panning at the moment of detachment.
    pub panning_frozen: Panning,

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
    pub period_at_fork: Period,

    /// RFC §2B — per-tick stereo gain when this voice is driven by a
    /// `ModScope::Voice` lane. `Some` means [`Voice::tick_scoped`] ran
    /// the channel modulation bus through this voice's own base this
    /// tick (overriding the frozen-gain path in [`Voice::next`]);
    /// `None` (the default, and always so for live voices and
    /// `Track`-scoped ghosts) keeps the historical behaviour.
    pub scoped_volume: Option<[Volume; 2]>,
}

impl<'a> Voice<'a> {
    /// Build a live voice (driven by a track's pattern column).
    /// Used by `Channel::install_live` when a fresh trigger needs
    /// a slot in the pool. The frozen-gain fields are populated to
    /// neutral defaults; the channel applies its own `actual_volume`
    /// in the mixer, so these are unread for live voices but kept
    /// at sensible values in case the voice is later promoted to a
    /// ghost without an explicit re-snapshot.
    pub fn new_live(instr: StateInstrDefault<'a>) -> Self {
        Self {
            instr,
            vol_frozen: Volume::FULL,
            channel_volume_frozen: ChannelVolume::FULL,
            panning_frozen: Panning::CENTER,
            note: None,
            sample_num: None,
            period_at_fork: Period::ZERO,
            scoped_volume: None,
        }
    }

    /// Build a detached (ghost) voice from a live instrument state.
    /// Callers must then dispatch the NNA-specific action on the
    /// returned voice via [`Voice::apply_nna`].
    ///
    /// `note` / `sample_num` carry the identity tags used by DCT
    /// matching on subsequent triggers.
    #[allow(clippy::too_many_arguments)]
    pub fn new_ghost(
        instr: StateInstrDefault<'a>,
        vol_frozen: Volume,
        channel_volume_frozen: ChannelVolume,
        panning_frozen: Panning,
        note: Option<Pitch>,
        sample_num: Option<usize>,
        period_at_fork: Period,
    ) -> Self {
        Self {
            instr,
            vol_frozen,
            channel_volume_frozen,
            panning_frozen,
            note,
            sample_num,
            period_at_fork,
            scoped_volume: None,
        }
    }

    /// Dispatch on an NNA variant. Callers must early-return on
    /// `NoteCut` (which short-circuits ghost spawning entirely),
    /// so reaching the `NoteCut` arm here is a programmer error.
    ///
    /// * `Continue` — no-op.
    /// * `NoteOff` — release sustain (`instr.key_off()`); the
    ///   envelope walks its release segment, then the fade engages.
    /// * `NoteFadeOut` — engage the fadeout register without
    ///   releasing sustain. Mirrors schism's `NNA_NOTEFADE` branch
    ///   in `csf_check_nna` (`effects.c:1791-1792`), which sets
    ///   `CHN_NOTEFADE` but NOT `CHN_KEYOFF` — the envelope keeps
    ///   wrapping in its sustain loop while the fadeout decays the
    ///   voice to silence.
    #[inline]
    pub fn apply_nna(&mut self, nna: NewNoteAction) {
        match nna {
            NewNoteAction::Continue => {}
            NewNoteAction::NoteOff => self.instr.key_off(),
            NewNoteAction::NoteFadeOut => self.instr.start_fadeout(),
            NewNoteAction::NoteCut => unreachable!("NoteCut is handled by the caller"),
        }
    }

    /// S7x past-note: immediate cut.
    ///
    /// Mirrors schism's S70 dispatch in `effects.c:737-738`:
    ///
    /// ```c
    /// bkp->flags |= CHN_NOTEFADE;
    /// bkp->fadeout_volume = 0;
    /// ```
    ///
    /// Engaging both `volume_fading_out` (= `CHN_NOTEFADE`) AND
    /// `volume_fadeout = 0.0` is what makes [`Self::is_alive`] flip to
    /// `false` on the next `tick_ghosts` pass, so the channel's ghost
    /// list reclaims the slot. The pre-fix version only zeroed
    /// `instr.volume` via `cut_pitch`, which silenced the voice but
    /// left every other predicate (`is_enabled`, `volume_fading_out`)
    /// untouched — the ghost stayed alive forever in `self.ghosts`,
    /// still consuming a `pool.get_mut` + sample-mix on every output
    /// sample. Modules using S70 (or DCA NoteCut on a ghost) saw CPU
    /// usage climb monotonically over the course of a song until the
    /// pool capped at 256 voices.
    ///
    /// `cut_pitch` is kept on the call path so `instr.get_volume()`
    /// returns 0 in the brief window between this call and the next
    /// `tick_ghosts` reap — avoiding the residual click that would
    /// otherwise leak through the fadeout-decay path on instruments
    /// with a non-trivial fadeout register value.
    #[inline]
    pub fn past_cut(&mut self) {
        self.instr.cut_pitch();
        self.instr.volume_fading_out = true;
        self.instr.volume_fadeout = Volume::SILENT;
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
        // Default (`ModScope::Track`) path: no channel modulation
        // reaches this ghost, so clear any scoped gain a previous
        // `Voice`-scoped tick left behind — `next` falls back to the
        // frozen-gain chain. A no-op for every tracker render (the
        // scope is always `Track`, so this is always already `None`).
        self.scoped_volume = None;
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
        //
        // The returned effective period is only consumed by a live
        // `Channel`'s snapshot; a detached ghost has no snapshot of its
        // own, so discard it here.
        let _ = self.instr.update_frequency(
            self.period_at_fork,
            PitchDelta::ZERO,
            0i16,
            PitchDelta::ZERO,
            false,
        );
    }

    /// RFC §2B (`ModScope::Voice`) — advance this ghost like
    /// [`Voice::tick`], but fold the channel's per-tick modulation bus
    /// into it instead of leaving it frozen. The voice keeps its **own
    /// base** (`period_at_fork`, frozen gains, and its own envelopes)
    /// and receives the channel's **deltas** (vibrato / tremolo /
    /// panbrello / arpeggio / Points transpose). The resulting stereo
    /// gain is stashed in [`Self::scoped_volume`] for [`Self::next`];
    /// the per-sample step is set by `apply_voice_mod` itself.
    #[inline]
    pub fn tick_scoped(&mut self, channel: &VoiceMod) {
        self.instr.tick();
        let m = VoiceMod {
            // Base — this voice's own, not the live note's.
            period: self.period_at_fork,
            volume: self.vol_frozen,
            panning: self.panning_frozen,
            channel_volume: self.channel_volume_frozen,
            // Deltas — the channel's current modulation.
            arp_pitch: channel.arp_pitch,
            vibrato_raw: channel.vibrato_raw,
            points_pitch: channel.points_pitch,
            semitone: channel.semitone,
            tremolo: channel.tremolo,
            panbrello: channel.panbrello,
            tremor: channel.tremor,
        };
        let (actual_volume, _effective_period) = self.instr.apply_voice_mod(&m);
        self.scoped_volume = Some(actual_volume);
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
    /// (the NNA = NoteFade case — `apply_nna` calls
    /// `start_fadeout` without releasing sustain). A predicate
    /// based on `sustained || volume_fadeout > 0.0` would have
    /// missed that case and held the voice alive forever after
    /// its fadeout register reached zero.
    #[inline]
    pub fn is_alive(&self) -> bool {
        if !self.instr.is_enabled() {
            return false;
        }
        // Schism's reap condition is `CHN_NOTEFADE && (fadeout|left|right) == 0`.
        // We have the fadeout-register half here; the envelope-driven half
        // covers the case where the volume envelope walks past its last
        // point AND that last point is zero — at which point the voice's
        // contribution is permanently silent regardless of the fadeout
        // register, but the existing predicate would keep it alive forever
        // on instruments authored with `fadeout = 0` and NNA != Cut
        // (a legal IT combination — see myheart.it inst 1 / 7).
        if self.instr.volume_fading_out {
            // Half 1: explicit fade ran the register to zero.
            if self.instr.volume_fadeout.raw() == Q15::ZERO {
                return false;
            }
            // Half 2: envelope settled at zero past its last point.
            if self.instr.envelope_volume.is_past_last_point()
                && self.instr.envelope_volume.value.is_silent()
            {
                return false;
            }
        }
        true
    }

    /// Produce one stereo sample. Mirrors the gain formula used in
    /// `Channel::tickn_update_instr` + `Channel::next` but with the
    /// frozen pattern-side gains. Returns `None` when the underlying
    /// sample has reached end-of-stream OR when the voice is silent
    /// and awaiting reap.
    pub fn next(&mut self) -> Option<(Amp, Amp)> {
        if !self.instr.is_enabled() {
            return None;
        }
        if self.instr.volume_fading_out && self.instr.volume_fadeout.raw() == Q15::ZERO {
            return None;
        }
        // RFC §2B: a `ModScope::Voice` ghost had its gain computed for
        // this tick by `tick_scoped` (channel bus folded onto its own
        // base) — use it, matching how the live voice's `actual_volume`
        // is computed once per tick and applied every sample. Otherwise
        // (the default `Track` path) take the frozen-gain chain: three
        // saturating Q1.15 mults composing `vol_frozen ×
        // channel_volume_frozen × instr.get_volume()`, then a stereo
        // split by `panning_frozen`.
        let (l_gain, r_gain) = match self.scoped_volume {
            Some([l, r]) => (l, r),
            None => {
                let v_q = self
                    .channel_volume_frozen
                    .applied_to(self.vol_frozen)
                    .scaled_by(self.instr.get_volume());
                let (pan_left, pan_right) = self.panning_frozen.stereo_split_linear();
                (v_q.scaled_by(pan_left), v_q.scaled_by(pan_right))
            }
        };

        self.instr
            .next()
            .map(|(l, r)| (l_gain.apply(l), r_gain.apply(r)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state_instr_default::StateInstrDefault;
    use alloc::boxed::Box;
    use xmrs::prelude::*;

    /// Minimal ghost over a default (sample-less) instrument. Suitable
    /// for tests that probe the per-voice flag bookkeeping; the sample
    /// path stays disabled (`is_enabled() == false`), which is fine
    /// for everything except behavioural tests that need actual audio.
    fn fresh_ghost() -> Voice<'static> {
        let instr: &'static InstrDefault = Box::leak(Box::new(InstrDefault::default()));
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        // `true` mirrors the historical unconditional `× self.volume`
        // and keeps these NNA-bookkeeping tests behaviour-stable.
        let state = StateInstrDefault::new(instr, 0, ph, SampleRate::from_hz(44100), true);
        Voice::new_ghost(
            state,
            Volume::FULL,
            ChannelVolume::FULL,
            Panning::CENTER,
            None,
            None,
            Period::ZERO,
        )
    }

    #[test]
    fn past_cut_engages_fadeout_for_immediate_reap() {
        // Schism's S70 path (effects.c:737-738) sets BOTH
        // `CHN_NOTEFADE` and `fadeout_volume = 0` so the next render
        // pass kills the voice via the sndmix.c:1112-1118 check.
        // The xmrsplayer equivalent is `volume_fading_out` plus
        // `volume_fadeout = 0`, which together flip `is_alive()` to
        // false on the next `tick_ghosts` pass.
        //
        // Pre-fix `past_cut` only zeroed `instr.volume`, leaving the
        // fadeout flags at their initial `false` / `1.0` values; the
        // ghost stayed in the channel's list forever and CPU usage
        // climbed proportionally to the number of S70/DCA-cut events
        // over the course of a song.
        let mut v = fresh_ghost();
        assert!(!v.instr.volume_fading_out);
        assert_eq!(v.instr.volume_fadeout, Volume::FULL);
        assert_eq!(v.instr.volume, Volume::FULL);

        v.past_cut();

        assert_eq!(v.instr.volume, Volume::SILENT, "instrument volume zeroed");
        assert!(
            v.instr.volume_fading_out,
            "fadeout flag engaged (= CHN_NOTEFADE)"
        );
        assert_eq!(
            v.instr.volume_fadeout,
            Volume::SILENT,
            "fadeout register zeroed (= fadeout_volume = 0)"
        );
    }
}
