/// A Sample State
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{
    Amp, ChannelVolume, Finetune, Frequency, Panning, PitchDelta, SampleRate,
};
use xmrs::core::sample::{LoopType, Sample};

const M: u32 = 25; // 25 bits for fract part seems the better i can have
const M_MASK: u32 = (1 << M) - 1;

/// Cached loop region (forward or sustain). Materialises `end =
/// start + length` so the per-frame seek doesn't recompute it.
#[derive(Clone, Copy)]
struct LoopRegion {
    start: usize,
    length: usize,
    end: usize,
    flag: LoopType,
}

impl LoopRegion {
    /// Build a loop region, clamping `start`/`length` against the
    /// decoded sample length `data_len` so the region can never
    /// overrun the actual data.
    ///
    /// This defends the per-frame seek against loop metadata that
    /// declares `loop_end > data_len`. The XM loader already clamps
    /// at import (`xm/xmsample.rs`), but the IT loader copies
    /// `loop_beginning`/`loop_end` verbatim — so a crafted `.it`
    /// can declare a forward/ping-pong loop whose end runs past the
    /// decoded bytes. Without this clamp, once the play position
    /// passes `region.end` the wrapped index lands in
    /// `[data_len, loop_end)` and `Sample::at` indexes the backing
    /// vec out of bounds → panic.
    ///
    /// For a well-formed sample (`start < data_len` and
    /// `start + length <= data_len`) both `.min(..)` are no-ops, so
    /// the cached region — and therefore the rendered audio — is
    /// byte-identical.
    fn new(start: usize, length: usize, flag: LoopType, data_len: usize) -> Self {
        let (start, length) = if data_len == 0 {
            (0, 0)
        } else {
            let start = start.min(data_len - 1);
            let length = length.min(data_len - start);
            (start, length)
        };
        Self {
            start,
            length,
            end: start + length,
            flag,
        }
    }
}

#[derive(Clone)]
pub struct StateSample<'a> {
    sample: &'a Sample,
    /// Sample interpolation mode (defaults to `Interpolation::Nearest`)
    pub interpolation: crate::interpolation::Interpolation,
    /// Voice finetune contribution in semitones. Stored as
    /// `Finetune` Q1.15 — represents fractional semitones in
    /// `[-1, +1)`. The tracker formats put a signed byte here
    /// `(byte/127)` semitones; Q1.15 has 1/32768 ≈ 0.003 semitone
    /// resolution, well below the XM byte's 1/127 step.
    finetune: Finetune,
    /// current sustain state
    sustained: bool,
    /// current seek position
    position: (u32, u32), // ( Position, Fract part M shifted )
    /// step is freq / rate
    step: Option<u32>, // step, M shifted
    /// Output sample-rate. Q-typed `u32` Hz throughout —
    /// `set_step` multiplies as `u64` to keep full precision
    /// with no rounding surprises.
    rate: SampleRate,

    // Cached length / loop parameters from the underlying `Sample`.
    //
    // These mirror `Sample::len` and the two `loop_*` blocks and are
    // populated once at construction. The `Sample` itself is borrowed
    // immutably for the lifetime of this `StateSample`, so caching is
    // safe — these values cannot change underneath us.
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
    cached_loop: LoopRegion,
    cached_sustain_loop: LoopRegion,
}

