//! Player-side gate for humanisation of
//! `InstrumentType::Euclidean` pulses.
//!
//! The instrument owns the *shape* (probability + max-advance) via
//! [`xmrs::core::instr_ekn::InstrEkn`]; the player owns the *execution* —
//! whether and how to draw randomness for each pulse.
//!
//! Default is [`HumanizeMode::Disabled`](crate::humanize::HumanizeMode::Disabled):
//! the player ignores the humanisation parameters and stays strictly
//! cycle-exact.

/// Player-side humanisation policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HumanizeMode {
    /// The player ignores `InstrEkn` humanisation parameters.
    /// Output is strictly deterministic and cycle-exact — default
    /// for tests and offline rendering oracles.
    #[default]
    Disabled,
    /// Fresh entropy per playback: every render of the same module
    /// sounds slightly different. The exact seed source is
    /// unspecified (the player picks one); not reproducible.
    Live,
    /// Apply humanisation with a caller-supplied seed. Same seed →
    /// same rendered output.
    Deterministic(u32),
}
