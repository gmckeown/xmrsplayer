use xmrs::core::fixed::fixed::Q8_8;
use xmrs::core::fixed::units::PitchDelta;
/// An Instrument Vibrato State
use xmrs::core::vibrato::Vibrato;
use xmrs::core::waveform::WaveformState;
use xmrs::tracker::period::{FrequencyType, PeriodHelper};

/// Auto-vibrato runtime state. Q-format throughout: `phase` is a
/// free-running `u32` Q.16 cycle accumulator (full cycle =
/// `0x10000`), `current_modulation` is a [`PitchDelta`] (Q8.8
/// semitones). Replaces the previous f32 implementation that
/// silently masked the precision difference between linear and
/// Amiga frequency modes.
#[derive(Clone)]
pub struct StateAutoVibrato<'a> {
    vibrato: &'a Vibrato,
    wfs: WaveformState,
    period_helper: PeriodHelper,
    /// Free-running phase accumulator in `u32` Q.16 cycles.
    /// `0x10000` represents one full LFO cycle. Used both for
    /// the wave LUT (low 16 bits as a `u16` phase) and for the
    /// sweep ramp (compared in Q.16 against `sweep_q16`).
    phase: u32,
    pub current_modulation: PitchDelta,
}

impl<'a> StateAutoVibrato<'a> {
    pub fn new(vibrato: &'a Vibrato, period_helper: PeriodHelper) -> Self {
        let mut sv = Self {
            vibrato,
            wfs: WaveformState::new(vibrato.waveform),
            period_helper,
            phase: 0,
            current_modulation: PitchDelta::ZERO,
        };

        sv.reset();

        sv
    }

    pub fn reset(&mut self) {
        self.retrig();
    }

    pub fn retrig(&mut self) {
        self.phase = 0;
        self.current_modulation = PitchDelta::ZERO;
    }

    /// Convert a Q8.8 cycle count into the matching Q.16 phase
    /// scale. `Q8.8 raw × 256 = Q.16 raw` (one extra factor of
    /// 256 to align the fractional widths).
    #[inline]
    fn q8_8_to_q16(q: Q8_8) -> u32 {
        // Negative values are not meaningful for speed/sweep —
        // the importer never emits them — so a simple
        // saturating-non-negative cast is correct.
        q.raw().max(0) as u32 * 256
    }

