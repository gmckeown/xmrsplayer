//! Effect-plugin module.
//!
//! Historically defined a `EffectPlugin` trait with `tick(&mut)
//! -> f32` and friends, used by `EffectArpeggio` and
//! `EffectVibratoTremolo`. The Q-format migration removed the
//! last `f32` consumers; the trait was unused (no `dyn`, no
//! generics, identical method names called on each concrete
//! type) and has been deleted entirely. Inherent methods now
//! return Q-format types or `()`.
