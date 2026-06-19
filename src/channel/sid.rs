//! Per-channel SID glue — the cycle-accurate-SID counterpart of
//! [`super::opl`]. A channel that triggers an `InstrumentType::RobSid`
//! instrument records a gesture here instead of installing a sample voice;
//! each tick `Voices` drains it via [`Channel::take_sid_update`] and drives
//! the per-instrument [`xmrs::generators::sid::driver::SidBank`].
//!
//! Phase A is note-level + per-tick pitch (porta / slide / vibrato track the
//! note like a sample voice). The SID has no per-voice volume register, so
//! dynamics come from the patch ADSR; per-track volume / pan / per-voice
//! routing are a later phase (not yet wired).

use xmrs::prelude::*;

use super::Channel;

/// Per-tick0 edge for a SID channel, consumed by `Voices`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum SidEdge {
    /// Nothing happened this tick (held note continues, or no SID here).
    #[default]
    None,
    /// A note was (re)triggered — allocate/key-on.
    Trigger,
    /// Key released (note-off / fade).
    KeyOff,
    /// Key released by an APPENDED note-stream entry (a note-off cell that also
    /// carries an instrument column = the replayer fetched the next note entry
    /// and cleared the gate). This is a note-FETCH frame: the replayer JMPs past
    /// the per-frame effects, so the PW sweep / vibrato / drum are NOT advanced
    /// this frame (the swept values HOLD), exactly like a trigger. The voice
    /// then releases on the following held frames, which do advance the effects.
    KeyOffFetch,
    /// Hard cut — release the chip voice.
    Cut,
}

/// The SID state a channel carries (mutually exclusive with a live sample
/// voice — a channel sounds one or the other).
#[derive(Clone, Default)]
pub(crate) struct SidChannelState {
    /// Module-instrument index of the held SID instrument, if any.
    pub instr: Option<usize>,
    /// The note currently sounding (for frequency + retrigger).
    pub pitch: Option<Pitch>,
    /// Edge produced at the last row-load tick.
    pub edge: SidEdge,
    /// Linear freq slide accumulator, in **SID freq-register units**, added on
    /// top of the note's base register by [`Channel::sid_current_milli_hz`]. The
    /// SID is a linear-frequency chip, so its per-frame freq-register sweep
    /// (Delta's `$c338` slide, decoded as a portamento on the `TrackPitch` Slide
    /// lane) must accumulate linearly in register space, not in the semitone-
    /// linear `self.period`. Register space is also where the chip's 16-bit
    /// **wrap** lives: a long sweep runs past `0xFFFF` and resumes from the low
    /// end (the repeating "zip"), which a clamp would freeze. Reset to 0 on every
    /// (re)trigger; advanced once per frame while a slide is armed.
    pub freq_slide_reg: i32,
}

/// What `Voices` needs to drive the bank for one channel this tick.
pub(crate) struct SidChannelUpdate {
    pub edge: SidEdge,
    /// Present while a SID instrument is held — the live frequency to push.
    pub held: Option<SidHeld>,
}

pub(crate) struct SidHeld {
    pub instr: usize,
    pub milli_hz: u32,
}

/// PAL C64 system clock (Hz). The SID freq register is `Hz · 2^24 / clock`, so
/// `milli_hz · 2^24 / (clock · 1000)` is the register value — used to fold the
/// freq-register slide (with its 16-bit wrap) back into musical milli-Hertz.
#[cfg(feature = "import_sid")]
const PAL_CLOCK_HZ: u64 = 985_248;

