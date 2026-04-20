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

    /// FT2 historical arpeggio lookup.
    ///
    /// In FT2, `song.tick` counts DOWN from `speed` (at row-load) to 1 (on the
    /// last effect tick of the row). The arpeggio routine looks up
    /// `arpeggioTab[song.tick & 31]`. Crucially, arpeggio is NOT run at the
    /// row-load tick in FT2 (it's not in the TickZero jump table), so the
    /// effective lookup at the row-load tick is implicit: `outPeriod` stays
    /// equal to `realPeriod`, which corresponds to the base note (offset 0).
    ///
    /// xmrsplayer uses a forward-counting tick (0 at row-load, tempo-1 at the
    /// last effect tick). At the row-load tick we must return 0 (base note)
    /// to match FT2's behaviour. For effect ticks (1..tempo-1), the equivalent
    /// of FT2's `song.tick` is `tempo - xmrs_tick`.
    ///
    /// The previous formula (`tempo - tick - 1`) was shifted by one and made
    /// tick 0 play offset2 for typical tempos — e.g. tempo=6 produced
    /// `[2, 1, 0, 2, 1, 0]` instead of FT2's `[base, 2, 1, 0, 2, 1]`.
    pub fn arpeggio_tick(&self, tick: u8) -> u8 {
        if self.tempo == 0 {
            // Speed 0 halts the song; no arpeggio progression.
            return 0;
        }
        let tick = tick as usize % self.tempo;
        if tick == 0 {
            // Row-load equivalent: FT2 does not execute the arpeggio effect
            // at tickZero, so output is the base note.
            return 0;
        }
        let reverse_tick = (self.tempo - tick) as u8;
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
