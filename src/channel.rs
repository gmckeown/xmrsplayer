//! Channel state and per-channel runtime — submodule root.
//!
//! The struct + accessor surface lives here; the rest is split
//! across (private) sibling submodules:
//!
//! * `voice` — live/ghost handles, install / drop / promote, key-off
//!   and note-fade primitives.
//! * `ghosts` — NNA, DCT, past-note effects.
//! * `macros` — IT MIDI macro interpreter.
//! * `triggers` — `trigger_pitch` and per-tick instrument tick.
//! * `tick` — `tick`, `tickn_effects`, NoteDelay/Retrig/Tremor.
//! * `tick0` — row-start loaders + the public `tick0` entry.
//! * `lanes` — AutomationLane queries + LFO arming + slides.
//! * `mix` — `next_sample`.

mod ghosts;
mod lanes;
mod macros;
mod mix;
mod tick;
mod tick0;
mod triggers;
mod voice;

// `Vec` is needed in the struct definition for `ghosts:
// Vec<VoiceId>`. Other places in the channel/ submodules import
// it locally as needed.
use alloc::vec::Vec;

use crate::effect_arpeggio::EffectArpeggio;
use crate::effect_vibrato_tremolo::EffectVibratoTremolo;
use crate::voice_pool::VoiceId;
use xmrs::core::fixed::fixed::Q8_8;
use xmrs::core::fixed::units::Pitch as PitchQ;
use xmrs::core::fixed::units::{ChannelVolume, Panning, Period, PitchDelta, SampleRate, Volume};
use xmrs::prelude::*;

use self::tick::NoteRetrigState;

/// Compose the played note (`Pitch::value() as i16` semitones)
/// with the sample's finetune contribution (semitones) into the
/// `Channel.note` field's Q8.8 `PitchQ`.
///
/// `played.value()` is `0..=119` (`C0..=B9`); `finetune` is a
/// `PitchDelta` Q8.8 carrying at most `relative_pitch as i16 +
/// ±1 semitone`. The worst-case raw sum
/// `(119 + 96 + 1) × 256 = 55296` overflows i16, so the result
/// saturates at high keys with high `relative_pitch` — the same
/// note-119 clamp the downstream period table applies (XM/IT
/// reference players clamp there too).
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

    /// Last triggered note in Q8.8 semitones. Bakes in finetune
    /// from the sample at trigger time. Used by `TonePortamento`
    /// to recompute the target period and by the arpeggio quirk
    /// clamp (FT2). Distinct from [`Self::current_note`]
    /// (integer-semitone enum).
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
    /// Used by [`Channel::apply_slides_and_glide_from_lanes`] to
    /// query `AutomationLane`s in song-tick coordinates without
    /// re-threading the sequencer's state through every
    /// per-tick call.
    pub(crate) current_abs_tick: u32,
    /// Current sub-song index. Set alongside
    /// [`Self::current_abs_tick`].
    pub(crate) current_song_idx: u16,

    /// Cached `Track.instrument` for the cell currently held in
    /// [`Self::current`]. Instruments live on the owning Track,
    /// not in `Cell`, so the replayer pushes the resolved value
    /// alongside the cell at each row-start dispatch. `None` for
    /// effect-only tracks (the
    /// [`crate::daw::build_timeline::EFFECT_ONLY_INSTRUMENT`] sentinel
    /// surfaces here as `None`) and for channels with no active clip.
    pub(crate) current_track_instrument: Option<usize>,

    /// Current period (Q-format). Slides accumulate `i16` deltas
    /// via `Period::saturating_add_signed`.
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
    /// `Voices::new`) and never modified afterwards. Used to look
    /// up the channel's lanes on the timeline.
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
    /// Forward a tempo change to the arpeggio effect. Called by the
    /// player on every step so the FT2 arpeggio LUT is indexed from
    /// the current speed rather than a stale copy.
    pub(crate) fn set_tempo(&mut self, tempo: usize) {
        self.effect_arpeggio.set_tempo(tempo);
    }

    pub fn is_muted(&self) -> bool {
        self.muted || self.instr_midi_mute
    }
}
