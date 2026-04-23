//! A built-in MIDI observer that prints events to stdout — the MIDI-
//! side counterpart of [`crate::debug_observer::DebugObserver`].
//!
//! Gated on the `std` feature because it uses `println!`. On `no_std`
//! targets write your own observer that routes through whatever log
//! facility is available on your platform.

use crate::midi_observer::{MidiEvent, MidiObserver};

/// Prints every MIDI event emitted by the macro interpreter to
/// stdout, prefixed with the tracker channel that produced it.
///
/// Register with [`crate::xmrsplayer::XmrsPlayer::add_midi_observer`]:
///
/// ```ignore
/// use xmrsplayer::prelude::*;
/// use xmrsplayer::debug_midi_observer::DebugMidiObserver;
///
/// let mut player = XmrsPlayer::new(&module, 48_000.0, 0);
/// player.add_midi_observer(Box::new(DebugMidiObserver::new()));
/// ```
///
/// The output format is one line per event:
///
/// ```text
/// ch03 NoteOn        midi_ch=0 note=60 vel=100
/// ch05 ControlChange midi_ch=0 cc=74 val=64
/// ch07 SysEx         len=6 data=[F0, 7E, 7F, 09, 01, ...]
/// ```
///
/// where `chNN` is the tracker channel (not the MIDI channel).
#[derive(Default)]
pub struct DebugMidiObserver {
    /// When `true`, truncate SysEx payloads to 8 bytes in the log.
    /// Off by default — full payloads print. Set to `true` for
    /// modules that bombard the observer with long SysEx strings.
    pub truncate_sysex: bool,
}

impl DebugMidiObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Truncate SysEx payloads to 8 bytes in the log. Off by default.
    pub fn with_truncate_sysex(mut self, on: bool) -> Self {
        self.truncate_sysex = on;
        self
    }
}

impl MidiObserver for DebugMidiObserver {
    fn on_midi_event(&mut self, event: &MidiEvent, source_channel: usize) {
        match event {
            MidiEvent::NoteOff {
                channel,
                note,
                velocity,
            } => println!(
                "ch{:02} NoteOff       midi_ch={} note={} vel={}",
                source_channel, channel, note, velocity
            ),
            MidiEvent::NoteOn {
                channel,
                note,
                velocity,
            } => println!(
                "ch{:02} NoteOn        midi_ch={} note={} vel={}",
                source_channel, channel, note, velocity
            ),
            MidiEvent::PolyAftertouch {
                channel,
                note,
                pressure,
            } => println!(
                "ch{:02} PolyAfterTouch midi_ch={} note={} pressure={}",
                source_channel, channel, note, pressure
            ),
            MidiEvent::ControlChange {
                channel,
                controller,
                value,
            } => println!(
                "ch{:02} ControlChange midi_ch={} cc={} val={}",
                source_channel, channel, controller, value
            ),
            MidiEvent::ProgramChange { channel, program } => println!(
                "ch{:02} ProgramChange midi_ch={} program={}",
                source_channel, channel, program
            ),
            MidiEvent::ChannelAftertouch { channel, pressure } => println!(
                "ch{:02} ChAfterTouch   midi_ch={} pressure={}",
                source_channel, channel, pressure
            ),
            MidiEvent::PitchBend { channel, value } => println!(
                "ch{:02} PitchBend     midi_ch={} value={} (centre=8192)",
                source_channel, channel, value
            ),
            MidiEvent::SysEx(data) => {
                if self.truncate_sysex && data.len() > 8 {
                    let head: alloc::vec::Vec<_> =
                        data.iter().take(8).map(|b| format!("{:02X}", b)).collect();
                    println!(
                        "ch{:02} SysEx         len={} data=[{}, ...]",
                        source_channel,
                        data.len(),
                        head.join(", ")
                    );
                } else {
                    let hex: alloc::vec::Vec<_> =
                        data.iter().map(|b| format!("{:02X}", b)).collect();
                    println!(
                        "ch{:02} SysEx         len={} data=[{}]",
                        source_channel,
                        data.len(),
                        hex.join(", ")
                    );
                }
            }
            MidiEvent::Raw(bytes) => {
                let hex: alloc::vec::Vec<_> = bytes.iter().map(|b| format!("{:02X}", b)).collect();
                println!(
                    "ch{:02} Raw           len={} data=[{}]",
                    source_channel,
                    bytes.len(),
                    hex.join(", ")
                );
            }
        }
    }
}
