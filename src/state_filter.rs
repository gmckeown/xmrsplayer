//! Per-voice resonant low-pass filter.
//!
//! IT modules carry two kinds of filter information:
//!
//! 1. A static pair on the instrument header (`initial_filter_cutoff`
//!    and `initial_filter_resonance`), each an 8-bit register where
//!    bit 7 = "enable" and bits 0-6 = the 0..127 value. Applied at
//!    note trigger time.
//! 2. Dynamic control via the MIDI-macro engine (SFx / Zxx / $xx /
//!    `\xx`) — not yet implemented, see `IT_ROADMAP.md §4`.
//!
//! The DSP core is a standard RBJ cookbook biquad low-pass: two
//! feedback taps, three feedforward taps, coefficients recomputed
//! only on cutoff / resonance changes, per-sample path branch-free.
//! Stereo processing uses independent history slots (`yL`, `yR`) so
//! a ping-pong or wide sample doesn't fold its channels together.
//!
//! # Cutoff mapping
//!
//! Per ITTECH:
//!
//! ```text
//! freq = 110 * 2^(0.25 + cutoff_reg * 512 / (24 * 512))
//!      = 110 * 2^(0.25 + cutoff_reg / 24)
//! ```
//!
//! At `cutoff_reg = 0` this is ~131 Hz; at `cutoff_reg = 127` it's
//! ~5.1 kHz. The cutoff is then clamped to `fs/2 - margin` to keep
//! the biquad stable when the sample rate and the register together
//! push above Nyquist.
//!
//! # Resonance mapping
//!
//! ITTECH gives a damping factor
//! `d = 10^(-resonance_reg * 24 / (128 * 20))` with `d=1` at
//! `resonance_reg=0` (no resonance) and `d≈0.065` at 127 (max
//! peaking). We convert to a biquad `Q` via `Q ≈ 1 / (2*d)`, which
//! lands around 0.5..8 — the usable musical range for LPF resonance.
//! A floor of 0.707 (Butterworth) prevents the low-resonance case
//! from over-damping audibly.

// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

use core::f32::consts::PI;

/// Filter response shape. IT's MIDI-macro `F0 F0 02 xx` selects
/// between low-pass (default, `xx` bit 4 clear) and high-pass
/// (`xx` bit 4 set). Bit 5 of `xx` disables the filter entirely
/// (bypass regardless of cutoff/resonance values).
///
/// OpenMPT / Schism support a few more variants (extended filter
/// modes) that aren't implemented here; they decay to `LowPass`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FilterMode {
    LowPass,
    HighPass,
}

#[derive(Clone, Debug)]
pub(crate) struct StateFilter {
    /// Sample rate in Hz.
    rate: f32,
    /// `true` when the biquad path is active. When `false`, `process`
    /// returns the input untouched (pass-through).
    pub enabled: bool,
    /// `true` once the filter has been enabled at any point during
    /// this voice's lifetime. Latched on first engagement (any of
    /// `configure_from_it_registers` with bit-7 set, `set_cutoff_reg`,
    /// `set_resonance_reg`, or `set_mode_from_macro` not in disable
    /// mode) and never cleared. Drives the per-sample fast path in
    /// [`Self::process`]: voices that never touch their filter — i.e.
    /// XM / MOD / S3M voices, and IT voices whose instrument header
    /// has both filter bytes bit-7-clear and whose pattern never
    /// fires a filter MIDI macro — short-circuit out of `process`
    /// without paying the four "warm input history" stores. Voices
    /// that have engaged the filter at least once keep the warm-
    /// history path so a subsequent re-engagement (rare, only via
    /// MIDI-macro automation) doesn't crash the biquad into a zero-
    /// initialised history state.
    ///
    /// Schism gets this for free via `MIXNDX_FILTER` static dispatch
    /// at the voice-setup level (`mixer.c:714`); xmrsplayer's mixer
    /// doesn't have an equivalent flag-driven dispatch table, so we
    /// gate inside `process` instead.
    was_ever_enabled: bool,
    /// Response shape — `LowPass` (default) or `HighPass`. Affects
    /// how `recompute_coefficients` derives the biquad numerator.
    mode: FilterMode,
    /// Last cutoff register written (0..127). Cached so we can skip
    /// the coefficient recompute when nothing moved.
    cutoff_reg: u8,
    /// Last resonance register written (0..127). Cached for the same
    /// reason.
    resonance_reg: u8,

    // --- Biquad coefficients (normalised so a0 == 1). ---
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,

    // --- History slots. Stereo — independent L/R histories avoid
    // cross-talk on ping-pong or true-stereo samples.
    xl1: f32,
    xl2: f32,
    yl1: f32,
    yl2: f32,
    xr1: f32,
    xr2: f32,
    yr1: f32,
    yr2: f32,
}

