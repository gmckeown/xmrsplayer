use crate::effect::EffectPlugin;
use core::default::Default;

#[derive(Clone, Default)]
pub struct Arpeggio {
    offset1: f32,
    offset2: f32,
}

/// Per-channel arpeggio state + FT2 historical LUT.
///
/// When `use_ft2_lut` is `true`, the offset to apply at each tick
/// is picked from FT2's historical `arpeggioTab` (reverse-counted
/// to match FT2's `song.tick`). Otherwise the effect falls back to
/// a plain `tick % 3` rotation, which is what Protracker /
/// Scream Tracker 3 / Impulse Tracker do.
///
/// `tempo` is the current song speed (number of ticks per row). The
/// FT2 lookup needs it to convert xmrs' forward-counting tick into
/// FT2's reverse `song.tick`. Callers must keep it in sync via
/// [`Self::set_tempo`] whenever the player's tempo changes; a stale
/// tempo would shift the lookup by one slot and swap base/offset1 at
/// the wrong ticks.
#[derive(Clone, Default)]
pub struct EffectArpeggio {
    data: Arpeggio,
    use_ft2_lut: bool,
    tempo: usize,
    tick: u8,
    in_progress: bool,
}

impl EffectArpeggio {
    pub fn new(use_ft2_lut: bool, tempo: usize) -> Self {
        Self {
            use_ft2_lut,
            tempo,
            ..Default::default()
        }
    }

    pub fn set_tempo(&mut self, tempo: usize) {
        self.tempo = tempo;
    }

    /// FT2 historical arpeggio lookup.
    ///
    /// In FT2, `song.tick` counts DOWN from `speed` (at row-load) to 1
    /// (on the last effect tick of the row). The arpeggio routine
    /// looks up `arpeggioTab[song.tick & 31]`. Crucially, arpeggio is
    /// NOT run at the row-load tick in FT2 (it's not in the TickZero
    /// jump table), so the effective lookup at the row-load tick is
    /// implicit: `outPeriod` stays equal to `realPeriod`, which
    /// corresponds to the base note (offset 0).
    ///
    /// xmrsplayer uses a forward-counting tick (0 at row-load,
    /// `tempo − 1` at the last effect tick). At the row-load tick this
    /// helper returns 0 (base note) to match FT2's behaviour; for
    /// effect ticks (`1..tempo-1`), the equivalent of FT2's
    /// `song.tick` is `tempo − xmrs_tick`.
    ///
    /// A previous formula (`tempo − tick − 1`) was shifted by one and
    /// made tick 0 play offset2 for typical tempos — e.g. tempo=6
    /// produced `[2, 1, 0, 2, 1, 0]` instead of FT2's
    /// `[base, 2, 1, 0, 2, 1]`.
    fn ft2_arpeggio_index(tick: u8, tempo: usize) -> u8 {
        if tempo == 0 {
            // Speed 0 halts the song; no arpeggio progression.
            return 0;
        }
        let tick = tick as usize % tempo;
        if tick == 0 {
            // Row-load equivalent: FT2 does not execute the arpeggio
            // effect at tickZero, so output is the base note.
            return 0;
        }
        let reverse_tick = (tempo - tick) as u8;
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
        let idx = if self.use_ft2_lut {
            Self::ft2_arpeggio_index(self.tick, self.tempo)
        } else {
            // Protracker / ST3 / IT all use the simple three-step
            // rotation with no LUT quirks.
            self.tick % 3
        };
        match idx {
            1 => self.data.offset1,
            2 => self.data.offset2,
            _ => 0.0,
        }
    }
}
