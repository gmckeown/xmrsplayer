use xmrs::period_helper::{FrequencyType, PeriodHelper};
/// An Instrument Vibrato State
use xmrs::vibrato::Vibrato;
use xmrs::waveform::WaveformState;

#[derive(Clone)]
pub struct StateAutoVibrato<'a> {
    vibrato: &'a Vibrato,
    wfs: WaveformState,
    period_helper: PeriodHelper,
    phase: f32,
    pub current_modulation: f32,
}

impl<'a> StateAutoVibrato<'a> {
    pub fn new(vibrato: &'a Vibrato, period_helper: PeriodHelper) -> Self {
        let mut sv = Self {
            vibrato,
            wfs: WaveformState::new(vibrato.waveform),
            period_helper,
            phase: 0.0,
            current_modulation: 0.0,
        };

        sv.reset();

        sv
    }

    pub fn reset(&mut self) {
        self.retrig();
    }

    pub fn retrig(&mut self) {
        self.phase = 0.0;
        self.current_modulation = 0.0;
    }

    pub fn tick(&mut self, _sustain: bool) {
        self.phase += self.vibrato.speed;

        // Sweep-in: auto-vibrato depth ramps from 0 to `depth`
        // over the first `sweep` units of phase accumulation. Once
        // `phase >= sweep` the LFO runs at full depth for the rest
        // of the voice's life — including after key-off (sustain =
        // false). Previously the `!sustain` gate made the ramp
        // only apply during release, which was exactly backwards.
        //
        // `sweep == 0` means "no ramp, go straight to full depth"
        // in IT (ITTECH) — handled explicitly to avoid a division
        // that would NaN out `current_modulation`.
        let current_depth = if self.vibrato.sweep <= 0.0 || self.phase >= self.vibrato.sweep {
            self.vibrato.depth
        } else {
            (self.phase / self.vibrato.sweep) * self.vibrato.depth
        };

        self.current_modulation = current_depth * self.wfs.value(self.phase);

        if let FrequencyType::AmigaFrequencies = self.period_helper.freq_type {
            self.current_modulation /= 4.0;
        }
    }
}
