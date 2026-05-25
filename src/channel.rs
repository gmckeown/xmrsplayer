use xmrs::core::waveform::WaveformState;

// Required for `Vec` under `no_std` (the `std` prelude
// otherwise provides it). Other places in this file already use
// the fully-qualified `alloc::vec::Vec` path, but `ghosts:
// Vec<VoiceId>` in the channel struct definition needs the type
// in scope.
use alloc::vec::Vec;

use crate::effect_arpeggio::EffectArpeggio;
use crate::effect_vibrato_tremolo::EffectVibratoTremolo;
use crate::triggerkeep::*;

// `helper::*` import removed: all f32 slide / lerp helpers
// have been replaced by Q-typed `Period::saturating_add_signed`,
// `Period::clamp`, `Period::slide_towards`, etc.
use crate::midi_observer::MidiEvent;
use crate::state_instr_default::StateInstrDefault;
use crate::voice::Voice;
use crate::voice_pool::{VoiceId, VoicePool};
use xmrs::core::fixed::fixed::{Q15, Q8_8};
use xmrs::core::fixed::units::Pitch as PitchQ;
use xmrs::core::fixed::units::{
    Amp, ChannelVolume, Panning, Period, PitchDelta, SampleRate, Volume,
};
use xmrs::prelude::*;

/// Simplified DCT tag, extracted once from the nested
/// [`DuplicateCheckType`] enum so the per-voice match loop can test
/// a plain three-way variant without re-pattern-matching on every
/// iteration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dct {
    Note,
    Sample,
    Instrument,
}

#[derive(Clone, PartialEq, Default)]
struct NoteRetrigState {
    event: CellEvent,
    /// Track-level instrument of the cell that armed this retrig.
    /// DAW migration Phase 1+2 moved the per-cell instrument off
    /// `Cell`, so the retrig identity now tracks both the typed
    /// event (carries pitch + velocity + ghost/fresh distinction) and
    /// the resolved track instrument captured by the replayer at
    /// row-start time.
    track_instr: Option<usize>,
    speed: usize,
    volume_modifier: NoteRetrigOperator,
}

// =====================================================================
// PHASE 2 TRANSITION BRIDGES (pitch chain f32 ↔ Q-format)
// =====================================================================
// Tour 4 status: `StateSample.{finetune}` and the
// `get_finetuned_pitch` chain are typed `Finetune` /
// `PitchDelta`. Note triggers compose the played MIDI note
// (`Pitch::value() as i16` semitones) with the finetune
// `PitchDelta` directly into a Q8.8 `PitchQ`, no f32 round
// trip. The remaining f32 surfaces are concentrated in
// `update_frequency`'s Amiga `log2` shim, `StateSample::set_step`
// (mixer phase), and the public API entry points.

/// Compose the played note (`Pitch::value() as i16` semitones)
/// with the sample's finetune contribution (semitones) into the
/// `Channel.note` field's Q8.8 `PitchQ`.
///
/// `played.value()` is `0..=119` (`C0..=B9`), well within the
/// `i16 << 8` range; `finetune` is `PitchDelta` Q8.8 carrying
/// at most `relative_pitch as i16 + ±1 semitone`. The sum
/// saturates inside the Q8.8 i16 range — `relative_pitch` is
/// import-clamped to `[-95, +96]` so the worst-case sum is
/// `(119 + 96 + 1) × 256 = 55296`, comfortably below
/// `i16::MAX = 32767 × ?` — wait, `55296 > 32767`. So we DO
/// saturate at high keys with high `relative_pitch`. That
/// matches the OLD f32 behaviour as far as the downstream
/// period table is concerned (clamped at note 119) — and the
/// XM/IT reference players also clamp there.
#[inline]
fn compose_played_pitch(played: Pitch, finetune: PitchDelta) -> PitchQ {
    let played_q = (played.value() as i16).saturating_mul(256);
    let raw = played_q.saturating_add(finetune.as_q8_8_i16());
    PitchQ::from_q8_8_i16(raw)
}

#[derive(Clone)]
pub struct Channel<'a> {
    module: &'a Module,
    period_helper: PeriodHelper,
    rate: SampleRate,

    /// Last triggered note in Q8.8 semitones (was `f32`). Bakes
    /// in finetune from the sample at trigger time. Used by
    /// `TonePortamento` to recompute the target period and by
    /// the arpeggio quirk clamp (FT2). Distinct from
    /// [`Self::current_note`] (legacy enum, integer semitones).
    note: PitchQ,
    /// The last Pitch enum value triggered on this channel. Parallels
    /// `note` (which is a Q8.8 semitone count baking finetune in);
    /// this one keeps the clean integer-semitone identity used by
    /// DCT::Note. `None` until the channel has seen any valid note
    /// trigger.
    current_note: Option<Pitch>,

    /// IT MIDI-macro selector: which parametric macro slot (0..15)
    /// is "active" on this channel. Modified by `SFx`; read by
    /// `Zxx` with `xx < 0x80`. Defaults to 0 (the `SF0` macro,
    /// typically the default filter-cutoff macro in IT files).
    midi_parametric_selector: usize,

    /// S91/S90 surround flag. When set, the right channel output
    /// is phase-inverted — the classic IT pseudo-stereo trick that
    /// folds to silence in mono and sounds spatially wide in stereo.
    /// Persists until explicitly toggled; not reset by note
    /// triggers.
    surround: bool,

    pub current: Cell,

    /// Row's absolute song tick — set at the top of every
    /// `tick0` call and kept stable for the row's tick run.
    /// Used by [`Channel::apply_slides_and_glide_from_lanes`] and
    /// the debug dual-read validators to query
    /// `AutomationLane`s in song-tick coordinates without
    /// re-threading the sequencer's state through every
    /// per-tick call. DAW migration Phase 3c.1.
    pub(crate) current_abs_tick: u32,
    /// Current sub-song index. Set alongside
    /// [`Self::current_abs_tick`].
    pub(crate) current_song_idx: u16,

    /// Cached `Track.instrument` for the cell currently held in
    /// [`Self::current`]. DAW migration Phase 1+2 dropped the
    /// per-cell `instrument: Option<usize>` from `Cell` — the value
    /// lives on the owning Track now, and the replayer pushes it
    /// alongside the cell through each row-start dispatch. `None` for
    /// effect-only tracks (the
    /// [`crate::daw::build_timeline::EFFECT_ONLY_INSTRUMENT`] sentinel
    /// surfaces here as `None`) and for channels with no active clip.
    pub(crate) current_track_instrument: Option<usize>,

    /// Current period (was `f32`). Slides accumulate `i16`
    /// deltas via `Period::saturating_add_signed`. Loss of
    /// sub-period precision against the OLD f32 storage is
    /// at most one LSB of period per slide step — far below a
    /// cent of pitch.
    period: Period,

    channel_volume: ChannelVolume,
    /// Per-voice running volume, Q1.15 in `[0, 1]`. Was `f32`.
    volume: Volume,
    /// Pan position, Q1.15 in `[0, 1]`. Was `f32`. `0` = full
    /// left, `0.5` = centre, `1` = full right.
    panning: Panning,

    // Instrument
    /// 1.4 — live voice in the shared pool.
    live: Option<VoiceId>,
    /// Cached midi_mute_computer; refreshed at trigger to keep
    /// `is_muted` pool-free.
    instr_midi_mute: bool,

    effect_arpeggio: EffectArpeggio,
    effect_note_retrig_backup: NoteRetrigState,
    effect_note_retrig_counter: usize,
    effect_panbrello: EffectVibratoTremolo,
    /// Tone-portamento target period. Was `f32`; now `Period`.
    effect_tone_portamento_goal: Period,
    effect_tremolo: EffectVibratoTremolo,
    effect_tremor: bool,
    effect_tremor_on: usize,
    effect_tremor_off: usize,
    /// S3M/ST3 tremor state machine (separate from the formula-based
    /// FT2/XM path above): number of ticks remaining before the
    /// on/off state toggles. Negative value = inactive (no Ixy has
    /// run yet on this channel since the last reset). Persists
    /// across rows so consecutive Ixy lines form a continuous cycle
    /// instead of restarting the phase at each row.
    effect_tremor_counter_s3m: i32,
    /// Paired with `effect_tremor_counter_s3m`: `true` when the
    /// current tremor slot is the *silent* half of the cycle (ST3
    /// `atreon == false`). Drives `effect_tremor` for the volume
    /// application path.
    effect_tremor_silent_s3m: bool,
    effect_vibrato: EffectVibratoTremolo,
    /// If `true`, the next note trigger resets the vibrato phase to 0;
    /// otherwise the phase carries over. Set from
    /// `TrackEffect::VibratoWaveform::retrig`. Defaults to `true`.
    vibrato_retrig_on_new_note: bool,
    /// Same as `vibrato_retrig_on_new_note` but for tremolo, driven by
    /// `TrackEffect::TremoloWaveform::retrig`.
    tremolo_retrig_on_new_note: bool,
    effect_semitone: bool,
    effect_note_delay: usize,

    /// Index of this channel inside the `Voices` list. Set once at
    /// construction time (via [`Self::set_track_index`], called by
    /// `Voices::new`) and never modified afterwards. Tags voices
    /// spawned by NNA with their originating track — see
    /// [`crate::voice::Voice::master_track`]. Ghosts spawned before
    /// `set_track_index` runs would carry index 0, which is correct
    /// for channel 0 and benign for the others (the field is purely
    /// informational).
    track_index: usize,

    pub muted: bool,

    actual_volume: [Volume; 2],

    // --- IT New Note Action / past-note state ---
    //
    // Ghost voices: notes that were playing when a new note fired on
    // the same pattern channel and whose instrument's `NewNoteAction`
    // was not `Cut`. They continue to sound (via their own envelope +
    // fadeout) alongside the live note until they go silent.
    //
    // Voices themselves live in the `VoicePool` carried by the
    // owning `Voices`; this list holds opaque `VoiceId` handles
    // into that pool. The pool's lowest-volume eviction policy
    // protects sustained voices and refuses ghost spawns when
    // every existing voice is still audible — see
    // [`crate::voice_pool::VoicePool::allocate_ghost`].
    //
    // No-op on MOD/XM/S3M because those importers leave NNA at its
    // `NoteCut` default — `spawn_ghost_*` short-circuits and this
    // list stays empty.
    ghosts: Vec<VoiceId>,
    /// S7x (S73–S76) lets the *pattern* override an instrument's
    /// stored NNA for the current channel, affecting only subsequent
    /// triggers on this channel. `None` = use the incoming
    /// instrument's own NNA value, which is the normal case.
    nna_override: Option<NewNoteAction>,
    /// PRNG for IT humanisation (random volume / pan variation).
    /// Seeded deterministically per-channel by `Voices::new` so the
    /// same module always produces the same WAV — IT humanisation
    /// is meant to de-robot static samples, not to introduce non-
    /// determinism into the render pipeline. Updated at every
    /// note-trigger that consults `random_*_variation`.
    rng: xmrs::core::xorshift::XorShift32,

    /// Euclidean humanisation state — see
    /// [`crate::state_humanize_ekn`].
    pub(crate) humanize_ekn: crate::state_humanize_ekn::StateHumanizeEkn,
}

