//! Per-voice resonant low-pass filter (Q-format).
//!
//! IT modules carry two kinds of filter information:
//!
//! 1. A static pair on the instrument header (`initial_filter_cutoff`
//!    and `initial_filter_resonance`), each an 8-bit register where
//!    bit 7 = "enable" and bits 0-6 = the 0..127 value.
//! 2. Dynamic control via the MIDI-macro engine.
//!
//! The DSP core is a standard RBJ cookbook biquad low-pass /
//! high-pass: two feedback taps, three feedforward taps,
//! coefficients recomputed only on cutoff / resonance changes,
//! per-sample path branch-free. Stereo processing uses
//! independent history slots.
//!
//! ## Q-format breakdown
//!
//! * Coefficients (`b0, b1, b2, a1, a2`): Q3.28 (`i32`, ±8
//!   range, 28 fractional bits).
//! * History states (`xL1..2, yL1..2, xR1..2, yR1..2`): Q15.16
//!   (`i32`, ±32768 range, 16 fractional bits).
//! * Sample input/output: `Amp` Q1.15 (`i16`).
//!
//! Per-sample MAC is `Q3.28 × Q15.16 → Q18.44` accumulated in
//! `i64`, narrowed back to Q15.16 with `>> 28`.
//!
//! Cutoff and resonance use compile-time-evaluated 128-entry
//! LUTs computed via `xmrs::fixed::tables::pow2_frac_q16_16` —
//! no `pow` / `log` / `exp` runs at audio rate (or anywhere).

use xmrs::fixed::tables::pow2_frac_q16_16;
use xmrs::fixed::units::Amp;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FilterMode {
    LowPass,
    HighPass,
}

#[derive(Clone, Debug)]
pub(crate) struct StateFilter {
    rate_hz: u32,
    pub enabled: bool,
    was_ever_enabled: bool,
    mode: FilterMode,
    cutoff_reg: u8,
    resonance_reg: u8,

    // Biquad coefficients (Q3.28, normalised so `a0 = 1`).
    b0: i32,
    b1: i32,
    b2: i32,
    a1: i32,
    a2: i32,

    // History slots (Q15.16). Stereo — independent L/R histories.
    xl1: i32,
    xl2: i32,
    yl1: i32,
    yl2: i32,
    xr1: i32,
    xr2: i32,
    yr1: i32,
    yr2: i32,
}

// --- Compile-time LUTs ----------------------------------------------

/// `2^(0.25 + r/24)` in Q16.16 for `r ∈ [0, 127]`. Multiplied by
/// 110 at runtime to obtain the cutoff frequency in Q16.16 Hz.
const CUTOFF_FACTOR_Q16_16: [u32; 128] = {
    let mut t = [0u32; 128];
    let mut r: u32 = 0;
    while r < 128 {
        // x_num = (0.25 + r/24) × 768 = 192 + 32 × r ∈ [192, 4256].
        let x_num = 192 + 32 * r;
        let int_part = x_num / 768;
        let frac_part = x_num % 768;
        let frac_factor = pow2_frac_q16_16(frac_part); // [1.0, 2.0) Q16.16
        t[r as usize] = frac_factor << int_part;
        r += 1;
    }
    t
};

/// `2 × Q` in Q16.16 for `r ∈ [0, 127]`, with the Butterworth
/// floor at `2 × 0.707 = 1.414` (raw `92682`).
///
/// `Q ≈ pow2_frac(r × 24) / 2` (≈ 0.3 % off from the exact IT
/// formula `Q = 10^(r × 3/320) / 2`, i.e. ≈ 0.2 % in `Q`).
const TWO_Q_Q16_16: [u32; 128] = {
    let mut t = [0u32; 128];
    const FLOOR: u32 = 92682; // 1.414 × 65536
    let mut r: u32 = 0;
    while r < 128 {
        let x_num = r * 24;
        let int_part = x_num / 768;
        let frac_part = x_num % 768;
        let frac_factor = pow2_frac_q16_16(frac_part);
        let v = frac_factor << int_part;
        t[r as usize] = if v > FLOOR { v } else { FLOOR };
        r += 1;
    }
    t
};

