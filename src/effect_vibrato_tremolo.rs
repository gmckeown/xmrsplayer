use xmrs::core::fixed::fixed::{Q15, Q8_8};
use xmrs::core::fixed::units::PitchDelta;
use xmrs::{core::waveform::WaveformState, prelude::Waveform};

/// LFO parameter pack. `speed` is the phase increment per tick
/// (Q8.8 cycles/tick — fractional in practice, e.g. `4 raw =
/// 1/64` for an XM 4xy with `x=1`). `depth` is the raw amplitude;
/// the type-level interpretation depends on which LFO instance
/// this struct belongs to:
///
/// * Vibrato → Q8.8 semitones (numerically equivalent to
///   [`PitchDelta::raw`]).
/// * Tremolo → Q1.15 volume amplitude.
/// * Panbrello → Q1.15 panning amplitude.
///
/// The LFO body itself never needs to know which interpretation
/// applies — it only multiplies `depth × wave_q15` and the
/// *consumer* picks the right typed accessor
/// ([`EffectVibratoTremolo::value_pitch_delta`] or
/// [`EffectVibratoTremolo::value_q15`]).
#[derive(Default, Clone, Copy, Debug)]
pub struct VibratoTremolo {
    pub waveform: WaveformState,
    pub speed: Q8_8,
    pub depth: i16,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct EffectVibratoTremolo {
    pub data: VibratoTremolo,
    in_progress: bool,
    /// Phase, `0x0000..=0xFFFF` = `[0, 1)` cycle. Wraps naturally
    /// at the `u16` boundary on every tick.
    pos: u16,
    /// Raw output, same Q-format as `data.depth`. Computed once
    /// per tick as `depth × wave_q15 / 32768`.
    value: i16,
}

impl EffectVibratoTremolo {
    pub fn new(wf: Waveform) -> Self {
        let mut evt = Self::default();
        evt.data.waveform = WaveformState::new(wf);
        evt
    }

    /// Zero the output modulation while preserving the LFO
    /// phase. Use this in preference to [`Self::retrigger_q`]
    /// when the effect merely disappears from the next row —
    /// `retrigger_q` additionally resets the phase and should
    /// only be called on new-note triggers.
    pub fn clear_output(&mut self) {
        self.in_progress = false;
        self.value = 0;
        // `self.pos` intentionally preserved.
    }

    /// Reset phase + output. Returns the new raw output (always 0).
    #[inline]
    pub fn retrigger_q(&mut self) -> i16 {
        self.in_progress = false;
        self.pos = 0;
        self.value = 0;
        0
    }

    /// Advance the LFO by one tick and update the output.
    #[inline]
    pub fn tick_q(&mut self) -> i16 {
        self.in_progress = true;

        // wave ∈ [-1, 1] Q15. `depth × wave / 32768` keeps the
        // depth's Q-format on the output. Clamp to `i16` on the
        // off-chance the product slightly overflows after the
        // arithmetic shift (worst case is one LSB).
        let wave = self
            .data
            .waveform
            .value_q15(xmrs::core::fixed::units::Phase::from_raw(self.pos));
        let product = ((self.data.depth as i32) * (wave.raw() as i32)) >> 15;
        self.value = product.clamp(i16::MIN as i32, i16::MAX as i32) as i16;

        // Advance phase. `speed.raw()` is `i16` Q8.8; clamp to
        // non-negative (negative LFO speed isn't a tracker thing)
        // and convert to a `u16` phase increment. Q8.8 raw `× 256`
        // matches the `u16` phase scale (a full cycle is `0x10000`
        // in `u16` space — `1.0 cycle/tick` in Q8.8 raw is 256 and
        // therefore wraps the phase exactly once per tick).
        let speed_pos = self.data.speed.raw().max(0) as u32;
        let phase_delta = speed_pos.wrapping_mul(256) as u16;
        self.pos = self.pos.wrapping_add(phase_delta);

        self.value
    }

    /// Current LFO output as [`Q15`] (volume / panning amplitude).
    /// Used by Tremolo and Panbrello.
    #[inline]
    pub fn value_q15(&self) -> Q15 {
        Q15::from_raw(self.value)
    }

    /// Current LFO output as [`PitchDelta`] (semitones, Q8.8).
    /// Used by Vibrato.
    #[inline]
    pub fn value_pitch_delta(&self) -> PitchDelta {
        PitchDelta::from_q8_8_i16(self.value)
    }

    /// Whether the LFO has been triggered and not yet stopped
    /// (used by channel housekeeping to know whether to keep
    /// re-evaluating it on subsequent ticks).
    pub fn in_progress(&self) -> bool {
        self.in_progress
    }
}
