#[cfg(feature = "micromath")]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(feature = "libm")]
#[allow(unused_imports)]
use num_traits::float::Float;
use xmrs::waveform::WaveformState;

use crate::effect::*;
use crate::effect_arpeggio::EffectArpeggio;
use crate::effect_vibrato_tremolo::EffectVibratoTremolo;
use crate::historical_helper::HistoricalHelper;
use crate::triggerkeep::*;

use crate::helper::*;
use crate::state_instr_default::StateInstrDefault;
use xmrs::prelude::*;

#[derive(Clone, PartialEq, Default)]
struct NoteRetrigState {
    note: Pitch,
    instr: Option<usize>,
    speed: usize,
    volume_modifier: NoteRetrigOperator,
}

#[derive(Clone)]
pub struct Channel<'a> {
    module: &'a Module,
    historical: Option<HistoricalHelper>,
    period_helper: PeriodHelper,
    rate: f32,

    note: f32,

    pub current: TrackUnit,

    period: f32,

    channel_volume: f32,
    volume: f32,  /* Ideally between 0 (muted) and 1 (loudest) */
    panning: f32, /* Between 0 (left) and 1 (right); 0.5 is centered */

    // Instrument
    instr: Option<StateInstrDefault<'a>>,

    effect_arpeggio: EffectArpeggio,
    effect_note_retrig_backup: NoteRetrigState,
    effect_note_retrig_counter: usize,
    effect_panbrello: EffectVibratoTremolo,
    effect_portamento: f32,
    effect_tone_portamento_goal: f32,
    effect_tremolo: EffectVibratoTremolo,
    effect_tremor: bool,
    effect_tremor_on: usize,
    effect_tremor_off: usize,
    effect_vibrato: EffectVibratoTremolo,
    effect_semitone: bool,
    effect_note_delay: usize,
    /// Where to restart a E6y loop
    pub(crate) pattern_loop_origin: usize,
    /// How many loop passes have been done
    pub(crate) pattern_loop_count: usize,

    pub muted: bool,

    actual_volume: [f32; 2],
}

impl<'a> Channel<'a> {
    pub(crate) fn new(module: &'a Module, rate: f32, historical: Option<HistoricalHelper>) -> Self {
        let period_helper = PeriodHelper::new(module.frequency_type, historical.is_some());
        Self {
            module,
            historical: historical,
            period_helper: period_helper.clone(),
            rate,
            channel_volume: 1.0,
            volume: 1.0,
            panning: 0.5,
            note: 0.0,
            current: TrackUnit::default(),
            period: 0.0,
            instr: None,
            effect_arpeggio: EffectArpeggio::new(historical.clone()),
            effect_note_retrig_backup: NoteRetrigState::default(),
            effect_note_retrig_counter: 0,
            effect_panbrello: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremolo: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremor: false,
            effect_tremor_on: 0,
            effect_tremor_off: 0,
            effect_vibrato: EffectVibratoTremolo::new(Waveform::Sine),
            effect_portamento: 0.0,
            effect_tone_portamento_goal: 0.0,
            effect_semitone: false,
            effect_note_delay: 0,
            pattern_loop_origin: 0,
            pattern_loop_count: 0,
            muted: false,
            actual_volume: [0.0, 0.0],
        }
    }

    pub fn is_muted(&self) -> bool {
        let midi_mute = if let Some(i) = &self.instr {
            i.midi_mute_computer
        } else {
            false
        };
        self.muted || midi_mute
    }

    fn cut_pitch(&mut self) {
        /* NB: this is not the same as Key Off */
        self.volume = 0.0;
    }

    fn key_off_historical(&mut self, tick: usize) {
        if let Some(i) = &mut self.instr {
            i.key_off();
            // openmpt `key_off.xm`: Key off at tick 0 (K00) is very dodgy command. If there is a note next to it, the note is ignored. If there is a volume column command or instrument next to it and the current instrument has no volume envelope, the note is faded out instead of being cut.
            if (tick == 0
                && (i.has_volume_envelope()
                    || self.current.instrument.is_some()
                    || self.current.velocity != 0.0))
                || (tick != 0 && i.has_volume_envelope())
            {
                self.trigger_pitch(
                    TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE,
                );
            } else {
                self.cut_pitch();
            }
        } else {
            self.cut_pitch();
        }
    }