#[inline(always)]
fn mac_q3_28_q15_16(coef_q3_28: i32, state_q15_16: i32) -> i64 {
    (coef_q3_28 as i64).wrapping_mul(state_q15_16 as i64)
}

impl StateFilter {
    pub fn new(rate_hz: u32) -> Self {
        Self {
            rate_hz: rate_hz.max(1),
            enabled: false,
            was_ever_enabled: false,
            mode: FilterMode::LowPass,
            cutoff_reg: 127,
            resonance_reg: 0,
            b0: 1 << 28, // identity passthrough (b0 = 1.0 Q3.28)
            b1: 0,
            b2: 0,
            a1: 0,
            a2: 0,
            xl1: 0,
            xl2: 0,
            yl1: 0,
            yl2: 0,
            xr1: 0,
            xr2: 0,
            yr1: 0,
            yr2: 0,
        }
    }

    pub fn reset_history(&mut self) {
        self.xl1 = 0;
        self.xl2 = 0;
        self.yl1 = 0;
        self.yl2 = 0;
        self.xr1 = 0;
        self.xr2 = 0;
        self.yr1 = 0;
        self.yr2 = 0;
    }

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

    /// Recompute biquad coefficients from `cutoff_reg` /
    /// `resonance_reg`. Pure integer arithmetic — uses the
    /// compile-time `CUTOFF_FACTOR_Q16_16` / `TWO_Q_Q16_16`
    /// LUTs and the runtime `xmrs::fixed::tables::sine` LUT for
    /// trig. No `pow` / `log` / `exp` ever invoked.
    fn recompute_coefficients(&mut self) {
        // Cutoff in Q16.16 Hz: `110 × cutoff_factor`.
        let factor_q16_16 = CUTOFF_FACTOR_Q16_16[self.cutoff_reg as usize] as u64;
        let mut fc_q16_16: u64 = factor_q16_16 * 110;

        // Clamp to `[20, rate × 0.49]`. `0.49 × 65536 ≈ 32113`.
        let fc_max_q16_16: u64 = (self.rate_hz as u64) * 32113;
        let fc_min_q16_16: u64 = 20 * 65536;
        if fc_q16_16 > fc_max_q16_16 {
            fc_q16_16 = fc_max_q16_16;
        } else if fc_q16_16 < fc_min_q16_16 {
            fc_q16_16 = fc_min_q16_16;
        }

        // Phase (`omega / 2π × 65536`) for the sine LUT:
        //   omega/2π = fc/rate
        //   phase    = fc/rate × 65536 = fc_q16_16 / rate_hz.
        // The result fits in u16 by construction (fc < rate/2 →
        // phase < 32768 < 65536).
        let phase_u32: u64 = fc_q16_16 / self.rate_hz as u64;
        let phase = xmrs::fixed::units::Phase::from_raw(phase_u32 as u16);

        // sin(omega) and cos(omega) as Q1.15. `cos = sin(· + π/2)`
        // is a quarter-cycle phase shift.
        let sin_q15 = xmrs::fixed::tables::sine(phase).raw() as i32;
        let cos_q15 = xmrs::fixed::tables::sine(
            phase.shifted(xmrs::fixed::units::Phase::QUARTER),
        )
        .raw() as i32;

        // alpha = sin / (2Q). Q-format:
        //   alpha_q15 = (sin_q15 × 65536) / two_q_q16_16
        let two_q_q16_16 = TWO_Q_Q16_16[self.resonance_reg as usize] as i64;
        let alpha_q15: i32 = if two_q_q16_16 > 0 {
            ((sin_q15 as i64 * 65536) / two_q_q16_16) as i32
        } else {
            0
        };

        // Promote Q1.15 → Q3.28: `<< 13`.
        let cos_q3_28: i64 = (cos_q15 as i64) << 13;
        let one_q3_28: i64 = 1 << 28;

        let (b0_pre, b1_pre, b2_pre) = match self.mode {
            FilterMode::LowPass => {
                let one_minus_cos = one_q3_28 - cos_q3_28;
                let half = one_minus_cos / 2;
                (half, one_minus_cos, half)
            }
            FilterMode::HighPass => {
                let one_plus_cos = one_q3_28 + cos_q3_28;
                let half = one_plus_cos / 2;
                (half, -one_plus_cos, half)
            }
        };

        let alpha_q3_28: i64 = (alpha_q15 as i64) << 13;
        let a0_pre = one_q3_28 + alpha_q3_28; // > 0 always
        let a1_pre = -(cos_q3_28 << 1); // -2 cos
        let a2_pre = one_q3_28 - alpha_q3_28;

        // Normalise so `a0 = 1`. `c_normalised_q3_28 =
        // c_pre_q3_28 << 28 / a0_pre_q3_28` — the `<< 28`
        // preserves Q3.28 precision through the divide.
        let normalise = |c_pre: i64| -> i32 {
            let num = c_pre << 28;
            let q = num / a0_pre;
            if q > i32::MAX as i64 {
                i32::MAX
            } else if q < i32::MIN as i64 {
                i32::MIN
            } else {
                q as i32
            }
        };
        self.b0 = normalise(b0_pre);
        self.b1 = normalise(b1_pre);
        self.b2 = normalise(b2_pre);
        self.a1 = normalise(a1_pre);
        self.a2 = normalise(a2_pre);
    }

