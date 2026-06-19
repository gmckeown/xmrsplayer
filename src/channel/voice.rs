//! Per-channel voice management: live/ghost handles into the
//! shared [`VoicePool`], install / drop / promote, key-off and
//! note-fade primitives.

use xmrs::core::fixed::units::Volume;

use crate::state_instr_default::StateInstrDefault;
use crate::voice::Voice;
use crate::voice_pool::{VoiceId, VoicePool};

use super::Channel;

impl<'a> Channel<'a> {
    pub(super) fn live<'p>(&self, pool: &'p VoicePool<'a>) -> Option<&'p StateInstrDefault<'a>> {
        self.live.and_then(|id| pool.get(id)).map(|v| &v.instr)
    }

    pub(super) fn live_mut<'p>(
        &self,
        pool: &'p mut VoicePool<'a>,
    ) -> Option<&'p mut StateInstrDefault<'a>> {
        self.live
            .and_then(|id| pool.get_mut(id))
            .map(|v| &mut v.instr)
    }

    pub(super) fn drop_live(&mut self, pool: &mut VoicePool<'a>) {
        if let Some(id) = self.live.take() {
            pool.release(id);
        }
    }

    /// Move the live voice into the channel's ghost list without
    /// reallocating in the pool — the slot just changes role from
    /// "live" to "detached". Returns the moved `VoiceId` so the
    /// caller can apply NNA. The ghost list grows unboundedly per
    /// channel (1.5+); the shared pool's capacity is the only cap.
    pub(super) fn promote_live_to_ghost(&mut self) -> Option<VoiceId> {
        let id = self.live.take()?;
        self.ghosts.push(id);
        Some(id)
    }

    /// Allocate a fresh live voice in the pool. Releases any
    /// previously-held live id (the caller should already have
    /// promoted it to a ghost if NNA != Cut). Caches the
    /// instrument's midi_mute_computer so `is_muted` stays
    /// pool-free.
    pub(super) fn install_live(
        &mut self,
        pool: &mut VoicePool<'a>,
        state: StateInstrDefault<'a>,
        midi_mute: bool,
    ) {
        if let Some(old) = self.live.take() {
            pool.release(old);
        }
        let voice = Voice::new_live(state);
        self.live = Some(pool.allocate_live(voice));
        self.instr_midi_mute = midi_mute;
    }

    pub(super) fn cut_pitch(&mut self) {
        /* NB: this is not the same as Key Off */
        self.volume = Volume::SILENT;
    }

    /// Release the currently-sustaining note.
    ///
    /// Behaviour is driven by
    /// `module.quirks.keyoff_cuts_without_vol_env`,
    /// which the importers preset per format because "how a
    /// note-off should sound" is a format-level semantic, not
    /// an FT2 bug:
    ///
    /// * Quirk **on** (XM via `profiles::ft2()`) —
    ///   reproduce `ft2_replayer.c:keyOff()`: if the instrument
    ///   has a volume envelope, flip `sustained` so the envelope
    ///   enters its release segment and `volume_fadeout` starts
    ///   counting down; otherwise CUT the channel volume to zero
    ///   immediately. The FT2 source branches identically on
    ///   `volEnvFlags & ENV_ENABLED`.
    ///
    /// * Quirk **off** (IT / MOD / S3M / modern) — rely on
    ///   `StateInstrDefault::key_off()` which flips
    ///   `sustained = false` (and cuts iff the instrument has
    ///   no envelope AND zero fadeout). This is the IT release
    ///   model and the sensible fallback elsewhere.
    ///
    /// FT2 itself also ignores which tick the key-off lands on —
    /// the branch on envelope is the only real decision.
    pub(super) fn key_off(&mut self, pool: &mut VoicePool<'a>) {
        let Some(i) = self.live_mut(pool) else {
            self.cut_pitch();
            return;
        };

        let had_vol_env = i.has_volume_envelope();
        i.key_off();

        // FT2's `keyOff()` cuts `realVol` / `outVol` to zero
        // (quick-ramped, anti-click) when the instrument has no
        // volume envelope. This isn't an FT2 bug — it's how XM
        // modules are meant to sound — so it lives on
        // `keyoff_cuts_without_vol_env`, which the FT2 profile
        // sets and others leave off.
        if self.module.quirks.keyoff_cuts_without_vol_env && !had_vol_env {
            self.cut_pitch();
        }
    }

    /// IT-only "note fade" (`~~~` in the pattern, raw byte 246).
    /// Distinct from `key_off`: a fade engages the volume-fadeout
    /// register without releasing sustain, so the envelope keeps
    /// wrapping in its sustain loop while the fadeout decays the
    /// voice. Matches schism's NOTE_FADE handling (`effects.c`
    /// fold of bytes 120..=252 into the fade path), which does NOT
    /// route through `fx_key_off`.
    pub(super) fn note_fade(&mut self, pool: &mut VoicePool<'a>) {
        let Some(i) = self.live_mut(pool) else {
            self.cut_pitch();
            return;
        };
        i.start_fadeout();
    }

    /// Drop every ghost voice on this channel. Used by the facade
    /// when the song is seeked (goto) — stale ghosts from the
    /// previous playback position must not bleed into the new one.
    pub(crate) fn clear_ghosts(&mut self, pool: &mut VoicePool<'a>) {
        for id in self.ghosts.drain(..) {
            pool.release(id);
        }
        self.nna_override = None;
    }
}