impl<'a> StateSample<'a> {
    pub fn new(sample: &'a Sample, rate: SampleRate) -> Self {
        let cached_len = sample.len();
        Self {
            sample,
            interpolation: crate::interpolation::Interpolation::default(),
            finetune: sample.finetune,
            sustained: true,
            position: (0, 0),
            step: None,
            rate,
            cached_len,
            cached_len_minus_1: cached_len.saturating_sub(1),
            cached_loop: LoopRegion::new(
                sample.loop_start as usize,
                sample.loop_length as usize,
                sample.loop_flag,
                cached_len,
            ),
            cached_sustain_loop: LoopRegion::new(
                sample.sustain_loop_start as usize,
                sample.sustain_loop_length as usize,
                sample.sustain_loop_flag,
                cached_len,
            ),
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
        self.cached_loop.flag != LoopType::No || self.cached_sustain_loop.flag != LoopType::No
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
        let region = if self.sustained && self.cached_sustain_loop.flag != LoopType::No {
            &self.cached_sustain_loop
        } else {
            &self.cached_loop
        };

        match region.flag {
            LoopType::No => {
                if pos < self.cached_len {
                    Some(pos)
                } else {
                    None
                }
            }
            LoopType::Forward => {
                if region.length == 0 || pos < region.end {
                    Some(pos.min(self.cached_len_minus_1))
                } else {
                    Some(region.start + (pos - region.start) % region.length)
                }
            }
            LoopType::PingPong => {
                if region.length == 0 || pos < region.end {
                    Some(pos.min(self.cached_len_minus_1))
                } else {
                    let total_length = 2 * region.length;
                    let mod_pos = (pos - region.start) % total_length;
                    if mod_pos < region.length {
                        Some(region.start + mod_pos)
                    } else {
                        Some(region.end - (mod_pos - region.length) - 1)
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
        let u = self.sample.at(useek);
        self.increment_position();

        match self.interpolation {
            crate::interpolation::Interpolation::Nearest => (u.0, u.1),
            crate::interpolation::Interpolation::Linear => {
                let vseek = self.seek_cached(pos + 1).unwrap_or(useek);
                let frac = self.get_position_fraction();
                let t = Q15::from_raw((frac >> (M - 15)) as i16);
                let v = self.sample.at(vseek);
                (u.0.lerp(v.0, t), u.1.lerp(v.1, t))
            }
        }
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

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use xmrs::core::fixed::units::{
        ChannelVolume, Finetune, Frequency, Panning, SampleRate, Volume,
    };
    use xmrs::core::sample::{LoopType, Sample, SampleDataType};

    /// A loop region declared past the end of the decoded data
    /// (`loop_end > data_len`) — which the IT loader can copy
    /// verbatim from a crafted file — must never make the per-frame
    /// seek index the backing vec out of bounds. Before the
    /// `LoopRegion::new` clamp, ticking past `region.end` panicked in
    /// `Sample::at`.
    fn sample_with_loop(
        data_len: usize,
        flag: LoopType,
        loop_start: u32,
        loop_length: u32,
    ) -> Sample {
        let data: Vec<i8> = (0..data_len as i32).map(|i| (i % 100) as i8).collect();
        Sample {
            name: "evil".into(),
            relative_pitch: 0,
            finetune: Finetune::default(),
            volume: ChannelVolume::FULL,
            default_note_volume: Volume::FULL,
            panning: Panning::CENTER,
            loop_flag: flag,
            loop_start,
            loop_length,
            sustain_loop_flag: LoopType::No,
            sustain_loop_start: 0,
            sustain_loop_length: 0,
            data: Some(SampleDataType::Mono8(data.into())),
        }
    }

    fn tick_well_past_loop(sample: &Sample) {
        let mut state = StateSample::new(sample, SampleRate::from_hz(48_000));
        state.set_step(Frequency::from_hz(48_000)); // one data frame per output frame
                                                    // Far more frames than `loop_end`, so the play position
                                                    // climbs past `region.end` and exercises the wrap branch.
        for _ in 0..4096 {
            let _ = state.tick();
        }
    }

    #[test]
    fn forward_loop_overrunning_data_does_not_panic() {
        // data_len = 16, but the forward loop claims [4, 1000).
        tick_well_past_loop(&sample_with_loop(16, LoopType::Forward, 4, 996));
    }

    #[test]
    fn pingpong_loop_overrunning_data_does_not_panic() {
        tick_well_past_loop(&sample_with_loop(16, LoopType::PingPong, 4, 996));
    }

    #[test]
    fn loop_start_past_end_does_not_panic() {
        // Even loop_start itself beyond the data is clamped.
        tick_well_past_loop(&sample_with_loop(16, LoopType::Forward, 9000, 996));
    }

    #[test]
    fn valid_loop_region_is_unchanged_by_clamp() {
        // A well-formed loop (loop_end == data_len) must be preserved
        // bit-for-bit by the clamp — same start/length/end.
        let sample = sample_with_loop(64, LoopType::Forward, 16, 48);
        let state = StateSample::new(&sample, SampleRate::from_hz(48_000));
        assert_eq!(state.cached_loop.start, 16);
        assert_eq!(state.cached_loop.length, 48);
        assert_eq!(state.cached_loop.end, 64);
    }
}