impl<'a> Channel<'a> {
    /// Build a channel. Format-specific quirks (FT2 arpeggio LUT,
    /// FT2 arpeggio period clamp) are driven by `module.profile.format` via
    /// `EffectArpeggio` and `PeriodHelper`; Channel itself no longer
    /// carries an `Ft2Quirks` field, so there is only one source of
    /// truth for "is this an XM?".
    ///
    /// `tempo` is the initial song speed, forwarded to the arpeggio
    /// effect so its FT2 LUT is usable on the very first row. The
    /// player must subsequently call [`Self::set_tempo`] whenever the
    /// song tempo changes (Fxx effect) to keep the LUT index in sync.
    pub(crate) fn new(module: &'a Module, rate: SampleRate, tempo: usize) -> Self {
        let period_helper =
            PeriodHelper::new(module.frequency_type, module.quirks.ft2_arpeggio_note_clamp);
        Self {
            module,
            period_helper: period_helper.clone(),
            rate,
            channel_volume: ChannelVolume::FULL,
            volume: Volume::FULL,
            panning: Panning::CENTER,
            note: PitchQ::from_q8_8(Q8_8::ZERO),
            current_note: None,
            midi_parametric_selector: 0,
            surround: false,
            current: Cell::default(),
            current_abs_tick: 0,
            current_song_idx: 0,
            current_track_instrument: None,
            period: Period::ZERO,
            live: None,
            instr_midi_mute: false,
            effect_arpeggio: EffectArpeggio::new(module.quirks.ft2_arpeggio_lut, tempo),
            effect_note_retrig_backup: NoteRetrigState::default(),
            effect_note_retrig_counter: 0,
            effect_panbrello: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremolo: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremor: false,
            effect_tremor_on: 0,
            effect_tremor_off: 0,
            // ST3 starts with `atreon = false` ("in the silent half
            // of the cycle") and `atremor = 0`. On the first Ixy
            // tick the `atremor == 0` check triggers the toggle,
            // which flips `atreon` to `true` (playing) and reloads
            // the counter with `on_time`. Mirroring that here: start
            // with `silent_s3m = true` so the first toggle flips us
            // to "playing"; the counter sentinel `-1` acts like ST3's
            // initial `atremor = 0` (not > 0, so toggle branch).
            effect_tremor_counter_s3m: -1,
            effect_tremor_silent_s3m: true,
            effect_vibrato: EffectVibratoTremolo::new(Waveform::Sine),
            vibrato_retrig_on_new_note: true,
            tremolo_retrig_on_new_note: true,
            effect_tone_portamento_goal: Period::ZERO,
            effect_semitone: false,
            effect_note_delay: 0,
            muted: false,
            track_index: 0,
            actual_volume: [Volume::SILENT, Volume::SILENT],
            ghosts: Vec::new(),
            nna_override: None,
            // `XorShift32::default()` uses the type's canonical
            // non-zero seed (4294967291). `Voices::new` re-seeds
            // each channel with a per-channel-unique value after
            // construction so the streams stay independent.
            rng: xmrs::core::xorshift::XorShift32::default(),
            humanize_ekn: crate::state_humanize_ekn::StateHumanizeEkn::new(0xC0DE_0001),
        }
    }

    /// Set the channel's resting panning before any pattern row has
    /// been processed. Used by [`crate::voices::Voices::new_with_voice_pool_capacity`] to apply per-channel
    /// pan defaults from the module header (`Module.channel_defaults
    /// [i].panning`). For formats that don't carry per-channel pan
    /// hints, the channel stays at its centre default.
    #[inline(always)]
    pub(crate) fn set_initial_panning(&mut self, panning: Panning) {
        self.panning = panning;
    }

    /// Set the channel volume before any pattern row has been
    /// processed. Used by [`crate::voices::Voices::new_with_voice_pool_capacity`] to apply IT's
    /// `initial_channel_volume` header bytes via
    /// `Module.channel_defaults[i].volume`.
    #[inline(always)]
    pub(crate) fn set_initial_channel_volume(&mut self, volume: ChannelVolume) {
        self.channel_volume = volume;
    }

    /// Set the channel's mute flag before any pattern row has been
    /// processed. Used by [`crate::voices::Voices::new_with_voice_pool_capacity`] to apply S3M's "channel
    /// disabled" bit and IT's `initial_channel_pan & 0x80` flag
    /// from `Module.channel_defaults[i].muted`.
    #[inline(always)]
    pub(crate) fn set_initial_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    /// Set the channel's surround flag before any pattern row has
    /// been processed. Used by [`crate::voices::Voices::new_with_voice_pool_capacity`] to apply IT's
    /// surround sentinel (`initial_channel_pan == 100`) via
    /// `Module.channel_defaults[i].surround`. Equivalent to an
    /// S91 effect, but applied as state at song start so it
    /// doesn't depend on row 0 being free.
    #[inline(always)]
    pub(crate) fn set_initial_surround(&mut self, surround: bool) {
        self.surround = surround;
    }

    /// Re-seed the channel's PRNG. Called by `Voices::new` with a
    /// per-channel value so each channel gets an independent (but
    /// deterministic) humanisation stream. Zero is rejected by
    /// `XorShift32::new` — the caller is responsible for passing
    /// a non-zero seed.
    pub(crate) fn reseed_rng(&mut self, seed: u32) {
        let seed = if seed == 0 { 0xDEADBEEF } else { seed };
        self.rng = xmrs::core::xorshift::XorShift32::new(Some(seed));
    }

    /// Record this channel's index inside the `Voices` list. Called
    /// by `Voices::new` immediately after construction. The index
    /// flows into voices spawned by NNA so the shared `VoicePool`
    /// can route past-note effects and DCT lookups via a single
    /// list keyed on the spawning channel.
    pub(crate) fn set_track_index(&mut self, idx: usize) {
        self.track_index = idx;
    }

