/// A Sample State
use crate::helper::*;
use xmrs::sample::Sample;

#[cfg(feature = "micromath")]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(feature = "libm")]
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
}

impl<'a> StateSample<'a> {
    pub fn new(sample: &'a Sample, rate: f32) -> Self {
        let finetune = sample.finetune;
        Self {
            sample,
            finetune,
            sustained: true,
            position: (0, 0),
            step: None,
            rate,
        }
    }

    pub fn reset(&mut self) {
        self.position = (0, 0);
        self.sustained = true;
        self.step = None;
    }

    pub fn set_step(&mut self, frequency: f32) {
        if self.sample.len() == 0 {
            self.disable();
        } else {
            self.step = Some(((1 << M) as f32 * (frequency / self.rate)) as u32);
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.step.is_some()
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
        self.position.0 = self.sample.meta_seek(self.position.0 as usize, self.sustained) as u32;
        self.sustained = sustained;
    }

    fn tick(&mut self) -> (f32, f32) {
        let t = self.get_position_fraction() as f32 / (1 << M) as f32;

        let useek = self.sample.meta_seek(self.position.0 as usize, self.sustained) as usize;
        let u = self.sample.at(useek);

        let vseek = self.sample.meta_seek(self.position.0 as usize + 1, self.sustained);
        let v = self.sample.at(vseek);

        self.increment_position();

        return (lerp(u.0, v.0, t), lerp(u.1, v.1, t));
    }

    pub fn set_position(&mut self, position: usize) {
        self.position.0 = position as u32;
        self.position.1 = 0;
    }

    #[inline(always)]
    fn increment_position(&mut self) -> u32 {
        if let Some(step) = self.step {
            self.position.1 += step;
            if self.position.1 > M_MASK {
                self.position.0 += self.get_position_trunc();
                self.position.1 = self.get_position_fraction();
            }
        }
        return self.position.0;
    }

    #[inline(always)]
    fn get_position_trunc(&self) -> u32 {
        self.position.1 >> M
    }

    #[inline(always)]
    fn get_position_fraction(&self) -> u32 {
        self.position.1 & M_MASK
    }
}

impl<'a> Iterator for StateSample<'a> {
    type Item = (f32, f32);

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_enabled() {
            Some(self.tick())
        } else {
            None
        }
    }
}
