//! Per-channel OPL (FM) glue — kept in one place so the sample engine in
//! the sibling `channel` modules stays unconcerned with FM.
//!
//! A channel that triggers an `InstrumentType::Opl` instrument does **not**
//! install a sample voice; instead it records the gesture here. Each tick,
//! `Voices` drains the channel via [`Channel::take_opl_update`] and drives
//! the shared `OplDriver`. Pitch/volume/pan are derived from the same
//! note/instrument data the sample path would use, so porta-free FM notes
//! land at the right frequency. (Pitch slides / vibrato on FM voices are a
//! later refinement — Phase A is note-level.)

use xmrs::prelude::*;

use super::Channel;

/// Per-tick0 edge for an OPL channel, consumed by `Voices`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum OplEdge {
    /// Nothing happened this tick (held note continues, or no FM here).
    #[default]
    None,
    /// A note was (re)triggered — allocate/key-on.
    Trigger,
    /// Key released (note-off / fade).
    KeyOff,
    /// Hard cut — release the hardware slot.
    Cut,
}

/// The OPL state a channel carries (mutually exclusive with a live sample
/// voice — a channel sounds one or the other).
#[derive(Clone, Default)]
pub(crate) struct OplChannelState {
    /// Module-instrument index of the held OPL instrument, if any.
    pub instr: Option<usize>,
    /// The note currently sounding (for frequency + retrigger).
    pub pitch: Option<Pitch>,
    /// Edge produced at the last row-load tick.
    pub edge: OplEdge,
}

/// What `Voices` needs to drive the chip for one channel this tick.
pub(crate) struct OplChannelUpdate {
    pub edge: OplEdge,
    /// Present while an OPL instrument is held — the live frequency /
    /// volume / pan to push every tick.
    pub held: Option<OplHeld>,
}

pub(crate) struct OplHeld {
    pub instr: usize,
    pub milli_hz: u32,
    pub vol63: u8,
    pub pan_l: bool,
    pub pan_r: bool,
}

impl<'a> Channel<'a> {
    /// True when the current row carries a note-column event (trigger,
    /// key-off, cut, fade or instrument reset) — as opposed to an
    /// effects-only or empty row, which must leave a held FM note alone.
    fn row_has_note_event(&self) -> bool {
        matches!(
            self.current.event,
            CellEvent::NoteOn { .. }
                | CellEvent::NoteOnGhost { .. }
                | CellEvent::NoteOff { .. }
                | CellEvent::NoteCut
                | CellEvent::NoteFade
                | CellEvent::InstrReset
        )
    }

    /// Does this row's note event target an OPL instrument? Returns the
    /// instrument index if so (explicit OPL instrument column, or a
    /// note-only row on a channel already holding an OPL instrument).
    /// `None` for sample instruments and non-note rows.
    pub(super) fn row_targets_opl(&self) -> Option<usize> {
        if !self.row_has_note_event() {
            return None;
        }
        if self.current.has_instrument_column() {
            match self.current_track_instrument {
                Some(i) => match self.module.instrument.get(i).map(|x| &x.instr_type) {
                    Some(InstrumentType::Opl(_)) => Some(i),
                    // An explicit sample instrument takes the channel.
                    _ => None,
                },
                None => self.opl.instr,
            }
        } else {
            // Note-only row: stays on the held OPL instrument if any.
            self.opl.instr
        }
    }

    /// Handle a row that targets an OPL instrument. Records the edge +
    /// note; never touches the sample voice pool.
    pub(super) fn tick0_opl(&mut self, opl_instr: usize) {
        match self.current.event {
            CellEvent::NoteOn { pitch, velocity } | CellEvent::NoteOnGhost { pitch, velocity } => {
                self.opl.instr = Some(opl_instr);
                self.opl.pitch = Some(pitch);
                self.opl.edge = OplEdge::Trigger;
                // Fresh note: seed the channel volume from the cell's
                // velocity (full when no volume column), so volume effects
                // layer on from there.
                self.volume = velocity;
                self.struck_this_tick = true;
                // Seed the pitch accumulators (`note`/`period`) so per-tick
                // porta / pitch-slide / vibrato track the FM note exactly
                // like a sample note. The instrument's relative_pitch +
                // finetune fold into the played pitch here.
                if let Some(delta) = self.opl_finetune_delta(opl_instr) {
                    self.note = super::compose_played_pitch(pitch, delta);
                    self.period = self.period_helper.note_to_period(self.note);
                    self.effective_period = self.period;
                }
            }
            CellEvent::InstrReset => {
                // Ghost retrigger at the current pitch.
                self.opl.instr = Some(opl_instr);
                self.opl.edge = OplEdge::Trigger;
                self.struck_this_tick = true;
            }
            CellEvent::NoteOff { .. } | CellEvent::NoteFade => {
                self.opl.edge = OplEdge::KeyOff;
            }
            CellEvent::NoteCut => {
                self.opl.edge = OplEdge::Cut;
                self.opl.instr = None;
            }
            CellEvent::None => {}
        }
    }

