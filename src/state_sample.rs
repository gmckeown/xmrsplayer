/// A Sample State
use xmrs::fixed::fixed::Q15;
use xmrs::fixed::units::{Amp, ChannelVolume, Finetune, Frequency, Panning, PitchDelta, SampleRate};
use xmrs::sample::{LoopType, Sample};

const M: u32 = 25; // 25 bits for fract part seems the better i can have
const M_MASK: u32 = (1 << M) - 1;

#[derive(Clone)]
pub struct StateSample<'a> {
    sample: &'a Sample,
    /// Voice finetune contribution in semitones (was `f32`).
    /// Stored as `Finetune` Q1.15 — represents fractional
    /// semitones in `[-1, +1)`. The tracker formats put a
    /// signed byte here `(byte/127)` semitones; Q1.15 has
    /// 1/32768 ≈ 0.003 semitone resolution, well below the
    /// XM byte's 1/127 step.
    finetune: Finetune,
    /// current sustain state
    sustained: bool,
    /// current seek position
    position: (u32, u32), // ( Position, Fract part M shifted )
    /// step is freq / rate
    step: Option<u32>, // step, M shifted
    /// Output sample-rate (was `f32`). Q-typed `u32` Hz
    /// throughout — `set_step` multiplies as `u64` to keep
    /// the precision of the previous f32 path with no rounding
    /// surprises.
    rate: SampleRate,

    // Cached loop / length parameters from the underlying `Sample`.
    //
    // These mirror `Sample::len`, `Sample::loop_*` and
    // `Sample::sustain_loop_*` and are populated once at construction.
    // The `Sample` itself is borrowed immutably for the lifetime of
    // this `StateSample`, so caching is safe — these values cannot
    // change underneath us.
    //
    // The point is to keep the per-sample hot path (`tick` →
    // `seek_cached`) free of the
    // `match self.data { Mono8(v) => v.len(), Mono16(v) => ... }`
    // walk inside `Sample::len` and `SampleDataType::len`. With
    // `#[inline(always)]` alone these matches inline at every call
    // site but still execute the chain of branches; caching the
    // resolved `usize` makes the hot path purely arithmetic. After
    // the inline-only patches, profiling still showed `Sample::len`
    // and `Sample::is_empty` cumulatively eating ~15% of total
    // runtime — the cache eliminates that entirely.
    cached_len: usize,
    /// `cached_len - 1` (or 0 if empty). Hot path uses this as the
    /// last-valid-index clamp; precomputing eliminates the
    /// subtraction (and the underflow check) inside `seek_cached`.
    cached_len_minus_1: usize,
    cached_loop_start: usize,
    cached_loop_length: usize,
    /// `cached_loop_start + cached_loop_length`. Materialising the
    /// end of the loop region avoids recomputing it inside the
    /// per-frame seek.
    cached_loop_end: usize,
    cached_loop_flag: LoopType,
    cached_sustain_loop_start: usize,
    cached_sustain_loop_length: usize,
    cached_sustain_loop_end: usize,
    cached_sustain_loop_flag: LoopType,
}

impl<'a> StateSample<'a> {
    pub fn new(sample: &'a Sample, rate: SampleRate) -> Self {
        let finetune = sample.finetune;
        let cached_len = sample.len();
        let cached_loop_start = sample.loop_start as usize;
        let cached_loop_length = sample.loop_length as usize;
        let cached_sustain_loop_start = sample.sustain_loop_start as usize;
        let cached_sustain_loop_length = sample.sustain_loop_length as usize;
        Self {
            sample,
            finetune,
            sustained: true,
            position: (0, 0),
            step: None,
            rate,
            cached_len,
            cached_len_minus_1: cached_len.saturating_sub(1),
            cached_loop_start,
            cached_loop_length,
            cached_loop_end: cached_loop_start + cached_loop_length,
            cached_loop_flag: sample.loop_flag,
            cached_sustain_loop_start,
            cached_sustain_loop_length,
            cached_sustain_loop_end: cached_sustain_loop_start + cached_sustain_loop_length,
            cached_sustain_loop_flag: sample.sustain_loop_flag,
        }
    }

    pub fn reset(&mut self) {
        self.position = (0, 0);
        self.sustained = true;
        self.step = None;
    }

