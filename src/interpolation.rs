/// Sample interpolation algorithm used during playback.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Interpolation {
    /// Point sampling / zero-order hold (nearest-neighbor).
    /// Holds the sample byte flat until the next step, matching authentic Amiga Paula DAC behavior.
    Nearest = 0,
    /// Linear interpolation between adjacent sample frames.
    #[default]
    Linear = 1,
}
