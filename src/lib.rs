#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

// `no_std` builds need a float math backend because `f32`'s transcendental
// methods (`sqrt`, `sin`, `log2`, ...) live in `std`. Users must opt into
// exactly one of `libm` or `micromath` when `std` is disabled.
#[cfg(not(any(feature = "std", feature = "libm", feature = "micromath")))]
::core::compile_error!(
    "xmrsplayer: with `std` disabled, enable exactly one float math backend: \
     feature `libm` or feature `micromath`."
);

pub(crate) mod effect;
pub(crate) mod triggerkeep;

pub(crate) mod effect_arpeggio;
pub(crate) mod effect_vibrato_tremolo;

pub mod channel;
pub(crate) mod helper;
pub mod prelude;
pub(crate) mod state_auto_vibrato;
pub(crate) mod state_envelope;
pub(crate) mod state_filter;
pub(crate) mod state_instr_default;
pub(crate) mod state_sample;
pub(crate) mod voice;

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

/// Built-in observer that prints pattern/row changes. Requires `std`.
#[cfg(feature = "std")]
pub mod debug_observer;

pub mod xmrsplayer;