    fn key_off(&mut self, tick: usize) {
        if self.historical.is_some() {
            self.key_off_historical(tick);
            return;
        }

        if let Some(i) = &mut self.instr {
            i.key_off();
        } else {
            self.cut_pitch();
        }
    }

    pub(crate) fn trigger_pitch(&mut self, flags: TriggerKeep) {
        self.effect_tremor = false;

        match &mut self.instr {
            Some(instr) => {
                if !contains(flags, TRIGGER_KEEP_SAMPLE_POSITION) {
                    instr.sample_reset();
                }

                if !contains(flags, TRIGGER_KEEP_ENVELOPE) {
                    instr.envelopes_reset();
                }

                instr.vibrato_reset();

                if !contains(flags, TRIGGER_KEEP_VOLUME) {
                    instr.volume_reset();
                    self.volume = instr.volume;
                }

                // TODO
                self.panning = instr.panning;

                if !contains(flags, TRIGGER_KEEP_PERIOD) {
                    self.period = self.period_helper.note_to_period(self.note);
                    instr.update_frequency(
                        self.period,
                        0.0,
                        self.effect_vibrato.value(),
                        self.effect_semitone,
                    );
                }
            }
            None => {}
        }
    }

    fn tickn_update_instr(&mut self) {
        if let Some(instr) = &mut self.instr {
            // Panning
            let space_from_center = (0.5 - (self.panning - 0.5).abs()) * 2.0;
            let instr_env_pan_val = instr.envelope_panning.value - 0.5;
            let panbrello = self.effect_panbrello.value();
            let panning: f32 = self.panning
                + instr_env_pan_val * space_from_center
                + panbrello * space_from_center;
            let panning = panning.clamp(0.0, 1.0);

            // Volume
            let mut volume = 0.0;
            if !self.effect_tremor {
                volume = self.volume + self.effect_tremolo.value();
                volume = volume.clamp(0.0, 1.0);
                volume *= instr.get_volume();
                volume *= self.channel_volume;
            }

            // Volume and Freq in one
            // panning = 0.0 => full left, 1.0 => full right.
            // actual_volume[0] multiplies the left sample channel,
            // actual_volume[1] the right.
            self.actual_volume[0] = volume * (1.0 - panning).sqrt();
            self.actual_volume[1] = volume * panning.sqrt();

            // Frequency
            let arp_pitch = if self.current.has_arpeggio() {
                self.effect_arpeggio.value()
            } else {
                0.0
            };

            instr.update_frequency(
                self.period,
                arp_pitch,
                self.effect_vibrato.value(),
                self.effect_semitone,
            )
        }
    }

    pub(crate) fn tick(&mut self, current_tick: usize) {
        if let Some(instr) = &mut self.instr {
            instr.tick();
            self.tickn_effects(current_tick);
            self.tickn_update_instr();
        } else if self.current.has_delay() {
            self.tickn_effects(current_tick);
            self.tickn_update_instr();
        }
    }