impl<'a> Channel<'a> {
    /// True when the current row carries a note-column event (trigger,
    /// key-off, cut, fade or instrument reset).
    fn sid_row_has_note_event(&self) -> bool {
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

    /// Does this row's note event target a SID instrument? Returns the
    /// instrument index if so. `None` for sample/OPL instruments and
    /// non-note rows.
    pub(super) fn row_targets_sid(&self) -> Option<usize> {
        if !self.sid_row_has_note_event() {
            return None;
        }
        if self.current.has_instrument_column() {
            match self.current_track_instrument {
                Some(i) => match self.module.instrument.get(i).map(|x| &x.instr_type) {
                    Some(InstrumentType::RobSid(_)) => Some(i),
                    _ => None,
                },
                None => self.sid.instr,
            }
        } else {
            // Note-only row: stays on the held SID instrument if any.
            self.sid.instr
        }
    }

    /// Handle a row that targets a SID instrument. Records the edge + note;
    /// never touches the sample voice pool.
    pub(super) fn tick0_sid(&mut self, sid_instr: usize) {
        match self.current.event {
            CellEvent::NoteOn { pitch, velocity } | CellEvent::NoteOnGhost { pitch, velocity } => {
                self.sid.instr = Some(sid_instr);
                self.sid.pitch = Some(pitch);
                self.sid.edge = SidEdge::Trigger;
                self.volume = velocity;
                self.struck_this_tick = true;
                // Seed the pitch accumulators so per-tick porta / slide /
                // vibrato track the SID note. Delta is zero: the bare note
                // plus the shared `period→musical-Hz` conversion already
                // yields the note's musical pitch — see `sid_current_milli_hz`.
                self.note = super::compose_played_pitch(pitch, PitchDelta::from_q8_8_i16(0));
                self.period = self.period_helper.note_to_period(self.note);
                self.effective_period = self.period;
            }
            CellEvent::InstrReset => {
                self.sid.instr = Some(sid_instr);
                self.sid.edge = SidEdge::Trigger;
                self.struck_this_tick = true;
            }
            CellEvent::NoteOff { .. } | CellEvent::NoteFade => {
                // A note-off cell that ALSO carries an instrument column is the
                // importer's encoding of an appended note-stream entry (gate
                // cleared at the same pitch) = a note-FETCH frame in the
                // replayer, which skips the per-frame effects. A bare note-off
                // (no instrument column) is an ordinary release that keeps the
                // effects (PW sweep) running.
                self.sid.edge = if self.current.has_instrument_column() {
                    SidEdge::KeyOffFetch
                } else {
                    SidEdge::KeyOff
                };
            }
            CellEvent::NoteCut => {
                self.sid.edge = SidEdge::Cut;
                self.sid.instr = None;
            }
            CellEvent::None => {}
        }
    }

    /// A sample note is taking this channel — cut any held SID note so the
    /// chip voice is released. Self-guards on a note event so an
    /// effects-only / empty row leaves a held SID note sounding.
    pub(super) fn sid_clear_for_sample(&mut self) {
        if self.sid_row_has_note_event() && self.sid.instr.is_some() {
            self.sid.edge = SidEdge::Cut;
            self.sid.instr = None;
            self.sid.pitch = None;
        }
    }

    /// Drain this channel's SID state for the bank, consuming the edge.
    pub(crate) fn take_sid_update(&mut self) -> Option<SidChannelUpdate> {
        let edge = core::mem::take(&mut self.sid.edge);
        // The freq-register slide reseeds to the note base on every (re)trigger;
        // on every other frame it accumulates while the Slide lane is armed.
        if edge == SidEdge::Trigger {
            self.sid.freq_slide_reg = 0;
        } else {
            self.sid_advance_freq_slide();
        }
        let held = match (self.sid.instr, self.sid.pitch) {
            (Some(i), Some(_)) => match self.module.instrument.get(i).map(|x| &x.instr_type) {
                Some(InstrumentType::RobSid(_)) => Some(SidHeld {
                    instr: i,
                    milli_hz: self.sid_current_milli_hz(),
                }),
                _ => None,
            },
            _ => None,
        };
        if edge == SidEdge::None && held.is_none() {
            None
        } else {
            Some(SidChannelUpdate { edge, held })
        }
    }

    /// Current played frequency (milli-Hertz) from the channel's live
    /// `period` accumulator (portamento / slides / arpeggio already folded),
    /// plus the effect vibrato on Amiga formats. Identical conversion to the
    /// OPL path: the channel `period` is a sample-playback rate; `× 261625 /
    /// 8363` rescales it to the note's musical milli-Hertz (8363 = the ST3
    /// reference rate, 261625 = middle C in milli-Hz). This factor (≈ /32)
    /// also exactly cancels the relative-pitch+wavetable scaling the legacy
    /// fake-sample path used, so the synth lands on the same musical pitch.
    fn sid_current_milli_hz(&self) -> u32 {
        let mut period = self.period;
        if matches!(
            self.period_helper.freq_type,
            FrequencyType::AmigaFrequencies
        ) {
            period = period.saturating_add_signed(self.vibrato_value_raw());
        }
        let freq = self.period_helper.period_to_frequency(period);
        let freq_hz_q8 = freq.raw_q24_8() as u64; // Hz × 256
        let base = ((freq_hz_q8 * 261_625) / (256 * 8363)) as u32;
        if self.sid.freq_slide_reg == 0 {
            return base;
        }
        // Fold in the freq-register slide. The slide sweeps the chip's 16-bit
        // register linearly and *wraps* at 0x10000, so do the add in register
        // space (base → reg, + slide, wrap), then convert back to milli-Hz. The
        // synth's milli-Hz → reg step is the inverse of this, so the round-trip
        // reproduces the wrapped register exactly.
        let base_reg = (((base as u64) << 24) / (PAL_CLOCK_HZ * 1000)) as i64;
        let swept = (base_reg + self.sid.freq_slide_reg as i64).rem_euclid(0x1_0000);
        (((swept as u64) * PAL_CLOCK_HZ * 1000) >> 24) as u32
    }

    /// Advance the SID freq-register slide by one frame. Reads the channel's
    /// `TrackPitch` Slide lane (the same lane a sample voice's Portamento drives
    /// via [`Channel::apply_pitch_slide_state`]) and accumulates a *linear*
    /// freq-register delta — the chip sweeps its freq register linearly, whereas
    /// a period-space porta would be exponential in Hz on this linear-freq chip.
    ///
    /// The lane `rate` is the semitone-linear period delta the importer encoded
    /// (`-4 · param`, negative = pitch up; `param = reg_delta >> 6`). We recover
    /// the chip's per-frame freq-register step as `|rate| << 4`. The `>> 6`/`<< 4`
    /// round-trip drops the slide step's low byte (`$c335`), a ≲2 % rate
    /// approximation — exact carriage would need a per-cell SID slide field.
    #[cfg(feature = "import_sid")]
    fn sid_advance_freq_slide(&mut self) {
        use xmrs::core::daw::automation::AutomationTarget;
        // Mirror `apply_slides_from_lanes`: the slide is keyed on the row-start
        // tick and the clip-resolved track, not `self.track_index`.
        let abs_tick = self.current_abs_tick;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, self.current_song_idx) else {
            return;
        };
        let tgt = AutomationTarget::TrackPitch(track_idx);
        let Some(state) = self
            .module
            .lanes_for(tgt, self.current_song_idx)
            .find_map(|l| l.slide_state_at(abs_tick))
        else {
            return;
        };
        if !state.armed {
            return;
        }
        let rate = state.rate.raw(); // period delta/frame (negative = pitch up)
        if rate == 0 {
            return;
        }
        // reg_delta ≈ |rate| << 4 ; pitch-up (rate < 0) sweeps the register up.
        let reg_delta = ((rate.unsigned_abs() as i64) << 4) * if rate < 0 { 1 } else { -1 };
        self.sid.freq_slide_reg = (self.sid.freq_slide_reg as i64 + reg_delta)
            .clamp(i32::MIN as i64, i32::MAX as i64) as i32;
    }
}
