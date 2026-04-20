#[cfg(feature = "micromath")]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(feature = "libm")]
#[allow(unused_imports)]
use num_traits::float::Float;

/// Struct is very small we can clone it everywhere in other structs...

#[derive(Default, Clone, Copy)]
pub struct HistoricalHelper {
    pub tempo: usize,
}

impl HistoricalHelper {
    pub fn new(tempo: usize) -> Self {
        Self { tempo }
    }

    pub fn set_tempo(&mut self, tempo: usize) {
        self.tempo = tempo;
    }

    /// Arpeggio
    pub fn arpeggio_tick(&self, tick: u8) -> u8 {
        if self.tempo == 0 {
            // Speed 0 halts the song; no arpeggio progression.
            return 0;
        }
        let tick = tick as usize % self.tempo;
        let reverse_tick = (self.tempo - tick - 1) as u8;
        match reverse_tick {
            0..=15 => reverse_tick % 3,
            51 | 54 | 60 | 63 | 72 | 78 | 81 | 93 | 99 | 105 | 108 | 111 | 114 | 117 | 120
            | 123 | 126 | 129 | 132 | 135 | 138 | 141 | 144 | 147 | 150 | 153 | 156 | 159 | 165
            | 168 | 171 | 174 | 177 | 180 | 183 | 186 | 189 | 192 | 195 | 198 | 201 | 204 | 207
            | 210 | 216 | 219 | 222 | 225 | 228 | 231 | 234 | 237 | 240 | 243 => 0,
            _ => 2,
        }
    }
}