    /// A sample note is taking this channel — cut any held OPL note so the
    /// chip slot is released. Self-guards on `row_has_note_event` so an
    /// effects-only / empty row leaves a held FM note sounding (its slides
    /// keep tracking).
    pub(super) fn opl_clear_for_sample(&mut self) {
        if self.row_has_note_event() && self.opl.instr.is_some() {
            self.opl.edge = OplEdge::Cut;
            self.opl.instr = None;
            self.opl.pitch = None;
        }
    }

    /// Drain this channel's OPL state for the driver, consuming the edge.
    pub(crate) fn take_opl_update(&mut self) -> Option<OplChannelUpdate> {
        let edge = core::mem::take(&mut self.opl.edge);
        let held = match (self.opl.instr, self.opl.pitch) {
            (Some(i), Some(pitch)) => {
                let module = self.module;
                match module.instrument.get(i).map(|x| &x.instr_type) {
                    Some(InstrumentType::Opl(o)) => {
                        let (pan_l, pan_r) = self.opl_pan();
                        let _ = pitch; // pitch is now carried by `self.period`
                        Some(OplHeld {
                            instr: i,
                            milli_hz: self.opl_current_milli_hz(),
                            vol63: self.opl_vol63(o),
                            pan_l,
                            pan_r,
                        })
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if edge == OplEdge::None && held.is_none() {
            None
        } else {
            Some(OplChannelUpdate { edge, held })
        }
    }

    /// The instrument's `relative_pitch` + `finetune` as a single Q8.8
    /// `PitchDelta`, mirroring `StateSample::get_finetuned_pitch` so an FM
    /// note tunes exactly like a sample of the same instrument. `None` if
    /// the index isn't an OPL instrument.
    fn opl_finetune_delta(&self, instr_idx: usize) -> Option<PitchDelta> {
        match self.module.instrument.get(instr_idx).map(|x| &x.instr_type) {
            Some(InstrumentType::Opl(o)) => {
                let rel_q88 = (o.relative_pitch as i16).saturating_mul(256);
                let fine_q88 = o.finetune.into_q15().raw() >> 7;
                Some(PitchDelta::from_q8_8_i16(rel_q88.saturating_add(fine_q88)))
            }
            _ => None,
        }
    }

    /// Current played frequency (milli-Hertz) from the channel's live
    /// `period` accumulator — which already carries portamento, pitch
    /// slides and arpeggio — plus the effect vibrato. On Amiga formats
    /// (S3M) vibrato is a direct period delta, exactly as the sample path
    /// folds it in `StateInstrDefault::update_frequency`. (Linear-format
    /// FM is Phase C; its vibrato is semitone-space and not folded here.)
    fn opl_current_milli_hz(&self) -> u32 {
        let mut period = self.period;
        if matches!(
            self.period_helper.freq_type,
            FrequencyType::AmigaFrequencies
        ) {
            period = period.saturating_add_signed(self.vibrato_value_raw());
        }
        let freq = self.period_helper.period_to_frequency(period);
        // `freq` is the **sample-playback** rate (Q24.8 Hz), ~32× the note's
        // musical pitch. The OPL needs the musical milli-Hertz: convert
        // exactly as Schism (`sndmix.c`): `freq_hz × 261625 / 8363`, where
        // 8363 = ST3 middle-C sample rate and 261625 = middle C (261.625 Hz)
        // in milli-Hertz. Without this the note lands ~32× too high, clamps
        // at the OPL's max F-number, and screeches.
        let freq_hz_q8 = freq.raw_q24_8() as u64; // Hz × 256
        ((freq_hz_q8 * 261_625) / (256 * 8363)) as u32
    }

    /// Final 0..63 OPL volume = instrument volume × channel volume × cell
    /// volume (Q15), with the effect tremolo folded in.
    fn opl_vol63(&self, instr: &xmrs::core::instr_opl::InstrOpl) -> u8 {
        let vol_q15 =
            (self.volume.as_q15_i32() + self.tremolo_value_q15().raw() as i32).clamp(0, 32768);
        let chan_q15 = (vol_q15 * self.channel_volume.as_q15_i32()) >> 15;
        ((instr.volume as i32 * chan_q15) >> 15).clamp(0, 63) as u8
    }

    /// Channel pan → OPL stereo enable bits, with Schism's thresholds
    /// (`OPL_Pan`): hard-left below ~⅓, hard-right above ~⅔, else both.
    fn opl_pan(&self) -> (bool, bool) {
        let p = self.panning.as_q15_i32(); // 0..32768
        (p <= 21_760, p >= 10_880)
    }
}