    // --- Live-voice accessors --------------------------------------
    fn live<'p>(&self, pool: &'p VoicePool<'a>) -> Option<&'p StateInstrDefault<'a>> {
        self.live.and_then(|id| pool.get(id)).map(|v| &v.instr)
    }
    fn live_mut<'p>(&self, pool: &'p mut VoicePool<'a>) -> Option<&'p mut StateInstrDefault<'a>> {
        self.live
            .and_then(|id| pool.get_mut(id))
            .map(|v| &mut v.instr)
    }
    fn drop_live(&mut self, pool: &mut VoicePool<'a>) {
        if let Some(id) = self.live.take() {
            pool.release(id);
        }
    }
    /// Move the live voice into the channel's ghost list without
    /// reallocating in the pool — the slot just changes role from
    /// "live" to "detached". Returns the moved `VoiceId` so the
    /// caller can apply NNA. The ghost list grows unboundedly per
    /// channel (1.5+); the shared pool's capacity is the only cap.
    fn promote_live_to_ghost(&mut self, _pool: &mut VoicePool<'a>) -> Option<VoiceId> {
        let id = self.live.take()?;
        self.ghosts.push(id);
        Some(id)
    }
    /// Allocate a fresh live voice in the pool. Releases any
    /// previously-held live id (the caller should already have
    /// promoted it to a ghost if NNA != Cut). Caches the
    /// instrument's midi_mute_computer so `is_muted` stays
    /// pool-free.
    fn install_live(
        &mut self,
        pool: &mut VoicePool<'a>,
        state: StateInstrDefault<'a>,
        midi_mute: bool,
    ) {
        if let Some(old) = self.live.take() {
            pool.release(old);
        }
        let voice = Voice::new_live(state, self.track_index);
        self.live = Some(pool.allocate_live(voice));
        self.instr_midi_mute = midi_mute;
    }

    /// Forward a tempo change to the arpeggio effect. Called by the
    /// player on every step so the FT2 arpeggio LUT is indexed from
    /// the current speed rather than a stale copy.
    pub(crate) fn set_tempo(&mut self, tempo: usize) {
        self.effect_arpeggio.set_tempo(tempo);
    }

    pub fn is_muted(&self) -> bool {
        self.muted || self.instr_midi_mute
    }

    fn cut_pitch(&mut self) {
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
    /// * Quirk **on** (XM via `CompatibilityProfile::ft2()`) —
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
    /// The `tick` parameter is kept in the signature so callers
    /// (Kxx effect at ticks > 0, note-column `===` at tick 0,
    /// note-delay paths) don't need to case-split. FT2 itself
    /// also ignores which tick the key-off lands on — the branch
    /// on envelope is the only real decision.
    fn key_off(&mut self, _tick: usize, pool: &mut VoicePool<'a>) {
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
    fn note_fade(&mut self, pool: &mut VoicePool<'a>) {
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

    // ------------------------------------------------------------------
    // IT MIDI-macro dispatch
    // ------------------------------------------------------------------

    /// Apply a MIDI-macro effect (`SFx` / `Zxx`) to this channel.
    ///
    /// Macro strings live on `module.midi_macros` — `None` on non-IT
    /// modules or IT files without an embedded macro table, in
    /// which case this is a silent no-op.
    ///
    /// # What's interpreted
    ///
    /// IT's internal filter-control frames (`F0 F0 00/01 xx`) drive
    /// the per-voice resonant filter directly. Standard MIDI channel
    /// messages (`8n..En` status bytes) are packaged as
    /// [`MidiEvent`]s and pushed into `out_events`; the facade then
    /// drains that buffer to any subscribed [`crate::midi_observer::MidiObserver`]. SysEx
    /// (`F0 .. F7`, except the IT-internal `F0 F0` prefix) is passed
    /// through as [`MidiEvent::SysEx`].
    ///
    /// The literal byte `z` (0x7A) anywhere in a macro is replaced
    /// by the `Zxx` parameter before frame parsing.
    pub(crate) fn apply_midi_macro(
        &mut self,
        macro_type: MidiMacroType,
        ch_index: usize,
        out_events: &mut alloc::vec::Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        match macro_type {
            MidiMacroType::SelectParametric(idx) => {
                // SFx — just stash the selector. The macro only
                // fires when a subsequent Zxx<0x80 picks it up.
                self.midi_parametric_selector = idx;
            }
            MidiMacroType::Parametric(z) => {
                let Some(macros) = &self.module.midi_macros else {
                    return;
                };
                let Some(bytes) = macros.parametric.get(self.midi_parametric_selector) else {
                    return;
                };
                self.interpret_macro_bytes(bytes.clone(), z, ch_index, out_events, pool);
            }
            MidiMacroType::Fixed { idx, z } => {
                let Some(macros) = &self.module.midi_macros else {
                    return;
                };
                let Some(bytes) = macros.fixed.get(idx) else {
                    return;
                };
                self.interpret_macro_bytes(bytes.clone(), z, ch_index, out_events, pool);
            }
        }
    }

    /// Walk the macro byte stream, substituting `z` (0x7A) with the
    /// Zxx parameter, and interpret each recognised message.
    /// `F0 F0 sub val` is consumed for internal filter control;
    /// standard MIDI channel messages and SysEx are packaged into
    /// `out_events` for external dispatch.
    fn interpret_macro_bytes(
        &mut self,
        bytes: alloc::vec::Vec<u8>,
        z_value: u8,
        ch_index: usize,
        out_events: &mut alloc::vec::Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        // IT macros are stored in the file as 32-byte ASCII strings
        // (ITTECH, "Internal Editing Of Macros"). Hex digits encode
        // nibbles two-per-byte and a small letter table substitutes
        // dynamic per-channel state (note, velocity, pan, etc.).
        // Raw-byte interpretation produces no MIDI output for any
        // real-world IT file because every ASCII char is a data byte
        // (< 0x80). We therefore expand the text first, then run the
        // MIDI parser on the resulting binary buffer.
        let bytes = self.expand_macro_text(&bytes, z_value, ch_index, &*pool);

        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                // IT internal filter command: `F0 F0 sub val`. The
                // `F0 F0` prefix is distinctive because real MIDI
                // SysEx is `F0 .. F7`, never `F0 F0` back-to-back.
                0xF0 if i + 1 < bytes.len() && bytes[i + 1] == 0xF0 => {
                    if i + 3 >= bytes.len() {
                        break; // incomplete frame
                    }
                    let sub = bytes[i + 2];
                    let val = bytes[i + 3];
                    match sub {
                        0x00 => {
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_cutoff_reg(val);
                            }
                        }
                        0x01 => {
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_resonance_reg(val);
                            }
                        }
                        0x02 => {
                            // Filter mode: bit 4 = HPF, bit 5 =
                            // disable. Implementation details in
                            // `StateFilter::set_mode_from_macro`.
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_mode_from_macro(val);
                            }
                        }
                        // 0x03..0x7F reserved.
                        _ => {}
                    }
                    i += 4;
                }

                // 8n nn vv — Note Off (3 bytes)
                0x80..=0x8F => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::NoteOff {
                            channel: b & 0x0F,
                            note: bytes[i + 1] & 0x7F,
                            velocity: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // 9n nn vv — Note On (3 bytes). Velocity 0 collapses
                // to Note Off per MIDI convention.
                0x90..=0x9F => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    let vel = bytes[i + 2] & 0x7F;
                    let note = bytes[i + 1] & 0x7F;
                    let channel = b & 0x0F;
                    let ev = if vel == 0 {
                        MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        }
                    } else {
                        MidiEvent::NoteOn {
                            channel,
                            note,
                            velocity: vel,
                        }
                    };
                    out_events.push((ch_index, ev));
                    i += 3;
                }

                // An nn pp — Polyphonic Aftertouch (3 bytes)
                0xA0..=0xAF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::PolyAftertouch {
                            channel: b & 0x0F,
                            note: bytes[i + 1] & 0x7F,
                            pressure: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // Bn cc vv — Control Change (3 bytes)
                0xB0..=0xBF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ControlChange {
                            channel: b & 0x0F,
                            controller: bytes[i + 1] & 0x7F,
                            value: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // Cn pp — Program Change (2 bytes)
                0xC0..=0xCF => {
                    if i + 1 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ProgramChange {
                            channel: b & 0x0F,
                            program: bytes[i + 1] & 0x7F,
                        },
                    ));
                    i += 2;
                }

                // Dn pp — Channel Aftertouch (2 bytes)
                0xD0..=0xDF => {
                    if i + 1 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ChannelAftertouch {
                            channel: b & 0x0F,
                            pressure: bytes[i + 1] & 0x7F,
                        },
                    ));
                    i += 2;
                }

                // En ll mm — Pitch Bend (3 bytes, 14-bit assembled)
                0xE0..=0xEF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    let lsb = (bytes[i + 1] & 0x7F) as u16;
                    let msb = (bytes[i + 2] & 0x7F) as u16;
                    out_events.push((
                        ch_index,
                        MidiEvent::PitchBend {
                            channel: b & 0x0F,
                            value: lsb | (msb << 7),
                        },
                    ));
                    i += 3;
                }

                // F0 .. F7 — SysEx (non-IT-internal; `F0 F0` handled
                // above). Collect bytes until F7 or end of macro.
                0xF0 => {
                    // Accumulate the SysEx including the leading F0
                    // but stopping before F7.
                    let start = i;
                    let mut end = start + 1;
                    while end < bytes.len() && bytes[end] != 0xF7 {
                        end += 1;
                    }
                    let payload = bytes[start..end].to_vec();
                    out_events.push((ch_index, MidiEvent::SysEx(payload)));
                    // Skip past F7 if present, else end-of-stream.
                    i = if end < bytes.len() { end + 1 } else { end };
                }

                // Non-status bytes (< 0x80) or unrecognised — skip
                // one byte forward. Some macros use arbitrary data
                // bytes as padding between commands.
                _ => i += 1,
            }
        }
    }

    /// Expand an IT MIDI-macro text string to a binary byte stream.
    ///
    /// Macros are stored in the IT file as 32-byte ASCII strings
    /// (NUL-padded). Schism's `csf_process_midi_macro`
    /// (`effects.c:1031-1180`) is the canonical reference. The
    /// substitution table:
    ///
    /// | Char    | Meaning                       | Form       |
    /// |---------|-------------------------------|------------|
    /// | `0`-`9` | hex nibble                    | nibble     |
    /// | `A`-`F` | hex nibble                    | nibble     |
    /// | `c`     | MIDI channel (instrument)     | nibble     |
    /// | `n`     | last triggered note           | full byte  |
    /// | `v`     | note velocity (Zxx ⇒ 1)       | full byte  |
    /// | `u`     | channel running volume        | full byte  |
    /// | `x`     | channel pan                   | full byte  |
    /// | `y`     | final channel pan             | full byte  |
    /// | `a`     | MIDI bank high (instrument)   | full byte  |
    /// | `b`     | MIDI bank low (instrument)    | full byte  |
    /// | `p`     | MIDI program (instrument)     | full byte  |
    /// | `z`     | Zxx parameter                 | full byte  |
    /// | `h`     | host (tracker) channel        | full byte  |
    /// | `m`     | sample loop direction         | full byte  |
    /// | `o`     | sample offset high byte       | full byte  |
    ///
    /// Hex nibbles pack two-per-byte (high nibble first); a
    /// substitution that's a full byte arriving while a single
    /// nibble is pending flushes the half-byte as-is (matching
    /// schism's `write_pos++` without OR-folding). Unrecognised
    /// bytes are silently dropped (also matching schism's `default:
    /// continue`).
    fn expand_macro_text(
        &self,
        bytes: &[u8],
        z_value: u8,
        ch_index: usize,
        pool: &VoicePool<'a>,
    ) -> alloc::vec::Vec<u8> {
        let mut out = alloc::vec::Vec::with_capacity(bytes.len());
        let mut pending_low_nibble: Option<u8> = None;

        for &b in bytes {
            // NUL terminator: schism's outer loop tests
            // `macro[read_pos]` (C-string sentinel) so any 0 byte
            // ends the macro. IT files NUL-pad each 32-byte slot,
            // and stopping at the first NUL keeps the trailing
            // padding from emitting a string of zero bytes.
            if b == 0 {
                break;
            }

            let (data, is_nibble) = match b {
                b'0'..=b'9' => (b - b'0', true),
                b'A'..=b'F' => (b - b'A' + 0x0A, true),
                b'c' => (self.macro_subst_midi_channel(), true),
                b'n' => (self.macro_subst_note(), false),
                b'v' => (self.macro_subst_velocity(), false),
                b'u' => (self.macro_subst_volume(), false),
                b'x' => (self.macro_subst_pan(), false),
                b'y' => (self.macro_subst_final_pan(), false),
                b'a' => (self.macro_subst_bank_high(), false),
                b'b' => (self.macro_subst_bank_low(), false),
                b'p' => (self.macro_subst_program(), false),
                b'z' => (z_value, false),
                b'h' => ((ch_index as u8) & 0x7F, false),
                b'm' => (self.macro_subst_loop_direction(pool), false),
                b'o' => (self.macro_subst_offset_high(), false),
                // Anything else (whitespace, lower-case letters not
                // listed, punctuation): skip silently like schism.
                _ => continue,
            };

            if is_nibble {
                let nib = data & 0x0F;
                if let Some(low) = pending_low_nibble {
                    // Second nibble of a pair: existing low half
                    // moves to the high position, new nibble takes
                    // the low half.
                    out.push((low << 4) | nib);
                    pending_low_nibble = None;
                } else {
                    pending_low_nibble = Some(nib);
                }
            } else {
                // Full-byte substitution. If a half-byte is pending,
                // commit it as a low-nibble byte (schism's behaviour
                // is `write_pos++` with no shift — the pending nibble
                // is already stored in `outbuffer[write_pos]`'s low
                // half from the first encounter).
                if let Some(low) = pending_low_nibble {
                    out.push(low);
                    pending_low_nibble = None;
                }
                out.push(data);
            }
        }

        // Trailing single nibble: schism flushes it the same way
        // (`if (nibble_pos == 1) write_pos++;`). Mirror it.
        if let Some(low) = pending_low_nibble {
            out.push(low);
        }

        out
    }

    // ------------------------------------------------------------------
    // MIDI-macro letter substitutions
    //
    // These mirror schism's `csf_process_midi_macro` substitution
    // dispatch (`effects.c:1061-1156`), each returning the value to
    // splice into the macro's binary expansion. Any substitution
    // that depends on state xmrsplayer doesn't currently surface
    // (final-pan post-envelope, live ping-pong direction, sticky
    // Oxx memory) returns a documented placeholder rather than
    // breaking the macro byte-stream.
    // ------------------------------------------------------------------

    /// Look up the current instrument's [`InstrMidi`] payload, if
    /// the cell's instrument index points at an `InstrumentType::
    /// Midi` variant. Returns `None` for sampled / OPL / SID /
    /// missing instruments — the caller substitutes 0 in that
    /// case, which is what schism does when `penv->midi_*` is
    /// unset.
    fn current_instr_midi(&self) -> Option<&InstrMidi> {
        let idx = self.current_track_instrument?;
        let instr = self.module.instrument.get(idx)?;
        match &instr.instr_type {
            InstrumentType::Midi(im) => Some(im),
            _ => None,
        }
    }

    /// `c` — MIDI channel for the active instrument, masked to
    /// 0..15. Schism falls back to 15 when no instrument has a
    /// MIDI-channel mask; we match that for non-Midi instruments.
    fn macro_subst_midi_channel(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| im.channel & 0x0F)
            .unwrap_or(15)
    }

    /// `n` — last-triggered note. xmrs's `Pitch::value()` is
    /// already 0-based MIDI-aligned (`Pitch::C0 = 0`,
    /// `Pitch::C5 = 60`), matching schism's `note - 1`
    /// transformation of the 1-based cell-stored byte.
    fn macro_subst_note(&self) -> u8 {
        self.current_note.map(|p| p.value()).unwrap_or(0)
    }

    /// `v` — note velocity. xmrsplayer reaches the macro
    /// interpreter only via `Zxx` / `SFx`; schism's caller passes
    /// `velocity = 0` in that path, then `CLAMP(0, 0x01, 0x7F)`
    /// promotes it to 1. Mirror that: the canonical "no
    /// explicit velocity" value is 1.
    fn macro_subst_velocity(&self) -> u8 {
        1
    }

    /// `u` — channel running volume mapped to the MIDI 1..127
    /// range. Schism does a four-register `_muldiv` with a known-
    /// approximate result (the comment at `effects.c:1091` flags it
    /// as not-quite-right); using the channel's already-resolved
    /// normalised volume produces the same musically-meaningful
    /// value for any correctly-authored macro.
    fn macro_subst_volume(&self) -> u8 {
        // Q-format integer conversion: Volume Q1.15 raw `v_q` ∈
        // `[0, 32767]` → `(v_q × 127) >> 15`, clamp `[1, 127]`.
        let v_q = self.volume.as_q15_i16().max(0) as u32;
        let v = ((v_q * 127) >> 15) as u8;
        v.clamp(1, 127)
    }

    /// `x` — channel pan, saturated at 127. xmrs stores pan as
    /// 0..1 normalised; schism stores it 0..256 with a `MIN(_, 127)`
    /// at substitution time. The musical mapping is the same once
    /// the saturation point lands at "fully right".
    fn macro_subst_pan(&self) -> u8 {
        // Q-format integer conversion: Panning Q1.15 raw `p_q` ∈
        // `[0, 32767]` → `(p_q × 127) >> 15`, clamp `[0, 127]`.
        let p_q = self.panning.as_q15_i16().max(0) as u32;
        let p = ((p_q * 127) >> 15) as u8;
        p.min(127)
    }

    /// `y` — "final" channel pan after pan envelope and pan-swing
    /// modulation. xmrsplayer's mixer keeps separate left/right
    /// gains in `actual_volume[0..2]` rather than a single post-
    /// envelope pan, so reconstructing the MIDI value would require
    /// a lossy inverse. Approximated by the static channel pan;
    /// macros relying on tight tracking of `y` will see the pre-
    /// envelope value.
    fn macro_subst_final_pan(&self) -> u8 {
        self.macro_subst_pan()
    }

    /// `a` — MIDI bank high byte: `(bank >> 7) & 0x7F`.
    fn macro_subst_bank_high(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| ((im.bank >> 7) & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `b` — MIDI bank low byte: `bank & 0x7F`.
    fn macro_subst_bank_low(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| (im.bank & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `p` — MIDI program, masked to 7 bits.
    fn macro_subst_program(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| (im.program & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `m` — sample-loop direction (`1` if currently playing
    /// reverse on a ping-pong loop, `0` otherwise). xmrsplayer's
    /// `Channel` does not surface live voice loop state at the
    /// macro-interpreter call site — the live voice's playback
    /// direction lives inside `StateSample` behind a `pool` look-
    /// up that would require mutable borrowing during what is
    /// otherwise a read-only expansion pass. Returning 0 keeps the
    /// macro byte-stream well-formed; macros using `m` are an
    /// OpenMPT-era extension that real-world IT files almost never
    /// exercise.
    fn macro_subst_loop_direction(&self, _pool: &VoicePool<'a>) -> u8 {
        0
    }

    /// `o` — sample offset high byte. Schism reads this from the
    /// channel's `mem_offset` register, which is the cumulative
    /// Oxx + SAx high-nibble memorised across rows (used by
    /// "ZxxSecrets.it" and similar OpenMPT test cases). xmrsplayer
    /// resolves Oxx inline when it fires and does not retain a
    /// sticky high-nibble register, so we substitute 0.
    fn macro_subst_offset_high(&self) -> u8 {
        0
    }

    // ------------------------------------------------------------------
    // IT New Note Action helpers
    // ------------------------------------------------------------------

    /// Resolve which NNA to apply when a new note displaces the
    /// currently-held voice. `S73`–`S76` override via
    /// `self.nna_override`; otherwise the NNA comes from the
    /// *outgoing* instrument (not the incoming one — IT's replay
    /// reads the old voice's NNA when deciding what happens to it).
    /// Returns `NoteCut` if no voice is currently held (nothing to
    /// ghost).
    fn effective_nna(&self, pool: &VoicePool<'a>) -> NewNoteAction {
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
    fn spawn_ghost_for_outgoing(&mut self, pool: &mut VoicePool<'a>) {
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
        let Some(id) = self.promote_live_to_ghost(pool) else {
            return;
        };
        if let Some(ghost) = pool.get_mut(id) {
            ghost.vol_frozen = self.volume;
            ghost.channel_volume_frozen = self.channel_volume;
            ghost.panning_frozen = self.panning;
            ghost.note = self.current_note;
            ghost.sample_num = ghost.instr.current_sample_num;
            ghost.period_at_fork = self.period;
            match nna {
                NewNoteAction::Continue => ghost.apply_nna_continue(),
                NewNoteAction::NoteOff => ghost.apply_nna_note_off(),
                NewNoteAction::NoteFadeOut => ghost.apply_nna_note_fade_out(),
                NewNoteAction::NoteCut => unreachable!(), // handled above
            }
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
    fn spawn_ghost_clone(&mut self, pool: &mut VoicePool<'a>) {
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
            self.track_index,
            self.volume,
            self.channel_volume,
            self.panning,
            self.current_note,
            sample_num,
            self.period,
        );
        match nna {
            NewNoteAction::Continue => ghost.apply_nna_continue(),
            NewNoteAction::NoteOff => ghost.apply_nna_note_off(),
            NewNoteAction::NoteFadeOut => ghost.apply_nna_note_fade_out(),
            NewNoteAction::NoteCut => unreachable!(),
        }
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
    fn tick_ghosts(&mut self, pool: &mut VoicePool<'a>) {
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
    fn past_note_cut_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_cut();
            }
        }
    }
    fn past_note_off_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_off();
            }
        }
    }
    fn past_note_fade_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_fade_out();
            }
        }
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
    fn apply_dct(
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

        // --- Live voice check. ---
        // Applying DCA to the live voice is equivalent to the usual
        // note-cut / key-off / fade semantics — just triggered by
        // DCT match rather than by effect column.
        let live_matches = self.live(pool).is_some_and(|live| match dct {
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
            // The note compared is the *input* note (pattern
            // column), not a remapped output — drum kits compare
            // on input.
            Dct::Note => {
                live.num == new_instrument_index
                    && match (self.current_note, new_note) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    }
            }
            Dct::Sample => {
                live.num == new_instrument_index
                    && live.current_sample_num == new_sample_index
                    && new_sample_index.is_some()
            }
            Dct::Instrument => live.num == new_instrument_index,
        });
        if live_matches {
            match &dca {
                DuplicateCheckAction::NoteCut(_) => {
                    // Match schism's DCA_NOTECUT (effects.c:1742-1745
                    // + the safety net at 1761-1764):
                    //
                    // ```c
                    // case DCA_NOTECUT:
                    //     fx_key_off(csf, i);
                    //     p->volume = 0;
                    //     ...
                    // if (!p->volume) {
                    //     p->fadeout_volume = 0;
                    //     p->flags |= (CHN_NOTEFADE|CHN_FASTVOLRAMP);
                    // }
                    // ```
                    //
                    // The pre-fix code only called the channel-side
                    // `cut_pitch` (zeroes `self.volume`, the channel's
                    // volume modulator). That silenced the live voice
                    // through `actual_volume`, but left every per-
                    // voice predicate (`is_enabled`, `volume_fading_
                    // out`, `volume_fadeout`) in their pre-cut state.
                    // When `spawn_ghost_for_outgoing` subsequently
                    // promoted this live voice to a ghost (line ~1790
                    // of `tick0_change_instr`), it captured a frozen
                    // `vol_frozen = 0` from the cut channel volume —
                    // a permanently silent ghost that nothing would
                    // ever reap. Modules with active DCT NoteCut on
                    // every retrigger thus accumulated one immortal
                    // silent ghost per displaced note.
                    self.cut_pitch();
                    if let Some(i) = self.live_mut(pool) {
                        i.key_off(); // = fx_key_off
                        i.cut_pitch(); // = p->volume = 0
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
            let matches = match dct {
                Dct::Note => {
                    // Same instrument-AND-note rule as for the live
                    // voice. See the comment on the live arm above.
                    ghost.instr.num == new_instrument_index
                        && match (ghost.note, new_note) {
                            (Some(a), Some(b)) => a == b,
                            _ => false,
                        }
                }
                Dct::Sample => {
                    ghost.instr.num == new_instrument_index
                        && ghost.sample_num == new_sample_index
                        && new_sample_index.is_some()
                }
                Dct::Instrument => ghost.instr.num == new_instrument_index,
            };
            if matches {
                match &dca {
                    DuplicateCheckAction::NoteCut(_) => ghost.past_cut(),
                    DuplicateCheckAction::NoteOff(_) => ghost.past_off(),
                    DuplicateCheckAction::NoteFadeOut(_) => ghost.past_fade_out(),
                }
            }
        }
    }

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
                // `volume + volume × d`, which is exactly the
                // semantic the f32 site computed (small
                // multiplicative perturbation around unity).
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

    fn tickn_update_instr(&mut self, pool: &mut VoicePool<'a>) {
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

    pub(crate) fn tick(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
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

    fn tickn_effects(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
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
                    } else {
                        if n1 != 0 || n2 != 0 {
                            self.effect_arpeggio.tick();
                        }
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
                TrackEffect::NoteDelay(delay) => {
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
                                    TRIGGER_KEEP_SAMPLE_POSITION
                                        | TRIGGER_KEEP_VOLUME
                                        | TRIGGER_KEEP_PERIOD,
                                    pool,
                                );
                            }
                            CellEvent::NoteOff { retrig: false } => {
                                self.key_off(0, pool);
                            }
                            CellEvent::NoteOff { retrig: true } => {
                                self.trigger_pitch(
                                    TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE,
                                    pool,
                                );
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
                            self.key_off(t, pool);
                        }
                    }
                }
                TrackEffect::NoteRetrig {
                    speed: s,
                    volume_modifier: m,
                } => {
                    // NoteDelay interaction: when a row carries both
                    // `SDx` (NoteDelay) and `Qxy` (NoteRetrig), the
                    // retrig cadence is measured from the delayed
                    // trigger, not from tick 0. Before the delay
                    // fires, we simply don't count — the voice
                    // hasn't actually started yet.
                    if current_tick < self.effect_note_delay {
                        continue;
                    }

                    let current_state = NoteRetrigState {
                        event: self.current.event,
                        track_instr: self.current_track_instrument,
                        speed: s,
                        volume_modifier: m.clone(),
                    };

                    // Reset the retrig counter at the moment the
                    // effect becomes active on a new row — for a
                    // plain row that's tick 0, for a SDx-delayed
                    // row it's the tick the delay fires.
                    let counter_reset_tick = self.effect_note_delay;
                    if current_tick == counter_reset_tick
                        && self.effect_note_retrig_backup != current_state
                    {
                        self.effect_note_retrig_counter = 0;
                        self.effect_note_retrig_backup = current_state;
                    }

                    // Increment the counter FIRST, then test.
                    self.effect_note_retrig_counter += 1;

                    // If speed (s) is 0, retrig is effectively disabled.
                    if s != 0 && self.effect_note_retrig_counter.is_multiple_of(s) {
                        self.trigger_pitch(TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_ENVELOPE, pool);
                        match m {
                            NoteRetrigOperator::None => {
                                // No volume modification, just retrigger.
                            }
                            NoteRetrigOperator::Sum(delta) => {
                                // Pure Q1.15: saturating add of a
                                // signed Q15 modulation, clamped to
                                // `[0, 1]` by `with_tremolo`.
                                self.volume = self.volume.with_tremolo(delta);
                            }
                            NoteRetrigOperator::Mul(factor) => {
                                // Pure Q1.15: Q3.13 × Q1.15 with
                                // saturation, all in
                                // `RetrigMul::applied_to`.
                                self.volume = factor.applied_to(self.volume);
                            }
                        }
                    }
                }
                TrackEffect::Panning(p) => {
                    if current_tick == 0 {
                        // Both sides typed — direct assignment.
                        self.panning = p;
                    }
                }
                TrackEffect::Tremor {
                    on_time: on,
                    off_time: off,
                } => {
                    if self.module.quirks.tremor_state_persists {
                        // ST3 tremor (digcmd.c:s_tremor line 803): a
                        // count-down `atremor` + on/off toggle
                        // `atreon`, both persistent across rows. On
                        // every tick where Ixy runs (including tick
                        // 0), if the counter is > 0 decrement it,
                        // else toggle the on/off state and reload the
                        // counter with the corresponding nibble.
                        //
                        // The on/off nibble *cache* is refreshed at
                        // tick 0 (so memory via GET_LAST_NFO is in
                        // sync) but the state machine is NOT reset —
                        // consecutive Ixy rows form a continuous
                        // cycle, matching ST3.
                        //
                        // Initial state: counter == -1 (inactive).
                        // On the first Ixy tick, counter is non-0
                        // positive? no — we treat -1 as "toggle now"
                        // and the first toggle lands on "playing"
                        // (silent=false, which matches ST3's first
                        // hit where atreon flips from false to true).
                        if current_tick == 0 {
                            self.effect_tremor_on = on;
                            self.effect_tremor_off = off;
                        }
                        if self.effect_tremor_counter_s3m > 0 {
                            self.effect_tremor_counter_s3m -= 1;
                        } else {
                            // Toggle on/off (or start "on" from the
                            // initial inactive state). ITTECH: the
                            // counter reloads with nibble + 1 so
                            // that `Ix0` still produces a one-tick
                            // on phase (reload = 1, fires once,
                            // toggles next tick). Parser stores raw
                            // nibble; we apply the `+1` here to
                            // keep import conventions simple.
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
                        // FT2/XM Txy: per-row retrigger with modular
                        // formula. `current_tick - 1` gives 0 on the
                        // first effect tick; `(on + off + 2)` is the
                        // cycle length (FT2 plays for `on + 1`
                        // ticks then silences for `off + 1`).
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
                TrackEffect::Volume { value: v, tick: t } => {
                    if current_tick == t {
                        // Both sides typed — direct assignment.
                        self.volume = v;
                    }
                }
                TrackEffect::MidiMacro(_) => {
                    // DAW migration Phase 3c.3: the macro is
                    // invoked from `cell.effects` by
                    // `Voices::apply_row_per_channel_effects`. The
                    // channel arm itself is a no-op — the macro
                    // bytes mutate channel state through the voices
                    // path, not here.
                }
            }
        }

        // DAW migration Phase 3c.1+3c.3 — Slide + LFO per-tick
        // application driven entirely by `AutomationLane`s. The
        // migrated `TrackEffect` variants have been removed from the
        // cell side, so there is nothing in the match-loop above to
        // advance the LFO phases or apply slides — both happen here.
        self.apply_slides_from_lanes(current_tick);
        self.advance_lfos_from_lanes(current_tick);
    }

    /// Change instr and return true if it was the same
    fn tick0_change_instr(&mut self, sample_only: bool, pool: &mut VoicePool<'a>) -> bool {
        let instrnr = self.current_track_instrument.unwrap();

        // Euclidean indirection: an `InstrumentType::Euclidean`
        // wraps a Default sample-based instrument via `ekn.instr`.
        // We follow the pointer and play the wrapped instrument's
        // samples. The front instrument number (`instrnr`) is kept
        // as the voice's identity so DCT/NNA comparisons match the
        // cell's view of the world.
        let is_euclidian;
        let wrapped_idx = match &self.module.instrument[instrnr].instr_type {
            InstrumentType::Euclidean(ekn) => {
                is_euclidian = true;
                match ekn.instr {
                    Some(inner) if inner < self.module.instrument.len() => inner,
                    _ => return false, // wrapper without a valid target
                }
            }
            _ => {
                is_euclidian = false;
                instrnr
            }
        };
        // Arm / disarm Euclidean humanisation tracking. An
        // Euclidean trigger arms the channel (the NEXT pulse of
        // the same wrapper becomes eligible); any other trigger
        // clears it.
        if is_euclidian {
            self.humanize_ekn.note_euclidian_triggered(instrnr);
        } else {
            self.humanize_ekn.clear_euclidian();
        }

        if let InstrumentType::Default(id) = &self.module.instrument[wrapped_idx].instr_type {
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
                    self.key_off(0, pool);
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

    fn tick0_load_instrument_and_pitch(&mut self, pool: &mut VoicePool<'a>) {
        // FT2 "K00 eats note" quirk. In FT2, K00 is handled specially at
        // tickZero via getNewNote (ft2_replayer.c:1418): it fires a keyoff
        // and then RETURNS, never running triggerNote — so the note column
        // of that row is effectively dropped.
        //
        // Key constraints:
        //   - This is canonical FT2/XM behaviour, gated on
        //     `module.quirks.k00_eats_note`. The FT2
        //     `CompatibilityProfile` sets it; modern / IT / S3M /
        //     MOD profiles leave it off. (Per the
        //     `PlaybackQuirks` design contract, runtime decisions
        //     never test `module.profile.format` directly — the
        //     format is metadata only.)
        //   - Only K00 (param 0) short-circuits. For Kxy with y > 0 FT2
        //     takes the normal getNewNote path and then fires keyOffCmd at
        //     tick y, which means the note DOES play. Use
        //     `has_note_off_at_tick_zero()` rather than `has_note_off_effect()`,
        //     which would also match non-zero Kxy and wrongly eat their
        //     notes.
        //   - Note-column `===` (`CellNote::KeyOff`) has a different semantic
        //     (see `key_off()`) and belongs on the normal tick-0 path
        //     via `tick0_load_pitch`, hence the helper distinguishes
        //     effect-Kxx from column-keyoff.
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

    // `current_track_idx` was the debug-only twin of
    // [`Self::lookup_track_idx`] used by the dual-read validators.
    // Both validators and helper removed in DAW migration
    // Phase 3c.3 — `lookup_track_idx` is the single survivor.

    // All dual-read `validate_*_lane` helpers were removed in
    // DAW migration Phase 3c.3 — the migrated `TrackEffect`
    // variants are gone, so there's no cell-side source to compare
    // against. The lanes ARE the authoritative form at runtime.

    /// DAW migration Phase 3c.1 — **substitution**.
    ///
    /// Arm the three per-Track LFO state machines (Vibrato,
    /// Tremolo, Panbrello) from their `AutomationLane`s. Called
    /// at every row-start tick (`Channel::tick0`) **before**
    /// `tickn_effects(0, pool)` so the cell-side arms see the
    /// already-armed `effect_*.data`. The cell-side arms no
    /// longer write the LFO params — they only drive the
    /// per-tick `.tick_q()` advance and the format-specific
    /// quirks (FT2 vol-col-B, IT old-effects depth doubling,
    /// IT 2.14 row-zero tick).
    ///
    /// Skipped on `NoteDelay` rows since the cell-side effects
    /// (and the LFO arm) wait for the delay tick.
    fn arm_lfos_from_lanes(&mut self, abs_tick: u32, song: u16) {
        use xmrs::core::daw::automation::AutomationTarget;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };

        // ---- Vibrato (TrackPitch) ---------------------------------------
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            if state.armed {
                let depth = if self.module.quirks.it_old_effects {
                    state.depth.raw().saturating_mul(2)
                } else {
                    state.depth.raw()
                };
                self.effect_vibrato.data.speed = state.speed;
                self.effect_vibrato.data.depth = depth;
                self.effect_vibrato.data.waveform = WaveformState::new(state.waveform);
                self.vibrato_retrig_on_new_note = state.retrig;
            }
        }

        // ---- Tremolo (TrackVolume) --------------------------------------
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackVolume(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            if state.armed {
                self.effect_tremolo.data.speed = state.speed;
                self.effect_tremolo.data.depth = state.depth.raw();
                self.effect_tremolo.data.waveform = WaveformState::new(state.waveform);
                self.tremolo_retrig_on_new_note = state.retrig;
            }
        }

        // ---- Panbrello (TrackPanning) -----------------------------------
        // Panbrello on legacy: `effect_panbrello.tick0_q(s, d.raw())`
        // = latch params + retrigger phase. Substituted equivalent:
        // write params from lane + `retrigger_q` when newly armed.
        // The "newly armed" gate avoids re-zeroing the phase on
        // every row where the lane stays armed (matches legacy
        // semantics where the cell arm only fires when the row
        // carries the main effect).
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPanning(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            // Only arm + retrigger the phase when the row carries a
            // fresh main-effect Panbrello (mirrors the legacy "cell
            // arm fires only when `Panbrello { .. }` is on the
            // row"). DAW migration Phase 3c.3: the cell-side
            // check is replaced by a lane-side `events_in` query at
            // exactly `abs_tick`.
            let row_has_main =
                self.lane_has_lfo_set_at(AutomationTarget::TrackPanning(track_idx), abs_tick);
            if state.armed && row_has_main {
                self.effect_panbrello.data.speed = state.speed;
                self.effect_panbrello.data.depth = state.depth.raw();
                self.effect_panbrello.data.waveform = WaveformState::new(state.waveform);
                self.effect_panbrello.retrigger_q();
            }
        }
    }

    /// Active-clip → Track index lookup, non-debug version of
    /// the debug-only `current_track_idx`. Used by
    /// [`Self::arm_lfos_from_lanes`].
    #[inline]
    fn lookup_track_idx(&self, abs_tick: u32, song: u16) -> Option<u32> {
        self.module
            .clips
            .active_at(song, self.track_index as u8, abs_tick)
            .map(|(_, c)| c.track)
    }

    /// `true` when an `AutomationLane` of `target` has an
    /// `LfoEvent::Set` whose tick exactly equals `tick`. Used by
    /// the cell-side quirks that previously asked "does this row
    /// carry a main `Vibrato` / `Tremolo` / `Panbrello`?" by
    /// scanning `cell.effects` (DAW migration Phase 3c.3).
    #[inline]
    fn lane_has_lfo_set_at(
        &self,
        target: xmrs::core::daw::automation::AutomationTarget,
        tick: u32,
    ) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, LfoEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Lfo(LfoEvent::Set { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has an
    /// `LfoEvent::DepthOnly` whose tick exactly equals `tick`.
    /// Stand-in for the legacy "row carries a standalone
    /// `VibratoDepth`" check (FT2 vol-col-B quirk).
    #[inline]
    fn lane_has_lfo_depthonly_at(
        &self,
        target: xmrs::core::daw::automation::AutomationTarget,
        tick: u32,
    ) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, LfoEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Lfo(LfoEvent::DepthOnly { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has a
    /// `SlideEvent::Set` whose tick exactly equals `tick`. Used
    /// for the FT2 `InstrReset` quirk (`has_volume_slide`-style)
    /// and similar row-carries-a-slide questions.
    #[inline]
    fn lane_has_slide_set_at(
        &self,
        target: xmrs::core::daw::automation::AutomationTarget,
        tick: u32,
    ) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, SlideEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Slide(SlideEvent::Set { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has a
    /// `GlideEvent::Set` whose tick exactly equals `tick`.
    /// Stand-in for the legacy "row carries TonePortamento" check.
    #[inline]
    fn lane_has_glide_set_at(
        &self,
        target: xmrs::core::daw::automation::AutomationTarget,
        tick: u32,
    ) -> bool {
        use xmrs::core::daw::automation::{GlideEvent, LaneEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Glide(GlideEvent::Set { .. })))
        })
    }

    /// Convenience: "row carries TonePortamento on the active
    /// Track's `TrackPitch` lane". Resolves the current Track from
    /// the cached `(abs_tick, song)` cursor.
    #[inline]
    fn row_has_tone_portamento_lane(&self) -> bool {
        let Some(track_idx) = self.lookup_track_idx(self.current_abs_tick, self.current_song_idx)
        else {
            return false;
        };
        self.lane_has_glide_set_at(
            xmrs::core::daw::automation::AutomationTarget::TrackPitch(track_idx),
            self.current_abs_tick,
        )
    }

    /// Convenience: "row carries a main `Vibrato` on the active
    /// Track's `TrackPitch` lane".
    #[inline]
    fn row_has_vibrato_lane(&self) -> bool {
        let Some(track_idx) = self.lookup_track_idx(self.current_abs_tick, self.current_song_idx)
        else {
            return false;
        };
        self.lane_has_lfo_set_at(
            xmrs::core::daw::automation::AutomationTarget::TrackPitch(track_idx),
            self.current_abs_tick,
        )
    }

    /// Convenience: "row carries a standalone `VibratoDepth` on
    /// the active Track's `TrackPitch` lane" — XM vol-col B quirk.
    #[inline]
    fn row_has_vibrato_depth_only_lane(&self) -> bool {
        let Some(track_idx) = self.lookup_track_idx(self.current_abs_tick, self.current_song_idx)
        else {
            return false;
        };
        self.lane_has_lfo_depthonly_at(
            xmrs::core::daw::automation::AutomationTarget::TrackPitch(track_idx),
            self.current_abs_tick,
        )
    }

    /// Convenience: "row carries a `VolumeSlide` on the active
    /// Track's `TrackVolume` lane".
    #[inline]
    fn row_has_volume_slide_lane(&self) -> bool {
        let Some(track_idx) = self.lookup_track_idx(self.current_abs_tick, self.current_song_idx)
        else {
            return false;
        };
        self.lane_has_slide_set_at(
            xmrs::core::daw::automation::AutomationTarget::TrackVolume(track_idx),
            self.current_abs_tick,
        )
    }

    // `row_has_panbrello_lane` would be the convenience companion
    // to the per-row Vibrato / VolumeSlide / TonePortamento helpers,
    // but the Panbrello quirk inlines `lane_has_lfo_set_at` directly
    // (it already has `track_idx` in scope), so the helper would be
    // dead code.

    /// DAW migration Phase 3c.1 — Slide and Glide per-tick
    /// application from `AutomationLane`s. Replaces the
    /// cell-side `VolumeSlide` / `ChannelVolumeSlide` /
    /// `PanningSlide` / `Portamento` / `TonePortamento` arms
    /// (now no-ops). Reads the kind-matched lane via
    /// `lanes_for(...).find_map(state_at)` so the right kind
    /// is picked even when several lanes share the same target
    /// (e.g. Tremolo + VolumeSlide on `TrackVolume(n)` ; Vibrato
    /// + Portamento + TonePortamento on `TrackPitch(n)`).
    ///
    /// **TonePortamento goal**: kept cell-driven via
    /// `self.note` (the channel's *played* pitch, which the
    /// IT keyboard-table remap can transform into a different
    /// output note than the cell's input pitch). The lane's
    /// `target` field is the input pitch — pre-remap — so it's
    /// intentionally ignored here. The lane only contributes
    /// the `rate` and the `armed` flag; `self.note` already
    /// went through `trigger_pitch` (which composes finetune
    /// and drum-kit remap), so reading it gives the canonical
    /// goal regardless of remap topology.
    /// DAW migration Phase 3c.3 — advance the per-Track LFO
    /// state machines (Vibrato / Tremolo / Panbrello) on every tick
    /// where the corresponding lane is armed. Replaces the legacy
    /// per-tick `.tick_q()` calls that used to fire from the
    /// `TrackEffect::Vibrato/Tremolo/Panbrello` cell-side match
    /// arms.
    ///
    /// Special cases honoured:
    /// * **Vibrato @ tick 0** advances only under the IT 2.14
    ///   `it_vibrato_ticks_at_row_zero` quirk (and only when
    ///   `it_old_effects` is off).
    /// * **FT2 vol-col-B** (`volcol_b_advances_vibrato`): a row
    ///   that carries a standalone `VibratoDepth` (no main
    ///   `Vibrato` Set) keeps the LFO advancing on `tick > 0`.
    fn advance_lfos_from_lanes(&mut self, current_tick: usize) {
        use xmrs::core::daw::automation::AutomationTarget;
        let abs_tick = self.current_abs_tick;
        let song = self.current_song_idx;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };
        // ---- Vibrato (TrackPitch) ---------------------------------------
        let vib_state = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick));
        if let Some(state) = vib_state {
            if state.armed {
                let row_has_main =
                    self.lane_has_lfo_set_at(AutomationTarget::TrackPitch(track_idx), abs_tick);
                let row_has_depth_only = self
                    .lane_has_lfo_depthonly_at(AutomationTarget::TrackPitch(track_idx), abs_tick);
                if current_tick == 0 {
                    if row_has_main
                        && self.module.quirks.it_vibrato_ticks_at_row_zero
                        && !self.module.quirks.it_old_effects
                    {
                        self.effect_vibrato.tick_q();
                    }
                } else if row_has_main
                    || (row_has_depth_only && self.module.quirks.volcol_b_advances_vibrato)
                {
                    self.effect_vibrato.tick_q();
                }
            }
        }
        // ---- Tremolo (TrackVolume) --------------------------------------
        if current_tick > 0 {
            let trem_state = self
                .module
                .lanes_for(AutomationTarget::TrackVolume(track_idx))
                .find_map(|l| l.lfo_state_at(abs_tick));
            if let Some(state) = trem_state {
                if state.armed
                    && self.lane_has_lfo_set_at(AutomationTarget::TrackVolume(track_idx), abs_tick)
                {
                    self.effect_tremolo.tick_q();
                }
            }
            // ---- Panbrello (TrackPanning) ------------------------------
            let pan_state = self
                .module
                .lanes_for(AutomationTarget::TrackPanning(track_idx))
                .find_map(|l| l.lfo_state_at(abs_tick));
            if let Some(state) = pan_state {
                if state.armed
                    && self.lane_has_lfo_set_at(AutomationTarget::TrackPanning(track_idx), abs_tick)
                {
                    self.effect_panbrello.tick_q();
                }
            }
        }
    }

    fn apply_slides_from_lanes(&mut self, current_tick: usize) {
        use xmrs::core::daw::automation::AutomationTarget;
        let abs_tick = self.current_abs_tick;
        let song = self.current_song_idx;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };
        let is_tick0 = current_tick == 0;

        // VolumeSlide on TrackVolume Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackVolume(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.volume = self.volume.with_tremolo(state.rate);
            }
        }
        // ChannelVolumeSlide on TrackChannelVolume Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackChannelVolume(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.channel_volume = self.channel_volume.shifted_by(state.rate);
            }
        }
        // PanningSlide on TrackPanning Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPanning(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.panning = self.panning.shifted_by(state.rate);
            }
        }
        // TonePortamento on TrackPitch Glide lane. At tick 0,
        // recompute the slide goal from `self.note` (which
        // `tick0_load_pitch` already updated to the cell's
        // played pitch via `played_pitch_for` + finetune
        // composition — drum-kit remaps included). At tick > 0,
        // approach the goal by `state.rate`.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.glide_state_at(abs_tick))
        {
            if state.armed {
                if is_tick0 {
                    self.effect_tone_portamento_goal = self.period_helper.note_to_period(self.note);
                } else if self.period != self.effect_tone_portamento_goal {
                    self.period = self
                        .period
                        .slide_towards(self.effect_tone_portamento_goal, state.rate);
                }
            }
        }

        // Portamento on TrackPitch Slide lane. Format-specific
        // clamping (FT2 signed-overflow, ST3 `period_clamp`, IT
        // `allow_zero_period`) is preserved verbatim from the
        // pre-substitution cell-side arm.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                let speed_i16 = state.rate.raw();
                let new_period = self.period.saturating_add_signed(speed_i16);
                if self.module.quirks.ft2_pitch_slide_overflow && speed_i16 > 0 {
                    // FT2 `pitchSlideDown` signed-overflow quirk
                    // (`ft2_replayer.c:1914`): periods in
                    // `[32000, 32767]` snap to 31999; periods
                    // ≥ 32768 wrap negative under the int16
                    // cast, the clamp silently fails, and the
                    // slide freewheels. Some XM modules rely
                    // on this.
                    let wrapped = new_period.raw() as i16;
                    if (32000..=32767).contains(&(wrapped as i32)) {
                        self.period = Period::from_raw(31999);
                    } else {
                        self.period = if new_period == Period::ZERO {
                            Period::from_raw(1)
                        } else {
                            new_period
                        };
                    }
                } else if let Some((clamp_min, clamp_max)) = self.module.quirks.period_clamp {
                    // ST3 `amigalimits` clamp.
                    self.period = new_period.clamp(clamp_min, clamp_max);
                } else {
                    // Conservative clamp; IT `allow_zero_period`
                    // opens the low edge so a pitch slide can
                    // reach "infinitely high note" — ST3
                    // convention without amigalimits.
                    let min_period = if self.module.quirks.allow_zero_period {
                        Period::ZERO
                    } else {
                        Period::from_raw(1)
                    };
                    self.period = new_period.clamp(min_period, Period::from_raw(31999));
                }
            }
        }
    }

    pub(crate) fn tick0(
        &mut self,
        pattern_slot: &Cell,
        track_instr: Option<usize>,
        abs_tick: u32,
        song: u16,
        pool: &mut VoicePool<'a>,
    ) {
        self.current = pattern_slot.clone();
        self.current_track_instrument = track_instr;
        // Phase 3c.1 — cache abs_tick / song for the row's
        // tick run. `tickn_effects` and the per-tick lane
        // queries read these without re-threading from
        // sequencer state.
        self.current_abs_tick = abs_tick;
        self.current_song_idx = song;

        let delay = self.current.get_delay();
        // Always write `effect_note_delay` — whether 0 or the row's
        // SDx value. Without this, a delay latched on a previous
        // row would linger and bias downstream code that keys off
        // "is this row delayed" (see `TrackEffect::NoteRetrig`'s
        // pre-delay skip).
        self.effect_note_delay = delay;
        if delay != 0 {
            // Delayed row: tick0 loads nothing; the delay fires in
            // tickn when current_tick reaches `delay`.
        } else {
            /* load instrument then note */
            self.tick0_load_instrument_and_pitch(pool);
            if let Some(instr) = self.live_mut(pool) {
                instr.tick();
            }
            // DAW migration Phase 3c.1 — lane is the source of
            // truth for LFO params (Vibrato/Tremolo/Panbrello).
            // Runs before `tickn_effects` so the cell-side arms
            // (which only drive per-tick advance + quirks now)
            // see the correct `effect_*.data` state.
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
        // Tick ghosts every frame, whether the live voice is present,
        // delayed, or absent. Ghosts are driven purely by their own
        // envelope / fadeout state and must not stall just because
        // the live note is muted or dormant.
        self.tick_ghosts(pool);
    }
}

