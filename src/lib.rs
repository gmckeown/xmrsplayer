#![forbid(unsafe_code)]
#![cfg_attr(not(feature = "std"), no_std)]

extern crate alloc;

#[cfg(not(any(feature = "std", feature = "libm", feature = "micromath")))]
::core::compile_error!("Must enable at least one of features `std`, `libm`, or `micromath`");

pub(crate) mod effect;
pub(crate) mod triggerkeep;

pub(crate) mod effect_arpeggio;
pub(crate) mod effect_vibrato_tremolo;

pub mod channel;
pub(crate) mod helper;
pub mod prelude;
pub(crate) mod state_auto_vibrato;
pub(crate) mod state_envelope;
pub(crate) mod state_instr_default;
pub(crate) mod state_sample;

pub mod xmrsplayer;