    /// Compute the integer-fixed-point sample step from the
    /// playback `frequency` (Q24.8 Hz) and the engine's
    /// sample-rate. All math is `u64` integer — no f32.
    ///
    /// `step = (2^M × frequency_hz) / rate_hz`, expanded into
    /// `(frequency_q24_8 << (M - 8)) / rate_hz` where the
    /// `<< (M - 8)` is exact (M = 25 ≥ 8) and the `u64`
    /// intermediate keeps full precision until the final
    /// truncation back to `u32`.
    pub fn set_step(&mut self, frequency: Frequency) {
        if self.cached_len == 0 {
            self.disable();
        } else {
            let rate_hz = self.rate.hz();
            if rate_hz == 0 {
                self.disable();
                return;
            }
            // Promote both operands to u64. M ≥ 8 so the shift
            // is non-negative; with M = 25 the worst-case
            // `frequency << 17 ≈ 16M × 2^17 ≈ 2 × 10^12`, well
            // within u64.
            let num: u64 = (frequency.raw_q24_8() as u64) << (M - 8);
            let step = num / rate_hz as u64;
            self.step = Some(step.min(u32::MAX as u64) as u32);
        }
    }

    #[inline(always)]
    pub fn is_enabled(&self) -> bool {
        self.step.is_some()
    }

    /// `true` if the underlying sample has any kind of loop (forward,
    /// ping-pong, or sustain). Used by the voice pool's eviction
    /// heuristic — looping voices can ring indefinitely so they're
    /// cheaper to drop than one-shot tails.
    pub fn is_looping(&self) -> bool {
        self.cached_loop_flag != LoopType::No || self.cached_sustain_loop_flag != LoopType::No
    }

    pub fn disable(&mut self) {
        self.step = None;
    }

    pub fn get_panning(&self) -> Panning {
        self.sample.panning
    }

    pub fn get_volume(&self) -> ChannelVolume {
        self.sample.volume
    }

    /// Sample's pitch contribution: integer relative-pitch
    /// semitones plus fractional finetune. Returns a
    /// [`PitchDelta`] (Q8.8 semitones) — the pitch chain unit.
    ///
    /// Encoding: `relative_pitch i8` widens to `i16` semitones
    /// then shifts left 8 (Q8.8 raw); finetune `Q1.15 raw` >> 7
    /// gives the matching Q8.8 fractional bits (lossy by 7 bits
    /// of finetune precision = 1/128 of a semitone, well below
    /// the XM byte's 1/127 step). Saturating add at the i16
    /// boundary — `relative_pitch` is clamped to `[-95, 96]`
    /// at import time so the sum can't overflow in practice
    /// either way.
    pub fn get_finetuned_pitch(&self) -> PitchDelta {
        let rel_q88 = (self.sample.relative_pitch as i16).saturating_mul(256);
        let fine_q88 = self.finetune.into_q15().raw() >> 7;
        PitchDelta::from_q8_8_i16(rel_q88.saturating_add(fine_q88))
    }

    pub fn set_finetune(&mut self, finetune: Finetune) {
        self.finetune = finetune;
    }

    pub fn set_sustained(&mut self, sustained: bool) {
        self.position.0 = self
            .sample
            .meta_seek(self.position.0 as usize, self.sustained) as u32;
        self.sustained = sustained;
    }

    /// Resolve a play-head position to a sample-index, honouring
    /// the active loop region.
    ///
    /// Returns `None` when the play head has run past the end of a
    /// non-looping sample — at which point the voice is done and
    /// must be shut off, otherwise it would hold the tail frame
    /// indefinitely and produce an audible drone on any instrument
    /// whose tail isn't silent.
    ///
    /// **Hot path**: this fires twice per audio frame per active
    /// voice (once for `pos`, once for `pos + 1` for linear
    /// interpolation). Everything is read from the per-state cache
    /// (no `Sample::len`, no enum dispatch on `SampleDataType`),
    /// the dispatch on `LoopType` happens once (no nested helper),
    /// and the loop end is precomputed (no `start + length` per
    /// call).
    #[inline(always)]
    fn seek_cached(&self, pos: usize) -> Option<usize> {
        if self.cached_len == 0 {
            return None;
        }
        let (start, end, length, loop_type) =
            if self.sustained && self.cached_sustain_loop_flag != LoopType::No {
                (
                    self.cached_sustain_loop_start,
                    self.cached_sustain_loop_end,
                    self.cached_sustain_loop_length,
                    self.cached_sustain_loop_flag,
                )
            } else {
                (
                    self.cached_loop_start,
                    self.cached_loop_end,
                    self.cached_loop_length,
                    self.cached_loop_flag,
                )
            };

        match loop_type {
            LoopType::No => {
                if pos < self.cached_len {
                    Some(pos)
                } else {
                    None
                }
            }
            LoopType::Forward => {
                if length == 0 || pos < end {
                    Some(pos.min(self.cached_len_minus_1))
                } else {
                    Some(start + (pos - start) % length)
                }
            }
            LoopType::PingPong => {
                if length == 0 || pos < end {
                    Some(pos.min(self.cached_len_minus_1))
                } else {
                    let total_length = 2 * length;
                    let mod_pos = (pos - start) % total_length;
                    if mod_pos < length {
                        Some(start + mod_pos)
                    } else {
                        Some(end - (mod_pos - length) - 1)
                    }
                }
            }
        }
    }