impl<'a> Channel<'a> {
    /// Produce one stereo sample for the channel: live voice + every
    /// still-singing ghost. Returns `None` only when both are silent
    /// — a channel whose live note has cut but whose NNA=Continue
    /// ghosts are still ringing keeps producing audio.
    ///
    /// Replaces the previous `impl Iterator for Channel` because the
    /// trait can't pass extra arguments and we now need the
    /// `VoicePool` to access ghost voices. Step 1.4 will move the
    /// live voice into the pool too, at which point this becomes a
    /// pure pool-driven mix loop.
    pub(crate) fn next_sample(&mut self, pool: &mut VoicePool<'a>) -> Option<(Amp, Amp)> {
        // Q-format integer accumulation: each contributing voice
        // is `(Amp, Amp)` Q1.15 (raw `i16`). Promote to `i32`
        // for the sum so 64+ stacked voices can't overflow the
        // accumulator before we narrow back to `Amp` at the
        // bottom. Saturating clamp on each accumulation step is
        // belt-and-braces (rare to need it in practice — a
        // typical 32-channel mix sums to ≤ ±16 of `Amp::FULL`).
        let mut left: i32 = 0;
        let mut right: i32 = 0;
        let mut any = false;

        // Live voice.
        if let Some(i) = self.live_mut(pool) {
            if let Some((l, r)) = i.next() {
                // Pure Q1.15 path. Apply `actual_volume[ch]`
                // (Q1.15) to the raw audio (Q1.15) → Q1.15
                // saturating, then accumulate in i32.
                self.actual_volume[0].apply(l).accumulate_into(&mut left);
                self.actual_volume[1].apply(r).accumulate_into(&mut right);
                any = true;
            }
        }

        // Ghost voices. Each ghost owns its frozen gain / pan,
        // so we just accumulate.
        for id in &self.ghosts {
            if let Some(g) = pool.get_mut(*id) {
                if let Some((l, r)) = g.next() {
                    l.accumulate_into(&mut left);
                    r.accumulate_into(&mut right);
                    any = true;
                }
            }
        }

        if any {
            // S91 surround: invert the right channel's phase.
            // Negate at the i32 layer so we don't clip a
            // silent-into-silent fold. The accumulator can't
            // reach `i32::MIN` (each voice contributes
            // ≤ ±0x7FFF, hundreds of voices ≪ 2^31).
            if self.surround {
                right = -right;
            }
            // Narrow back to Q1.15 with saturation. The
            // per-channel accumulator may exceed one Q1.15
            // unit when several voices stack at full volume —
            // that's expected; the final mix-bus saturation
            // happens in `voices.mix`.
            Some((Amp::from_q15_i32_sat(left), Amp::from_q15_i32_sat(right)))
        } else {
            None
        }
    }
}