    /// Process one stereo Q1.15 sample pair. Returns the input
    /// unchanged for voices that never engaged the filter
    /// (typical XM/MOD/S3M case). Pure integer arithmetic.
    #[inline]
    pub fn process(&mut self, l: Amp, r: Amp) -> (Amp, Amp) {
        if !self.was_ever_enabled {
            return (l, r);
        }
        // Q1.15 → Q15.16 via `<< 1` (same numerical value).
        let l_q15_16 = l.widen_q15_16();
        let r_q15_16 = r.widen_q15_16();

        if !self.enabled {
            // Pass-through but keep history warm.
            self.xl2 = self.xl1;
            self.xl1 = l_q15_16;
            self.xr2 = self.xr1;
            self.xr1 = r_q15_16;
            return (l, r);
        }

        let yl_q18_44: i64 = mac_q3_28_q15_16(self.b0, l_q15_16)
            + mac_q3_28_q15_16(self.b1, self.xl1)
            + mac_q3_28_q15_16(self.b2, self.xl2)
            - mac_q3_28_q15_16(self.a1, self.yl1)
            - mac_q3_28_q15_16(self.a2, self.yl2);
        let yl_q15_16 = ((yl_q18_44 + (1 << 27)) >> 28) as i32;

        self.xl2 = self.xl1;
        self.xl1 = l_q15_16;
        self.yl2 = self.yl1;
        self.yl1 = yl_q15_16;

        let yr_q18_44: i64 = mac_q3_28_q15_16(self.b0, r_q15_16)
            + mac_q3_28_q15_16(self.b1, self.xr1)
            + mac_q3_28_q15_16(self.b2, self.xr2)
            - mac_q3_28_q15_16(self.a1, self.yr1)
            - mac_q3_28_q15_16(self.a2, self.yr2);
        let yr_q15_16 = ((yr_q18_44 + (1 << 27)) >> 28) as i32;

        self.xr2 = self.xr1;
        self.xr1 = r_q15_16;
        self.yr2 = self.yr1;
        self.yr1 = yr_q15_16;

        // Q15.16 → Q1.15 via `>> 1` with round-half + saturation.
        let to_amp = |v: i32| -> Amp {
            // Round-half-up before halving; `from_q15_i32_sat`
            // clamps the result to the i16 range.
            Amp::from_q15_i32_sat((v + 1) >> 1)
        };

        (to_amp(yl_q15_16), to_amp(yr_q15_16))
    }
}