    #[inline(always)]
    fn tick(&mut self) -> (Amp, Amp) {
        // Ask the sample where we actually are. `seek_cached` returns
        // `None` when the play head has run past the end of a non-
        // looping sample — at which point the voice is done and must
        // be shut off, otherwise it would hold the tail frame
        // indefinitely and produce an audible drone on any instrument
        // whose tail isn't silent.
        let pos = self.position.0 as usize;
        let useek = match self.seek_cached(pos) {
            Some(s) => s,
            None => {
                self.disable();
                return (Amp::SILENCE, Amp::SILENCE);
            }
        };
        // Linear interpolation peek: the "next" sample follows the
        // same rules. If the next frame is past the end, reuse the
        // current one rather than blending into silence.
        let vseek = self.seek_cached(pos + 1).unwrap_or(useek);

        // Position fraction in Q1.15. `get_position_fraction()`
        // returns a `u32` in `[0, 1 << M)` (M is the player's
        // sub-sample bit count, typically 25). Right-shift by
        // `M - 15` to bring it into the Q1.15 numerator space —
        // exact for any `M ≥ 15`, no f32.
        let frac = self.get_position_fraction();
        let t = Q15::from_raw((frac >> (M - 15)) as i16);

        let u = self.sample.at(useek);
        let v = self.sample.at(vseek);

        self.increment_position();

        // `Amp::lerp` is a saturating Q1.15 lerp — `u + (v - u) * t`
        // computed in widened i32. Replaces the f32 lerp helper.
        (u.0.lerp(v.0, t), u.1.lerp(v.1, t))
    }

    pub fn set_position(&mut self, position: usize) {
        self.position.0 = position as u32;
        self.position.1 = 0;
    }

    /// Length of the underlying sample in sample frames. Used by the
    /// channel's Oxx-past-end branch to decide whether to clamp
    /// (IT old-effects) or drop the offset (IT default / XM / etc.).
    pub fn sample_len(&self) -> usize {
        self.cached_len
    }

    #[inline(always)]
    fn increment_position(&mut self) -> u32 {
        if let Some(step) = self.step {
            // Split `step` into integer and fractional parts *before*
            // accumulating, so the fractional register never overflows
            // u32 on 32-bit targets. With M = 25:
            //   step & M_MASK   < 2^25
            //   position.1     <= M_MASK   < 2^25
            //   → position.1 + (step & M_MASK) < 2^26   (safe u32 add)
            //
            // The naive `position.1 += step` panicked whenever `step`
            // saturated to u32::MAX — which happens as soon as the
            // `f32 as u32` cast in `set_step` is fed a multi-MHz
            // frequency (e.g. S3M portamento-up driving the Amiga
            // period toward zero). Branchless form below keeps the
            // hot path free of conditional jumps.
            self.position.1 += step & M_MASK;
            let carry = self.position.1 >> M; // 0 or 1
            self.position.0 = self
                .position
                .0
                .wrapping_add((step >> M).wrapping_add(carry));
            self.position.1 &= M_MASK;
        }
        self.position.0
    }

    #[inline(always)]
    fn get_position_fraction(&self) -> u32 {
        self.position.1 & M_MASK
    }
}

impl<'a> Iterator for StateSample<'a> {
    type Item = (Amp, Amp);

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.is_enabled() {
            Some(self.tick())
        } else {
            None
        }
    }
}
