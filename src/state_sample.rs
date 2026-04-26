/// A Sample State
use crate::helper::*;
use xmrs::sample::{LoopType, Sample};

// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

const M: u32 = 25; // 25 bits for fract part seems the better i can have
const M_MASK: u32 = (1 << M) - 1;

#[derive(Clone)]
pub struct StateSample<'a> {
    sample: &'a Sample,
    finetune: f32,
    /// current sustain state
    sustained: bool,
    /// current seek position
    position: (u32, u32), // ( Position, Fract part M shifted )
    /// step is freq / rate
    step: Option<u32>, // step, M shifted
    // Output frequency
    rate: f32,

    // Cached loop / length parameters from the underlying `Sample`.
    //
    // These mirror `Sample::len`, `Sample::loop_*` and
    // `Sample::sustain_loop_*` and are populated once at construction.
    // The `Sample` itself is borrowed immutably for the lifetime of
    // this `StateSample`, so caching is safe — these values cannot
    // change underneath us.
    //
    // The point is to keep the per-sample hot path (`tick` →
    // `seek_cached` → `calculate_loop_cached`) free of the
    // `match self.data { Mono8(v) => v.len(), Mono16(v) => ... }`
    // walk inside `Sample::len` and `SampleDataType::len`. With
    // `#[inline(always)]` alone these matches inline at every call
    // site but still execute the chain of branches; caching the
    // resolved `usize` makes the hot path purely arithmetic. After
    // the inline-only patches, profiling still showed `Sample::len`
    // and `Sample::is_empty` cumulatively eating ~15% of total
    // runtime — the cache eliminates that entirely.
    cached_len: usize,
    cached_loop_start: usize,
    cached_loop_length: usize,
    cached_loop_flag: LoopType,
    cached_sustain_loop_start: usize,
    cached_sustain_loop_length: usize,
    cached_sustain_loop_flag: LoopType,
}

impl<'a> StateSample<'a> {
    pub fn new(sample: &'a Sample, rate: f32) -> Self {
        let finetune = sample.finetune;
        let cached_len = sample.len();
        Self {
            sample,
            finetune,
            sustained: true,
            position: (0, 0),
            step: None,
            rate,
            cached_len,
            cached_loop_start: sample.loop_start as usize,
            cached_loop_length: sample.loop_length as usize,
            cached_loop_flag: sample.loop_flag,
            cached_sustain_loop_start: sample.sustain_loop_start as usize,
            cached_sustain_loop_length: sample.sustain_loop_length as usize,
            cached_sustain_loop_flag: sample.sustain_loop_flag,
        }
    }

    pub fn reset(&mut self) {
        self.position = (0, 0);
        self.sustained = true;
        self.step = None;
    }

    pub fn set_step(&mut self, frequency: f32) {
        if self.cached_len == 0 {
            self.disable();
        } else {
            self.step = Some(((1 << M) as f32 * (frequency / self.rate)) as u32);
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

    pub fn get_panning(&self) -> f32 {
        self.sample.panning
    }

    pub fn get_volume(&self) -> f32 {
        self.sample.volume
    }

    /// use sample finetune or force if finetune arg!=0
    pub fn get_finetuned_pitch(&self) -> f32 {
        self.sample.relative_pitch as f32 + self.finetune
    }

    pub fn set_finetune(&mut self, finetune: f32) {
        self.finetune = finetune;
    }

    pub fn set_sustained(&mut self, sustained: bool) {
        self.position.0 = self
            .sample
            .meta_seek(self.position.0 as usize, self.sustained) as u32;
        self.sustained = sustained;
    }

    /// Cached version of `Sample::calculate_loop` — same algorithm,
    /// but uses `self.cached_len` instead of dispatching through the
    /// `SampleDataType` enum. `#[inline(always)]` so it folds entirely
    /// into `seek_cached`'s call site.
    #[inline(always)]
    fn calculate_loop_cached(
        &self,
        pos: usize,
        start: usize,
        length: usize,
        loop_type: LoopType,
    ) -> usize {
        if self.cached_len == 0 {
            return 0;
        }
        let end = start + length;
        match loop_type {
            LoopType::No => {
                if pos < self.cached_len {
                    pos
                } else {
                    self.cached_len - 1
                }
            }
            LoopType::Forward => {
                if length == 0 || pos < end {
                    pos.min(self.cached_len - 1)
                } else {
                    start + (pos - start) % length
                }
            }
            LoopType::PingPong => {
                if length == 0 || pos < end {
                    pos.min(self.cached_len - 1)
                } else {
                    let total_length = 2 * length;
                    let mod_pos = (pos - start) % total_length;
                    if mod_pos < length {
                        start + mod_pos
                    } else {
                        end - (mod_pos - length) - 1
                    }
                }
            }
        }
    }

    /// Cached `seek`. Equivalent to `Sample::seek` but uses the
    /// `StateSample`-side cache so the hot path doesn't hit the
    /// `SampleDataType` match. Returns `None` when the play head has
    /// run past the end of a non-looping sample.
    #[inline(always)]
    fn seek_cached(&self, pos: usize) -> Option<usize> {
        if self.cached_len == 0 {
            return None;
        }
        let (start, length, loop_type) =
            if self.sustained && self.cached_sustain_loop_flag != LoopType::No {
                (
                    self.cached_sustain_loop_start,
                    self.cached_sustain_loop_length,
                    self.cached_sustain_loop_flag,
                )
            } else {
                (
                    self.cached_loop_start,
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
            LoopType::Forward | LoopType::PingPong => {
                Some(self.calculate_loop_cached(pos, start, length, loop_type))
            }
        }
    }

    #[inline(always)]
    fn tick(&mut self) -> (f32, f32) {
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
                return (0.0, 0.0);
            }
        };
        // Linear interpolation peek: the "next" sample follows the
        // same rules. If the next frame is past the end, reuse the
        // current one rather than blending into silence.
        let vseek = self.seek_cached(pos + 1).unwrap_or(useek);

        let t = self.get_position_fraction() as f32 / (1 << M) as f32;
        let u = self.sample.at(useek);
        let v = self.sample.at(vseek);

        self.increment_position();

        (lerp(u.0, v.0, t), lerp(u.1, v.1, t))
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
    type Item = (f32, f32);

    #[inline(always)]
    fn next(&mut self) -> Option<Self::Item> {
        if self.is_enabled() {
            Some(self.tick())
        } else {
            None
        }
    }
}
