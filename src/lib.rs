#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

pub(crate) mod triggerkeep;

pub(crate) mod effect_arpeggio;
pub(crate) mod effect_vibrato_tremolo;

pub mod channel;
pub mod prelude;
pub(crate) mod state_auto_vibrato;
pub(crate) mod state_envelope;
pub(crate) mod state_filter;
pub(crate) mod state_instr_default;
pub(crate) mod state_sample;
pub(crate) mod voice;
pub(crate) mod voice_mod;

/// Generational arena of `Voice`s, indexed by opaque `VoiceId`
/// handles. Owns every live and ghost voice the player has in
/// flight; channels carry only handles. Capacity is configurable
/// at construction (default 256, mirroring schism's `MAX_VOICES`)
/// and the pool falls back to lowest-audible-volume eviction when
/// full — see [`voice_pool::VoicePool::allocate_live`] /
/// [`voice_pool::VoicePool::allocate_ghost`].
pub(crate) mod voice_pool;

// --- Post-refactor modules (0.10) ---

/// Song cursor and tick clock. Consumes navigation-side global effects
/// (`Fxx`, `Bxx`, `Dxx`, `E6y`, `EEy`, `BPM`, `BPMSlide`) directly from
/// pattern cells, without going through channels.
pub mod sequencer;

/// Audio engine: per-channel state, mixer gain, and sample generation.
/// Consumes volume-side global effects (`Volume`, `VolumeSlide`).
pub mod voices;

/// Subscription API — `PlayerObserver` trait and event context structs.
pub mod observer;

/// Audio-side subscription API — `MixObserver` and `ChannelsObserver`
/// traits for consumers of the produced audio stream (VU-meters,
/// oscilloscopes, recorders, analyzers).
pub mod audio_observer;

/// MIDI-side subscription API — `MidiObserver` trait for consumers
/// of MIDI events emitted by the IT macro interpreter (`Zxx` macros
/// that encode Note On/Off, CC, Program Change, SysEx, etc.).
pub mod midi_observer;

/// Built-in observer that prints pattern/row changes. Requires `std`.
#[cfg(feature = "std")]
pub mod debug_observer;

/// Built-in MIDI observer that prints MIDI events to stdout. Requires
/// `std`. Mirrors `debug_observer` but for the `MidiObserver` trait.
#[cfg(feature = "std")]
pub mod debug_midi_observer;

pub mod xmrsplayer;

/// Audition a single instrument/sample of a module that has no song
/// (e.g. a David Whittaker `*-sfx.dw` bank) by splicing a temporary
/// one-note timeline into it in place, rendering, then restoring it —
/// no second module, no instrument cloning.
pub mod sample_preview;

/// Toggle and policy for humanisation of
/// `Track::Euclidean` pulses at playback.
pub mod humanize;

/// Per-channel state for Euclidean humanisation, coordinated by
/// the player.
pub(crate) mod state_humanize_ekn;