    fn tickn_effects(&mut self, current_tick: usize) {
        if current_tick == 0 {
            // At every row-load tick, clear the tremor mute flag. FT2's Tremor
            // effect never runs at tickZero (it's only in the TickNonZero jump
            // table), so the row always begins "playing". If the current row
            // also carries a Tremor effect, its handler below will overwrite
            // this; if it doesn't, the previous row's last tremor state
            // (which might have left us on the "off" half) must not mute the
            // whole next row.
            self.effect_tremor = false;
        }
        let len = self.current.effects.len();
        for i in 0..len {
            match self.current.effects[i].clone() {
                TrackEffect::Arpeggio {
                    half1: n1,
                    half2: n2,
                } => {
                    if current_tick == 0 {
                        self.effect_arpeggio.tick0(n1 as f32, n2 as f32);
                    } else {
                        if n1 != 0 || n2 != 0 {
                            self.effect_arpeggio.tick();
                        }
                    }
                }
                TrackEffect::ChannelVolume(v) => {
                    self.channel_volume = v.clamp(0.0, 1.0);
                }
                TrackEffect::ChannelVolumeSlide {
                    speed: _s,
                    fine: _f,
                } => {
                    #[cfg(feature = "demo")]
                    println!("ChannelVolumeSlide");
                    todo!();
                }
                TrackEffect::Glissando(glissando) => {
                    if current_tick == 0 {
                        self.effect_semitone = glissando;
                    }
                }
                TrackEffect::InstrumentFineTune(finetune) => {
                    if current_tick == 0 && self.current.note.is_valid() {
                        if let Some(instr) = &mut self.instr {
                            instr.set_finetune(finetune);
                            self.note =
                                self.current.note.value() as f32 + instr.get_finetuned_pitch();
                            self.period = self.period_helper.note_to_period(self.note);
                        }
                    }
                }
                TrackEffect::InstrumentNewNoteAction(_nna) => {
                    #[cfg(feature = "demo")]
                    println!("InstrumentNewNoteAction");
                    todo!();
                }
                TrackEffect::InstrumentPanningEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = &mut self.instr {
                            instr.envelope_panning.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentPanningEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = &mut self.instr {
                            instr.envelope_panning.enabled = pe;
                        }
                    }
                }
                TrackEffect::InstrumentPitchEnvelope(_pe) => {
                    #[cfg(feature = "demo")]
                    println!("InstrumentPitchEnvelope");
                    todo!()
                }
                TrackEffect::InstrumentSampleOffset(seek) => {
                    if current_tick == 0 {
                        if self.current.note.is_valid() {
                            if let Some(instr) = &mut self.instr {
                                if let Some(sample) = &mut instr.state_sample {
                                    sample.set_position(seek);
                                }
                            }
                        }
                    }
                }
                TrackEffect::InstrumentSurround(_s) => {
                    #[cfg(feature = "demo")]
                    println!("InstrumentSurround");
                    todo!()
                }
                TrackEffect::InstrumentVolumeEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = &mut self.instr {
                            instr.envelope_volume.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentVolumeEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = &mut self.instr {
                            instr.envelope_volume.enabled = pe;
                        }
                    }
                }
                TrackEffect::NoteCut { tick: t, past: _p } => {
                    // TODO: past
                    if current_tick == t {
                        self.cut_pitch();
                    }
                }
                TrackEffect::NoteDelay(delay) => {
                    if current_tick == 0 {
                        if self.current.note.is_none() {
                            self.trigger_pitch(
                                TRIGGER_KEEP_SAMPLE_POSITION
                                    | TRIGGER_KEEP_VOLUME
                                    | TRIGGER_KEEP_PERIOD,
                            );
                        } else if self.current.note.is_keyoff() {
                            if self.current.instrument.is_none() {
                                self.key_off(0);
                            } else {
                                self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE);
                            }
                        }
                    } else if current_tick == delay {
                        self.tick0_load_instrument_and_pitch();
                        self.tickn_effects(0);

                        /* Special KeyOff cases */
                        if self.current.note.is_keyoff() {
                            if self.current.instrument.is_none() {
                                if let Some(i) = &mut self.instr {
                                    i.volume_reset();
                                }
                            } else {
                                self.trigger_pitch(TRIGGER_KEEP_NONE);
                            }
                        }
                    }
                }
                TrackEffect::NoteFadeOut { tick: _t, past: _p } => {
                    #[cfg(feature = "demo")]
                    println!("NoteFadeOut");
                    todo!();
                }
                TrackEffect::NoteOff { tick: t, past: _p } => {
                    // TODO: past
                    if current_tick == t {
                        self.key_off(t);
                    }
                }
                TrackEffect::NoteRetrig {
                    speed: s,
                    volume_modifier: m,
                } => {
                    //TODO: check if NoteDelay then check with current_tick - delay

                    let current_state = NoteRetrigState {
                        note: self.current.note,
                        instr: self.current.instrument,
                        speed: s,
                        volume_modifier: m.clone(),
                    };

                    if current_tick == 0 {
                        if self.effect_note_retrig_backup != current_state {
                            self.effect_note_retrig_counter = 0;
                            self.effect_note_retrig_backup = current_state;
                        }
                    }
                    // If speed (s) is 0, retrig is effectively disabled.
                    if s != 0 && self.effect_note_retrig_counter % s == 0 {
                        self.trigger_pitch(TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_ENVELOPE);
                        match m {
                            NoteRetrigOperator::None => {
                                // No volume modification, just retrigger.
                            }
                            NoteRetrigOperator::Sum(delta) => {
                                self.volume = (self.volume + delta).clamp(0.0, 1.0);
                            }
                            NoteRetrigOperator::Mul(factor) => {
                                self.volume = (self.volume * factor).clamp(0.0, 1.0);
                            }
                        }
                    }
                    // Increment retrig counter after processing
                    self.effect_note_retrig_counter += 1;
                }
                TrackEffect::Panbrello { speed: s, depth: d } => {
                    if current_tick == 0 {
                        self.effect_panbrello.tick0(s, d);
                    } else {
                        self.effect_panbrello.tick();
                    }
                }
                TrackEffect::PanbrelloWaveform {
                    waveform: w,
                    retrig: r,
                } => {
                    self.effect_panbrello.data.waveform = WaveformState::new(w);
                    if r {
                        self.effect_panbrello.retrigger();
                    }
                }
                TrackEffect::Panning(p) => {
                    if current_tick == 0 {
                        self.panning = p.clamp(0.0, 1.0);
                    }
                }
                TrackEffect::PanningSlide { speed: s, fine: f } => {
                    // Fine slides fire once at tick 0; non-fine slides fire on ticks > 0 only.
                    if f == (current_tick == 0) {
                        self.panning = (self.panning + s).clamp(0.0, 1.0);
                    }
                }
                TrackEffect::Portamento(p) => {
                    if current_tick == 0 {
                        self.effect_portamento = p;
                    } else {
                        self.period = (self.period + p).clamp(1.0, 32000.0 - 1.0);
                    }
                }
                TrackEffect::TonePortamento(p) => {
                    if current_tick == 0 {
                        self.effect_tone_portamento_goal =
                            self.period_helper.note_to_period(self.note);
                    } else {
                        if self.period != self.effect_tone_portamento_goal {
                            slide_towards(&mut self.period, self.effect_tone_portamento_goal, p);
                        }
                    }
                }
                TrackEffect::Tremolo { speed: s, depth: d } => {
                    if current_tick == 0 {
                        self.effect_tremolo.data.speed = s;
                        self.effect_tremolo.data.depth = d;
                    } else {
                        self.effect_tremolo.tick();
                    }
                }
                TrackEffect::TremoloWaveform {
                    waveform: w,
                    retrig: r,
                } => {
                    self.effect_tremolo.data.waveform = WaveformState::new(w);
                    if r {
                        self.effect_tremolo.retrigger();
                    }
                }
                TrackEffect::Tremor {
                    on_time: on,
                    off_time: off,
                } => {
                    if current_tick == 0 {
                        self.effect_tremor_on = on;
                        self.effect_tremor_off = off;
                        self.effect_tremor = false;
                    } else {
                        let on = self.effect_tremor_on;
                        let off = self.effect_tremor_off;
                        self.effect_tremor = (current_tick - 1) % (on + 1 + off + 1) > on;
                    }
                }
                TrackEffect::Vibrato { speed: s, depth: d } => {
                    if current_tick == 0 {
                        self.effect_vibrato.data.speed = s;
                        self.effect_vibrato.data.depth = d;
                    } else {
                        self.effect_vibrato.tick();
                    }
                }
                TrackEffect::VibratoSpeed(s) => {
                    if current_tick == 0 {
                        self.effect_vibrato.data.speed = s;
                    }
                }
                TrackEffect::VibratoDepth(d) => {
                    if current_tick == 0 {
                        self.effect_vibrato.data.depth = d;
                    }
                }
                TrackEffect::VibratoWaveform {
                    waveform: w,
                    retrig: r,
                } => {
                    self.effect_vibrato.data.waveform = WaveformState::new(w);
                    if r {
                        self.effect_vibrato.retrigger();
                    }
                }
                TrackEffect::Volume { value: v, tick: t } => {
                    if current_tick == t {
                        self.volume = v.clamp(0.0, 1.0);
                    }
                }
                TrackEffect::VolumeSlide { speed: s, fine: f } => {
                    // Fine slides fire once at tick 0; non-fine slides fire on ticks > 0 only.
                    if f == (current_tick == 0) {
                        self.volume = (self.volume + s).clamp(0.0, 1.0);
                    }
                }
            }
        }
    }

    /// Change instr and return true if it was the same
    fn tick0_change_instr(&mut self, sample_only: bool) -> bool {
        let instrnr = self.current.instrument.unwrap();

        if let InstrumentType::Default(id) = &self.module.instrument[instrnr].instr_type {
            let was_same = self.instr.as_ref().map_or(false, |i| i.num == instrnr);

            // Only proceed if the instrument has samples
            if !id.sample.is_empty() {
                if sample_only {
                    if let Some(i) = &mut self.instr {
                        i.replace_instr(id);
                    }
                } else {
                    self.instr = Some(StateInstrDefault::new(
                        id,
                        instrnr,
                        self.period_helper.clone(),
                        self.rate,
                    ));
                }
            }

            return was_same;
        } else {
            // TODO
            return false;
        }
    }

    /// Return true if it was the same instrument
    fn tick0_load_instrument(&mut self) -> bool {
        if let Some(instr) = self.current.instrument {
            if instr >= self.module.instrument.len() {
                // Invalid instrument, cut current note
                self.cut_pitch();
                self.instr = None;
                return false;
            }
        } else {
            // No instrument to load
            return true;
        }

        if self.current.has_tone_portamento() {
            self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_SAMPLE_POSITION);
            return self.tick0_change_instr(true);
        }

        if self.current.note.is_none() {
            /* Ghost instrument, trigger note */
            let trigger_flags = if self.current.has_volume_slide() {
                TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_PERIOD
            } else {
                /* Sample position is kept, but envelopes are reset */
                TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_PERIOD
            };
            self.trigger_pitch(trigger_flags);
            return self.tick0_change_instr(true);
        }

        if self.current.note.is_keyoff() {
            self.trigger_pitch(TRIGGER_KEEP_PERIOD);
            return true; // Keyoff does not change instrument
        }

        return self.tick0_change_instr(false);
    }

    fn tick0_load_pitch(&mut self, new_instr: bool) {
        // Note is not valid? Return early.
        if !self.current.note.is_valid() {
            if self.current.note.is_keyoff() {
                if self.current.instrument.is_none() || new_instr {
                    self.key_off(0);
                } else {
                    self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE);
                }
            }
            return;
        }

        // Instr?
        if let Some(instr) = &mut self.instr {
            // Portamento?
            if self.current.has_tone_portamento() {
                if let Some(s) = &instr.state_sample {
                    if s.is_enabled() {
                        self.note = self.current.note.value() as f32 + s.get_finetuned_pitch();
                        return;
                    }
                }
                self.cut_pitch();
                return;
            }

            // SetNote
            if instr.set_pitch(self.current.note) {
                if let Some(s) = &instr.state_sample {
                    self.note = self.current.note.value() as f32 + s.get_finetuned_pitch();
                }

                let trigger_flag = if self.current.instrument.is_some() {
                    TRIGGER_KEEP_NONE
                } else {
                    /* Ghost note: keep old volume */
                    TRIGGER_KEEP_VOLUME
                };
                self.trigger_pitch(trigger_flag);
                return;
            }
        }

        self.cut_pitch();
    }

    fn tick0_load_instrument_and_pitch(&mut self) {
        if self.historical.is_some() {
            if self.current.has_note_off() {
                // Historical Kxy effect bug
                return;
            }
        }

        // First, load instr
        let new_instr: bool = self.tick0_load_instrument();
        // Next, choose sample from note
        self.tick0_load_pitch(new_instr);
    }

    pub(crate) fn tick0(&mut self, pattern_slot: &TrackUnit) {
        self.current = pattern_slot.clone();

        let delay = self.current.get_delay();
        if delay != 0 {
            self.effect_note_delay = delay;
        } else {
            /* load instrument then note */
            self.tick0_load_instrument_and_pitch();
            if let Some(instr) = &mut self.instr {
                instr.tick();
            }
            self.tickn_effects(0);

            if self.effect_arpeggio.in_progress() && !self.current.has_arpeggio() {
                self.effect_arpeggio.retrigger();
            }

            if self.effect_vibrato.in_progress() && !self.current.has_vibrato() {
                self.effect_vibrato.retrigger();
            }

            self.tickn_update_instr();
        }
    }
}

impl<'a> Iterator for Channel<'a> {
    type Item = (f32, f32);

    // Was next_of_sample()
    fn next(&mut self) -> Option<Self::Item> {
        match &mut self.instr {
            Some(i) => match i.next() {
                Some(fval) => Some((
                    fval.0 * self.actual_volume[0],
                    fval.1 * self.actual_volume[1],
                )),
                None => None,
            },
            None => None,
        }
    }
}