    pub fn tick(&mut self, _sustain: bool) {
        let speed_q16 = Self::q8_8_to_q16(self.vibrato.speed);
        let sweep_q16 = Self::q8_8_to_q16(self.vibrato.sweep);
        let depth_raw = self.vibrato.depth.as_q8_8_i32(); // Q8.8 i16 widened.

        // Advance the phase accumulator. `wrapping_add` is safe
        // here — the `u32` headroom is enormous (~4.2 M ticks
        // before wrap at typical `speed = 0x10` raw, i.e. several
        // days at 50 ticks/s).
        self.phase = self.phase.wrapping_add(speed_q16);

        // Sweep-in: auto-vibrato depth ramps from 0 to `depth`
        // over the first `sweep_q16` units of phase accumulation.
        // Once `phase >= sweep_q16` the LFO runs at full depth
        // for the rest of the voice's life — including after
        // key-off (sustain = false).
        //
        // `sweep == 0` means "no ramp, go straight to full
        // depth" in IT (ITTECH) — handled explicitly to avoid a
        // divide-by-zero.
        let current_depth_raw: i32 = if sweep_q16 == 0 || self.phase >= sweep_q16 {
            depth_raw
        } else {
            // `(phase / sweep) × depth` with rounding. Use `i64`
            // to keep the multiply lossless for the worst-case
            // `phase ≈ sweep ≈ 0xFFFF`, `depth ≈ i16::MAX`.
            let num = self.phase as i64 * depth_raw as i64;
            (num / sweep_q16 as i64) as i32
        };

        // Wave LUT input: low 16 bits of `phase` (naturally
        // wraps at one cycle).
        let wave = self
            .wfs
            .value_q15(xmrs::core::fixed::units::Phase::from_raw(self.phase as u16));

        // Modulation: `current_depth × wave / 32768` keeps
        // output in Q8.8 semitones.
        let mut modulation = (current_depth_raw * wave.raw() as i32) >> 15;

        // Amiga period scale is 4× coarser than linear, so the
        // semitone delta produced by the LFO has 4× less effect
        // on the period in Amiga mode. Pre-divide here so the
        // downstream pitch path sees the same audible vibrato
        // depth in both modes.
        if let FrequencyType::AmigaFrequencies = self.period_helper.freq_type {
            modulation >>= 2;
        }

        self.current_modulation = PitchDelta::from_q8_8_i32_sat(modulation);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xmrs::core::fixed::fixed::Q8_8;
    use xmrs::core::fixed::units::PitchDelta;
    use xmrs::core::waveform::Waveform;

    /// Regression test for the `binary_world.xm` symptom: auto-
    /// vibrato instruments (inst 14: rate=15, depth=5, sweep=65)
    /// should produce non-zero `current_modulation` after a single
    /// tick. If this test fails, the import or the `to_f32()`
    /// bridge has zeroed the vibrato parameters.
    #[test]
    fn binary_world_inst14_auto_vibrato_is_nonzero() {
        // XM importer mapping (mirrors xmrs::tracker::import::xm::xminstrument):
        //   speed = byte / (63 * 4) = byte / 252
        //   depth = byte / (15 * 2) = byte / 30
        //   sweep = byte / 255
        let vibrato = Vibrato {
            waveform: Waveform::TranslatedSine,
            speed: Q8_8::from_ratio(15, 63 * 4),
            depth: PitchDelta::from_ratio(5, 15 * 2),
            sweep: Q8_8::from_ratio(65, 255),
        };

        // Sanity: the Q-format storage must hold non-zero values.
        // If any of these fail, the importer is producing zeros.
        eprintln!(
            "[auto-vibrato test] raw values: speed={} depth={} sweep={}",
            vibrato.speed.raw(),
            vibrato.depth.as_q8_8_i16(),
            vibrato.sweep.raw(),
        );

        assert_ne!(
            vibrato.speed.raw(),
            0,
            "vibrato.speed Q8_8 raw is zero — Q8_8::from_ratio(15, 252) gave 0?",
        );
        assert_ne!(
            vibrato.depth.as_q8_8_i16(),
            0,
            "vibrato.depth PitchDelta raw is zero — PitchDelta::from_ratio(5, 30) gave 0?",
        );
        assert_ne!(
            vibrato.sweep.raw(),
            0,
            "vibrato.sweep Q8_8 raw is zero — Q8_8::from_ratio(65, 255) gave 0?",
        );

        // Q-format raw range checks (replace the OLD f32 ≈ 0.0586 /
        // 0.164 / 0.254 expectations). At Q8.8 (0.0586 → 15, 0.254 →
        // 65) and PitchDelta Q8.8 (0.164 → 42 raw):
        let speed_raw = vibrato.speed.raw();
        let depth_raw = vibrato.depth.as_q8_8_i16();
        let sweep_raw = vibrato.sweep.raw();
        assert!(
            speed_raw > 12 && speed_raw < 18,
            "speed raw = {} (expected ≈ 15 = 0.0586 in Q8.8)",
            speed_raw
        );
        assert!(
            depth_raw > 38 && depth_raw < 46,
            "depth raw = {} (expected ≈ 42 = 0.164 in Q8.8)",
            depth_raw
        );
        assert!(
            sweep_raw > 60 && sweep_raw < 70,
            "sweep raw = {} (expected ≈ 65 = 0.254 in Q8.8)",
            sweep_raw
        );

        // Now drive a few ticks of the LFO and verify the
        // modulation actually moves.
        let ph = PeriodHelper::new(FrequencyType::LinearFrequencies, false);
        let mut sv = StateAutoVibrato::new(&vibrato, ph);
        assert_eq!(
            sv.current_modulation,
            PitchDelta::ZERO,
            "initial modulation"
        );

        // Tick 1: should produce non-zero modulation (sweep is
        // ramping in, but the waveform value is large).
        sv.tick(true);
        eprintln!(
            "[auto-vibrato test] tick 1 modulation raw = {}",
            sv.current_modulation.as_q8_8_i16()
        );
        assert_ne!(
            sv.current_modulation.as_q8_8_i16(),
            0,
            "tick 1: current_modulation raw is 0 (expected non-zero LFO output)",
        );

        // After ~10 ticks the sweep is fully done and the LFO is
        // running at full depth. Verify the modulation reaches at
        // least 80 % of the depth at some point during the ramp.
        // Compare in raw `i32` to keep the test integer-only.
        let depth_raw = vibrato.depth.as_q8_8_i16().abs() as i32;
        let mut peak_raw: i32 = sv.current_modulation.as_q8_8_i16().abs() as i32;
        for _ in 0..30 {
            sv.tick(true);
            peak_raw = peak_raw.max(sv.current_modulation.as_q8_8_i16().abs() as i32);
        }
        eprintln!(
            "[auto-vibrato test] peak modulation raw over 30 ticks = {} (depth raw = {})",
            peak_raw, depth_raw,
        );
        // 80 % of depth in raw Q8.8: `depth_raw × 4 / 5`.
        assert!(
            peak_raw * 5 > depth_raw * 4,
            "peak modulation raw = {} (expected ≳ 80 % of depth raw = {})",
            peak_raw,
            depth_raw,
        );
    }
}