impl StateFilter {
    pub fn new(rate: f32) -> Self {
        Self {
            rate,
            enabled: false,
            was_ever_enabled: false,
            mode: FilterMode::LowPass,
            cutoff_reg: 127,
            resonance_reg: 0,
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            xl1: 0.0,
            xl2: 0.0,
            yl1: 0.0,
            yl2: 0.0,
            xr1: 0.0,
            xr2: 0.0,
            yr1: 0.0,
            yr2: 0.0,
        }
    }

    /// Zero the biquad's delay line. Called at note trigger so the
    /// filter doesn't carry a click / decay pattern from whatever
    /// was playing before.
    pub fn reset_history(&mut self) {
        self.xl1 = 0.0;
        self.xl2 = 0.0;
        self.yl1 = 0.0;
        self.yl2 = 0.0;
        self.xr1 = 0.0;
        self.xr2 = 0.0;
        self.yr1 = 0.0;
        self.yr2 = 0.0;
    }

    /// Configure the filter from the raw IT instrument-header bytes.
    /// Bit 7 of either register acts as an enable flag (OpenMPT /
    /// Schism convention); clearing both bits 7 leaves the filter
    /// disabled (pass-through). Stateless with respect to history —
    /// call `reset_history` separately when you want a clean start.
    pub fn configure_from_it_registers(&mut self, cutoff_reg: u8, resonance_reg: u8) {
        let cutoff_enabled = cutoff_reg & 0x80 != 0;
        let resonance_enabled = resonance_reg & 0x80 != 0;
        self.enabled = cutoff_enabled || resonance_enabled;
        if !self.enabled {
            return;
        }
        self.was_ever_enabled = true;
        self.cutoff_reg = cutoff_reg & 0x7F;
        self.resonance_reg = resonance_reg & 0x7F;
        self.recompute_coefficients();
    }

    /// Set the cutoff register directly (0..127) without touching
    /// resonance or the enable flag. Used by the pitch-envelope-as-
    /// filter routing (and eventually MIDI macros) to modulate
    /// cutoff dynamically. Force-enables the filter the first time
    /// this is called on a voice whose header registers were both
    /// bit-7-clear — matching OpenMPT / Schism: dynamic control
    /// engages the filter regardless of the instrument's static
    /// settings. Skips coefficient recompute when the register
    /// didn't move, to avoid trig/exp churn on envelopes that hold
    /// a flat value for many ticks.
    pub fn set_cutoff_reg(&mut self, cutoff_reg: u8) {
        let new = cutoff_reg.min(127);
        if self.cutoff_reg == new && self.enabled {
            return;
        }
        self.cutoff_reg = new;
        self.enabled = true;
        self.was_ever_enabled = true;
        self.recompute_coefficients();
    }

    /// Set the resonance register directly (0..127). Mirrors
    /// `set_cutoff_reg`: force-engages the filter on first call,
    /// skips coefficient recompute when unchanged.
    pub fn set_resonance_reg(&mut self, resonance_reg: u8) {
        let new = resonance_reg.min(127);
        if self.resonance_reg == new && self.enabled {
            return;
        }
        self.resonance_reg = new;
        self.enabled = true;
        self.was_ever_enabled = true;
        self.recompute_coefficients();
    }

    /// Apply a filter-mode macro byte (`F0 F0 02 xx`).
    ///
    /// Bit layout of `xx`:
    ///   bit 4 (0x10) — high-pass mode when set
    ///   bit 5 (0x20) — disable filter (bypass)
    ///
    /// All other bits are reserved and ignored. Cleared mode bits
    /// default to low-pass, which is the IT canonical default.
    ///
    /// Unlike `set_cutoff_reg` / `set_resonance_reg`, this does NOT
    /// force-enable the filter — the caller's `xx` can explicitly
    /// disable via bit 5.
    pub fn set_mode_from_macro(&mut self, xx: u8) {
        let disable = xx & 0x20 != 0;
        let hp = xx & 0x10 != 0;

        if disable {
            self.enabled = false;
            return;
        }

        let new_mode = if hp {
            FilterMode::HighPass
        } else {
            FilterMode::LowPass
        };
        if new_mode != self.mode {
            self.mode = new_mode;
            if self.enabled {
                self.recompute_coefficients();
            }
        }
    }

