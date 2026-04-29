//! MIDI event observer.
//!
//! IT modules can embed MIDI macros that, when triggered via `Zxx` /
//! `SFx`, emit MIDI data. xmrsplayer's internal interpreter handles
//! the `F0 F0 nn xx` "filter control" frames natively (they drive the
//! per-voice resonant filter) but for everything else — standard
//! channel messages (Note On/Off, CC, Program Change, Pitch Bend,
//! Aftertouch) and SysEx — the engine has no internal meaning to
//! attach, so the bytes are emitted as `MidiEvent`s to any subscribed
//! [`MidiObserver`].
//!
//! This lets downstream code bridge modules to real MIDI hardware or
//! a soft synth without the core replayer having to know anything
//! about MIDI I/O. The observer pattern mirrors the [`PlayerObserver`]
//! trait that already publishes row / tick / pattern-change events.
//!
//! # Scope
//!
//! * Events are emitted once per occurrence in a macro — a single
//!   `Zxx` triggers the full macro, which may emit zero or more
//!   events depending on the macro's byte sequence.
//! * The replayer itself does not enforce MIDI timing constraints
//!   (running status, sysex aggregation across macros, etc.). Each
//!   macro is parsed independently; whatever byte sequence the macro
//!   encodes is what gets emitted.
//! * Tracker channel vs MIDI channel: `source_channel` in
//!   [`MidiObserver::on_midi_event`] is the *pattern* channel the
//!   macro was triggered on (0..num_channels). The `channel` field
//!   inside [`MidiEvent`] variants is the *MIDI* channel encoded in
//!   the status byte's low nibble (0..15). The two are independent:
//!   a macro on tracker channel 3 can address MIDI channel 7.
//!
//! [`PlayerObserver`]: crate::observer::PlayerObserver
//! [`MidiObserver`]: crate::midi_observer::MidiObserver
//! [`MidiObserver::on_midi_event`]: crate::midi_observer::MidiObserver::on_midi_event
//! [`MidiEvent`]: crate::midi_observer::MidiEvent

use alloc::vec::Vec;

/// A MIDI event emitted by the macro interpreter.
///
/// Channel messages carry the MIDI channel (0..15) and payload bytes
/// as documented in the MIDI 1.0 spec. SysEx data is passed through
/// verbatim including the leading `F0` but stopping BEFORE the
/// terminating `F7` — consumers wanting the raw stream can infer it.
/// `Raw` is a fallback for byte sequences that look like MIDI but
/// don't match a known status byte (or are too short to form a
/// complete message).
#[derive(Debug, Clone)]
pub enum MidiEvent {
    /// `8n nn vv` — Note Off
    NoteOff { channel: u8, note: u8, velocity: u8 },
    /// `9n nn vv` — Note On. Note: per MIDI convention, `Note On`
    /// with `velocity == 0` is aliased to `Note Off` by the
    /// interpreter before dispatch, so this variant always carries
    /// a non-zero velocity.
    NoteOn { channel: u8, note: u8, velocity: u8 },
    /// `An nn pp` — Polyphonic Aftertouch
    PolyAftertouch { channel: u8, note: u8, pressure: u8 },
    /// `Bn cc vv` — Control Change
    ControlChange {
        channel: u8,
        controller: u8,
        value: u8,
    },
    /// `Cn pp` — Program Change
    ProgramChange { channel: u8, program: u8 },
    /// `Dn pp` — Channel Aftertouch
    ChannelAftertouch { channel: u8, pressure: u8 },
    /// `En ll mm` — Pitch Bend (14-bit value assembled from the two
    /// data bytes, 0..16383 with 8192 = centre).
    PitchBend { channel: u8, value: u16 },
    /// `F0 ... (F7)` — System Exclusive. The leading `F0` is
    /// included in the payload; the terminating `F7` is stripped.
    /// IT-internal `F0 F0 nn xx` filter frames are consumed by the
    /// replayer and never reach this variant.
    SysEx(Vec<u8>),
    /// Fallback for byte sequences that look like MIDI data but
    /// don't parse cleanly (truncated message, unknown status).
    /// Consumers that want to forward everything can concatenate
    /// `Raw` payloads with the typed variants re-serialised.
    Raw(Vec<u8>),
}

/// A consumer of MIDI events emitted by the macro interpreter.
///
/// Register with [`XmrsPlayer::add_midi_observer`](crate::xmrsplayer::XmrsPlayer::add_midi_observer).
///
/// The `source_channel` argument is the *tracker* channel (pattern
/// column, 0..num_channels) the macro fired on — distinct from the
/// MIDI channel carried inside the event's variant fields. See the
/// module-level note on the two channel concepts.
pub trait MidiObserver {
    /// Called once per MIDI event emitted by the macro interpreter.
    /// Multiple calls may happen in quick succession if the macro
    /// encoded multiple events.
    fn on_midi_event(&mut self, event: &MidiEvent, source_channel: usize);
}
