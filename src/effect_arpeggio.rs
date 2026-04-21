use crate::effect::EffectPlugin;
use crate::ft2_quirks::Ft2Quirks;
use core::default::Default;

#[derive(Clone, Default)]
pub struct Arpeggio {
    offset1: f32,
    offset2: f32,
}

#[derive(Clone, Default)]
pub struct EffectArpeggio {
    data: Arpeggio,
    ft2_quirks: Option<Ft2Quirks>,
    tick: u8,
    in_progress: bool,
}

impl EffectArpeggio {
    pub fn new(ft2_quirks: Option<Ft2Quirks>) -> Self {
        Self {
            ft2_quirks,
            ..Default::default()
        }
    }
}

impl EffectPlugin for EffectArpeggio {
    fn tick0(&mut self, param1: f32, param2: f32) -> f32 {
        self.data.offset1 = param1;
        self.data.offset2 = param2;
        self.retrigger()
    }

    fn tick(&mut self) -> f32 {
        self.in_progress = true;
        self.tick += 1;
        self.value()
    }

    fn in_progress(&self) -> bool {
        self.in_progress
    }

    fn retrigger(&mut self) -> f32 {
        self.tick = 0;
        self.in_progress = false;
        self.value()
    }

    fn value(&self) -> f32 {
        match &self.ft2_quirks {
            Some(q) => match q.arpeggio_tick(self.tick) {
                1 => self.data.offset1,
                2 => self.data.offset2,
                _ => 0.0,
            },
            None => match self.tick % 3 {
                1 => self.data.offset1,
                2 => self.data.offset2,
                _ => 0.0,
            },
        }
    }
}