    /// Recompute biquad coefficients from the cached `cutoff_reg` /
    /// `resonance_reg`. Called only when those registers change; the
    /// per-sample path never reaches this.
    fn recompute_coefficients(&mut self) {
        // Cutoff frequency in Hz, IT formula.
        let fc = 110.0 * powf(2.0, 0.25 + (self.cutoff_reg as f32) / 24.0);
        // Clamp to just under Nyquist to keep the biquad stable —
        // IT cutoff can easily exceed `fs/2` at low sample rates.
        let fc = fc.min(self.rate * 0.49).max(20.0);

        // Q derivation from IT resonance register — see module doc.
        let d = powf(10.0, -(self.resonance_reg as f32) * 24.0 / (128.0 * 20.0));
        let q = (1.0 / (2.0 * d)).max(0.707);

        // RBJ cookbook — shared poles (a1, a2 and the normalising
        // a0). The numerator (b0..b2) switches on mode: LPF uses
        // `(1 - cos_w) * {0.5, 1.0, 0.5}`, HPF uses `(1 + cos_w) *
        // {0.5, -1.0, 0.5}`.
        let omega = 2.0 * PI * fc / self.rate;
        let sin_w = sinf(omega);
        let cos_w = cosf(omega);
        let alpha = sin_w / (2.0 * q);

        let (b0, b1, b2) = match self.mode {
            FilterMode::LowPass => {
                let k = (1.0 - cos_w) * 0.5;
                (k, 1.0 - cos_w, k)
            }
            FilterMode::HighPass => {
                let k = (1.0 + cos_w) * 0.5;
                (k, -(1.0 + cos_w), k)
            }
        };
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w;
        let a2 = 1.0 - alpha;

        // Normalise so `a0 = 1` — lets the per-sample loop drop one
        // division.
        let inv_a0 = 1.0 / a0;
        self.b0 = b0 * inv_a0;
        self.b1 = b1 * inv_a0;
        self.b2 = b2 * inv_a0;
        self.a1 = a1 * inv_a0;
        self.a2 = a2 * inv_a0;
    }

    /// Process one stereo sample pair through the biquad. Returns the
    /// input untouched when the filter has never been engaged for this
    /// voice (the typical XM/MOD/S3M case) or when it's currently in
    /// pass-through. The two paths differ in their bookkeeping:
    ///
    /// 1. **Never engaged** (`was_ever_enabled == false`): true zero-
    ///    cost pass-through. No state to maintain, because no
    ///    subsequent `set_cutoff_reg` / `set_resonance_reg` /
    ///    `set_mode_from_macro` will ever fire on this voice. Saves
    ///    four `f32` writes per sample per voice — at 32 channels ×
    ///    ~10 voices × 48 kHz that's ~60M useless stores per second
    ///    on filter-free modules.
    ///
    /// 2. **Currently bypassed but previously engaged** (`enabled ==
    ///    false`, `was_ever_enabled == true`): keep input history
    ///    current so a future re-engagement (rare, only via MIDI-
    ///    macro automation) doesn't crash the biquad into a zero-
    ///    initialised history state and click. This path is reached
    ///    only on IT modules that toggle their filter dynamically.
    ///    Output history stays zero — nothing valid to store there
    ///    until the filter produces output.
    ///
    /// Schism's mixer dispatches statically on the `MIXNDX_FILTER`
    /// flag (`mixer.c:714`), so its filter-free voices pay zero
    /// instructions. xmrsplayer's mixer doesn't have that level of
    /// dispatch flexibility, so we gate inside `process` instead —
    /// effectively the same fast-path, just one branch later.
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        if !self.was_ever_enabled {
            return (l, r);
        }
        if !self.enabled {
            self.xl2 = self.xl1;
            self.xl1 = l;
            self.xr2 = self.xr1;
            self.xr1 = r;
            return (l, r);
        }
        // Left channel.
        let yl = self.b0 * l + self.b1 * self.xl1 + self.b2 * self.xl2
            - self.a1 * self.yl1
            - self.a2 * self.yl2;
        self.xl2 = self.xl1;
        self.xl1 = l;
        self.yl2 = self.yl1;
        self.yl1 = yl;

        // Right channel.
        let yr = self.b0 * r + self.b1 * self.xr1 + self.b2 * self.xr2
            - self.a1 * self.yr1
            - self.a2 * self.yr2;
        self.xr2 = self.xr1;
        self.xr1 = r;
        self.yr2 = self.yr1;
        self.yr1 = yr;

        (yl, yr)
    }
}

// --- Small float shims so `no_std` builds without `std` pull the
//     right trait method for powf / sinf / cosf. When `std` is on,
//     the inherent `f32::powf` / `sin` / `cos` win via the trait
//     imports above.

#[inline]
fn powf(x: f32, y: f32) -> f32 {
    x.powf(y)
}

#[inline]
fn sinf(x: f32) -> f32 {
    x.sin()
}

#[inline]
fn cosf(x: f32) -> f32 {
    x.cos()
}
