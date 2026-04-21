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
    effect_tone_portamento_goal: f32,
    effect_tremolo: EffectVibratoTremolo,
    effect_tremor: bool,
    effect_tremor_on: usize,
    effect_tremor_off: usize,
    effect_vibrato: EffectVibratoTremolo,
    /// If `true`, the next note trigger resets the vibrato phase to 0;
    /// otherwise the phase carries over. Set from
    /// `TrackEffect::VibratoWaveform::retrig`. Defaults to `true`.
    vibrato_retrig_on_new_note: bool,
    /// Same as `vibrato_retrig_on_new_note` but for tremolo, driven by
    /// `TrackEffect::TremoloWaveform::retrig`.
    tremolo_retrig_on_new_note: bool,
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
    /// Build a channel. Format-specific quirks (FT2 arpeggio LUT,
    /// FT2 arpeggio period clamp) are driven by `module.format` via
    /// `EffectArpeggio` and `PeriodHelper`; Channel itself no longer
    /// carries an `Ft2Quirks` field, so there is only one source of
    /// truth for "is this an XM?".
    ///
    /// `tempo` is the initial song speed, forwarded to the arpeggio
    /// effect so its FT2 LUT is usable on the very first row. The
    /// player must subsequently call [`Self::set_tempo`] whenever the
    /// song tempo changes (Fxx effect) to keep the LUT index in sync.
    pub(crate) fn new(module: &'a Module, rate: f32, tempo: usize) -> Self {
        let period_helper = PeriodHelper::new(module.frequency_type, module.format);
        Self {
            module,
            period_helper: period_helper.clone(),
            rate,
            channel_volume: 1.0,
            volume: 1.0,
            panning: 0.5,
            note: 0.0,
            current: TrackUnit::default(),
            period: 0.0,
            instr: None,
            effect_arpeggio: EffectArpeggio::new(module.format, tempo),
            effect_note_retrig_backup: NoteRetrigState::default(),
            effect_note_retrig_counter: 0,
            effect_panbrello: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremolo: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremor: false,
            effect_tremor_on: 0,
            effect_tremor_off: 0,
            effect_vibrato: EffectVibratoTremolo::new(Waveform::Sine),
            vibrato_retrig_on_new_note: true,
            tremolo_retrig_on_new_note: true,
            effect_tone_portamento_goal: 0.0,
            effect_semitone: false,
            effect_note_delay: 0,
            pattern_loop_origin: 0,
            pattern_loop_count: 0,
            muted: false,
            actual_volume: [0.0, 0.0],
        }
    }

    /// Forward a tempo change to the arpeggio effect. Called by the
    /// player on every step so the FT2 arpeggio LUT is indexed from
    /// the current speed rather than a stale copy.
    pub(crate) fn set_tempo(&mut self, tempo: usize) {
        self.effect_arpeggio.set_tempo(tempo);
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

    /// Release the currently-sustaining note.
    ///
    /// Behaviour depends on the module's canonical format (`module.format`),
    /// not on the FT2-quirks flag, because "how a note-off should sound" is
    /// a format-level semantic, not an FT2 bug:
    ///
    /// * `ModuleFormat::Xm` — reproduce `ft2_replayer.c:keyOff()`:
    ///   if the instrument has a volume envelope, flip `sustained` so the
    ///   envelope enters its release segment and `volume_fadeout` starts
    ///   counting down; otherwise CUT the channel volume to zero
    ///   immediately. The FT2 source branches identically on
    ///   `volEnvFlags & ENV_ENABLED`.
    ///
    /// * `ModuleFormat::It` / `Mod` / `S3m` / `Unknown` — rely on
    ///   `StateInstrDefault::key_off()` which flips `sustained = false`
    ///   (and cuts iff the instrument has no envelope AND zero fadeout).
    ///   This is the IT release model and the sensible fallback
    ///   elsewhere.
    ///
    /// The `tick` parameter is kept in the signature so callers (Kxx
    /// effect at ticks > 0, note-column `===` at tick 0, note-delay
    /// paths) don't need to case-split. FT2 itself also ignores which
    /// tick the key-off lands on — the branch on envelope is the only
    /// real decision.
    fn key_off(&mut self, _tick: usize) {
        let Some(i) = &mut self.instr else {
            self.cut_pitch();
            return;
        };

        let had_vol_env = i.has_volume_envelope();
        i.key_off();

        // XM canonical: FT2's `keyOff()` cuts `realVol` / `outVol`
        // to zero (quick-ramped, anti-click) when the instrument has
        // no volume envelope. This is not an FT2 bug — it is how XM
        // modules are meant to sound — so it is gated on the module's
        // declared format, not on the FT2-quirks opt-in.
        if self.module.format == ModuleFormat::Xm && !had_vol_env {
            self.cut_pitch();
        }
    }

    pub(crate) fn trigger_pitch(&mut self, flags: TriggerKeep) {
        self.effect_tremor = false;

        if let Some(instr) = &mut self.instr {
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

                // Reset the effect vibrato / tremolo phase on a new-note
                // trigger, unless the channel flag explicitly says to
                // keep it. Tone portamento, ghost instruments and keyoffs
                // take the `TRIGGER_KEEP_PERIOD` path above and skip this
                // block entirely — they don't count as new-note triggers.
                if self.vibrato_retrig_on_new_note {
                    self.effect_vibrato.retrigger();
                }
                if self.tremolo_retrig_on_new_note {
                    self.effect_tremolo.retrigger();
                }

                instr.update_frequency(
                    self.period,
                    0.0,
                    self.effect_vibrato.value(),
                    self.effect_semitone,
                );
            }
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
                    if current_tick == 0 && self.current.note.is_valid() {
                        if let Some(instr) = &mut self.instr {
                            if let Some(sample) = &mut instr.state_sample {
                                sample.set_position(seek);
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

                    if current_tick == 0 && self.effect_note_retrig_backup != current_state {
                        self.effect_note_retrig_counter = 0;
                        self.effect_note_retrig_backup = current_state;
                    }

                    // Increment the counter FIRST, then test.
                    self.effect_note_retrig_counter += 1;

                    // If speed (s) is 0, retrig is effectively disabled.
                    if s != 0 && self.effect_note_retrig_counter.is_multiple_of(s) {
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
                TrackEffect::Portamento { speed: p, fine: f } => {
                    // Effect memory is resolved at import time in
                    // `xmrs/src/import/import_memory.rs`, so by the time
                    // this arm runs `p` is always the final (non-zero)
                    // value — we only need to apply the slide and never
                    // stash anything.
                    //
                    // `fine = false` (regular slide, e.g. 1xx/2xx in XM,
                    // Exx/Fxx in S3M/IT when hi-nibble < 0xE) fires at
                    // every non-zero tick. `fine = true` (E1x/E2x,
                    // X1y/X2y, EFx/FFx, EEx/FEx) fires exactly once at
                    // tick 0. Previously both collapsed into a single
                    // tick-scope, causing fine slides to be over-applied
                    // by a factor of `(speed − 1)`.
                    //
                    // The format-specific clamping semantics apply
                    // identically in both scopes:
                    //
                    // * **S3M** clamps the period at 0 (Scream Tracker
                    //   3 behaviour — lets the note go infinitely
                    //   high).
                    //
                    // * **XM** reproduces FT2's portamento-down
                    //   signed-overflow quirk
                    //   (`ft2_replayer.c:pitchSlideDown` at line 1914,
                    //   and the parallel bugs in `finePitchSlideDown`
                    //   line 643 and `extraFinePitchSlide` line 1213 —
                    //   all three are explicitly marked "FT2 bug" in
                    //   the source). The clamp
                    //   `if ((int16_t)realPeriod >= 32000)` uses a
                    //   signed comparison: values in [32000, 32767]
                    //   snap back to 31999, but once the period crosses
                    //   32767 the int16 cast flips negative and the
                    //   clamp is silently skipped, letting the period
                    //   keep growing. Some XM modules exploit this to
                    //   produce very deep pitch slides.
                    //
                    // * Other formats (Mod, It) use the plain clamp
                    //   at 31999.
                    let fires = if f {
                        current_tick == 0
                    } else {
                        current_tick != 0
                    };
                    if fires {
                        let min_period = if self.module.format == ModuleFormat::S3m {
                            0.0
                        } else {
                            1.0
                        };
                        let new_period = self.period + p;
                        // The FT2 signed-overflow quirk is **only** in
                        // pitchSlideDown (p > 0 = period grows). FT2's
                        // pitchSlideUp has a plain min-clamp, not a
                        // max-clamp, so a portamento up that happens
                        // to land in the [32000, 32767] window must
                        // NOT snap to 31999 — it just passes through.
                        // Gated on `p > 0.0`.
                        if self.module.format == ModuleFormat::Xm && p > 0.0 {
                            let wrapped = (new_period as i32).rem_euclid(65536) as i16;
                            if (32000..=32767).contains(&(wrapped as i32)) {
                                self.period = 31999.0;
                            } else {
                                // wrapped < 32000: still climbing
                                //   through the normal range.
                                // wrapped ≥ 32768 (i16-negative):
                                //   FT2's signed check fails, no clamp
                                //   fires, period keeps growing.
                                self.period = new_period.max(min_period);
                            }
                        } else {
                            self.period = new_period.clamp(min_period, 32000.0 - 1.0);
                        }
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
                    // Update the waveform and latch the retrig flag for
                    // the next note trigger; don't reset the phase now.
                    self.effect_tremolo.data.waveform = WaveformState::new(w);
                    self.tremolo_retrig_on_new_note = r;
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
                    } else if self.module.format == ModuleFormat::Xm && !self.current.has_vibrato()
                    {
                        // FT2 vol-column Bx quirk: when the slot carries a
                        // vol-column vibrato-depth with NO main-effect 4xx
                        // accompanying it, FT2's `v_Vibrato` still advances
                        // the vibrato LFO every non-zero tick
                        // (ft2_replayer.c:v_Vibrato calls doVibrato after
                        // optionally updating the depth). Without this call
                        // the LFO would freeze at its last position and
                        // only the depth would change, which is perceptibly
                        // wrong on XM modules that drive vibrato primarily
                        // from the vol column.
                        //
                        // When a main Vibrato IS on the row, its own arm
                        // (below) calls `tick()`, so we must NOT double-
                        // tick here — hence the `!has_vibrato()` guard.
                        self.effect_vibrato.tick();
                    }
                }
                TrackEffect::VibratoWaveform {
                    waveform: w,
                    retrig: r,
                } => {
                    // Same as TremoloWaveform above: latch the flag, no
                    // immediate phase reset.
                    self.effect_vibrato.data.waveform = WaveformState::new(w);
                    self.vibrato_retrig_on_new_note = r;
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
            let was_same = self.instr.as_ref().is_some_and(|i| i.num == instrnr);

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

            was_same
        } else {
            // TODO
            false
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

        self.tick0_change_instr(false)
    }

    fn tick0_load_pitch(&mut self, was_same_instr: bool) {
        // Note is not valid? Return early.
        if !self.current.note.is_valid() {
            if self.current.note.is_keyoff() {
                if self.current.instrument.is_none() || was_same_instr {
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
        // FT2 "K00 eats note" quirk. In FT2, K00 is handled specially at
        // tickZero via getNewNote (ft2_replayer.c:1418): it fires a keyoff
        // and then RETURNS, never running triggerNote — so the note column
        // of that row is effectively dropped.
        //
        // Key constraints:
        //   - This is canonical XM behaviour: `module.format ==
        //     ModuleFormat::Xm` is now the single source of truth for
        //     "apply FT2 replay semantics", so every XM module gets
        //     this quirk automatically. (There is no longer a separate
        //     `ft2_quirks` flag — format IS the switch.)
        //   - Only K00 (param 0) short-circuits. For Kxy with y > 0 FT2
        //     takes the normal getNewNote path and then fires keyOffCmd at
        //     tick y, which means the note DOES play. Use
        //     `has_note_off_at_tick_zero()` rather than `has_note_off_effect()`,
        //     which would also match non-zero Kxy and wrongly eat their
        //     notes.
        //   - Note-column `===` (`Pitch::Off`) has a different semantic
        //     (see `key_off()`) and belongs on the normal tick-0 path
        //     via `tick0_load_pitch`, hence the helper distinguishes
        //     effect-Kxx from column-keyoff.
        if self.module.format == ModuleFormat::Xm && self.current.has_note_off_at_tick_zero() {
            return;
        }

        // First, load instr. `was_same_instr` is true when the instrument
        // slot on this row matches the one currently held by the channel
        // (or when there's no instrument to load — see tick0_load_instrument
        // for the exact contract). This flag drives the keyoff branch in
        // tick0_load_pitch: a keyoff with a *different* instrument must
        // retrigger rather than just cut.
        let was_same_instr: bool = self.tick0_load_instrument();
        // Next, choose sample from note
        self.tick0_load_pitch(was_same_instr);
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
                // Arpeggio state is row-local (it's always restarted from
                // tick=0 whenever the effect reappears via `tick0`), so a
                // full retrigger is correct here.
                self.effect_arpeggio.retrigger();
            }

            if self.effect_vibrato.in_progress() && !self.current.has_vibrato() {
                // When leaving a Vibrato (main-effect 4xx/6xx) row, clear
                // the lingering period offset so the next row plays at
                // `realPeriod`.
                //
                // Exception: XM vol-column Bx produces a standalone
                // `VibratoDepth` without a `Vibrato`. FT2 keeps the LFO
                // alive in that case (see the VibratoDepth arm above), so
                // we must NOT clear the output — otherwise the vibrato
                // would audibly cut in and out on every row that only
                // carries vol-col B.
                let xm_volcol_b_alone =
                    self.module.format == ModuleFormat::Xm && self.current.has_vibrato_depth();
                if !xm_volcol_b_alone {
                    // Only clear the output modulation.
                    self.effect_vibrato.clear_output();
                }
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
