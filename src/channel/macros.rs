//! IT MIDI macro interpreter — text expansion + byte-stream parse +
//! `$x..` substitution table.

use alloc::vec::Vec;

use xmrs::prelude::*;

use crate::midi_observer::MidiEvent;
use crate::voice_pool::VoicePool;

use super::Channel;

impl<'a> Channel<'a> {
    /// Apply a MIDI-macro effect (`SFx` / `Zxx`) to this channel.
    ///
    /// Macro strings live on `module.midi_macros` — `None` on non-IT
    /// modules or IT files without an embedded macro table, in
    /// which case this is a silent no-op.
    ///
    /// # What's interpreted
    ///
    /// IT's internal filter-control frames (`F0 F0 00/01 xx`) drive
    /// the per-voice resonant filter directly. Standard MIDI channel
    /// messages (`8n..En` status bytes) are packaged as
    /// [`MidiEvent`]s and pushed into `out_events`; the facade then
    /// drains that buffer to any subscribed [`crate::midi_observer::MidiObserver`]. SysEx
    /// (`F0 .. F7`, except the IT-internal `F0 F0` prefix) is passed
    /// through as [`MidiEvent::SysEx`].
    ///
    /// The literal byte `z` (0x7A) anywhere in a macro is replaced
    /// by the `Zxx` parameter before frame parsing.
    pub(crate) fn apply_midi_macro(
        &mut self,
        macro_type: MidiMacroType,
        ch_index: usize,
        out_events: &mut Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        match macro_type {
            MidiMacroType::SelectParametric(idx) => {
                // SFx — just stash the selector. The macro only
                // fires when a subsequent Zxx<0x80 picks it up.
                self.midi_parametric_selector = idx;
            }
            MidiMacroType::Parametric(z) => {
                let Some(macros) = &self.module.midi_macros else {
                    return;
                };
                let Some(bytes) = macros.parametric.get(self.midi_parametric_selector) else {
                    return;
                };
                self.interpret_macro_bytes(bytes.clone(), z, ch_index, out_events, pool);
            }
            MidiMacroType::Fixed { idx, z } => {
                let Some(macros) = &self.module.midi_macros else {
                    return;
                };
                let Some(bytes) = macros.fixed.get(idx) else {
                    return;
                };
                self.interpret_macro_bytes(bytes.clone(), z, ch_index, out_events, pool);
            }
        }
    }

    /// Walk the macro byte stream, substituting `z` (0x7A) with the
    /// Zxx parameter, and interpret each recognised message.
    /// `F0 F0 sub val` is consumed for internal filter control;
    /// standard MIDI channel messages and SysEx are packaged into
    /// `out_events` for external dispatch.
    fn interpret_macro_bytes(
        &mut self,
        bytes: Vec<u8>,
        z_value: u8,
        ch_index: usize,
        out_events: &mut Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        // IT macros are stored in the file as 32-byte ASCII strings
        // (ITTECH, "Internal Editing Of Macros"). Hex digits encode
        // nibbles two-per-byte and a small letter table substitutes
        // dynamic per-channel state (note, velocity, pan, etc.).
        // Raw-byte interpretation produces no MIDI output for any
        // real-world IT file because every ASCII char is a data byte
        // (< 0x80). We therefore expand the text first, then run the
        // MIDI parser on the resulting binary buffer.
        let bytes = self.expand_macro_text(&bytes, z_value, ch_index);

        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            match b {
                // IT internal filter command: `F0 F0 sub val`. The
                // `F0 F0` prefix is distinctive because real MIDI
                // SysEx is `F0 .. F7`, never `F0 F0` back-to-back.
                0xF0 if i + 1 < bytes.len() && bytes[i + 1] == 0xF0 => {
                    if i + 3 >= bytes.len() {
                        break; // incomplete frame
                    }
                    let sub = bytes[i + 2];
                    let val = bytes[i + 3];
                    match sub {
                        0x00 => {
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_cutoff_reg(val);
                            }
                        }
                        0x01 => {
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_resonance_reg(val);
                            }
                        }
                        0x02 => {
                            // Filter mode: bit 4 = HPF, bit 5 =
                            // disable. Implementation details in
                            // `StateFilter::set_mode_from_macro`.
                            if let Some(instr) = self.live_mut(pool) {
                                instr.filter.set_mode_from_macro(val);
                            }
                        }
                        // 0x03..0x7F reserved.
                        _ => {}
                    }
                    i += 4;
                }

                // 8n nn vv — Note Off (3 bytes)
                0x80..=0x8F => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::NoteOff {
                            channel: b & 0x0F,
                            note: bytes[i + 1] & 0x7F,
                            velocity: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // 9n nn vv — Note On (3 bytes). Velocity 0 collapses
                // to Note Off per MIDI convention.
                0x90..=0x9F => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    let vel = bytes[i + 2] & 0x7F;
                    let note = bytes[i + 1] & 0x7F;
                    let channel = b & 0x0F;
                    let ev = if vel == 0 {
                        MidiEvent::NoteOff {
                            channel,
                            note,
                            velocity: 0,
                        }
                    } else {
                        MidiEvent::NoteOn {
                            channel,
                            note,
                            velocity: vel,
                        }
                    };
                    out_events.push((ch_index, ev));
                    i += 3;
                }

                // An nn pp — Polyphonic Aftertouch (3 bytes)
                0xA0..=0xAF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::PolyAftertouch {
                            channel: b & 0x0F,
                            note: bytes[i + 1] & 0x7F,
                            pressure: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // Bn cc vv — Control Change (3 bytes)
                0xB0..=0xBF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ControlChange {
                            channel: b & 0x0F,
                            controller: bytes[i + 1] & 0x7F,
                            value: bytes[i + 2] & 0x7F,
                        },
                    ));
                    i += 3;
                }

                // Cn pp — Program Change (2 bytes)
                0xC0..=0xCF => {
                    if i + 1 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ProgramChange {
                            channel: b & 0x0F,
                            program: bytes[i + 1] & 0x7F,
                        },
                    ));
                    i += 2;
                }

                // Dn pp — Channel Aftertouch (2 bytes)
                0xD0..=0xDF => {
                    if i + 1 >= bytes.len() {
                        break;
                    }
                    out_events.push((
                        ch_index,
                        MidiEvent::ChannelAftertouch {
                            channel: b & 0x0F,
                            pressure: bytes[i + 1] & 0x7F,
                        },
                    ));
                    i += 2;
                }

                // En ll mm — Pitch Bend (3 bytes, 14-bit assembled)
                0xE0..=0xEF => {
                    if i + 2 >= bytes.len() {
                        break;
                    }
                    let lsb = (bytes[i + 1] & 0x7F) as u16;
                    let msb = (bytes[i + 2] & 0x7F) as u16;
                    out_events.push((
                        ch_index,
                        MidiEvent::PitchBend {
                            channel: b & 0x0F,
                            value: lsb | (msb << 7),
                        },
                    ));
                    i += 3;
                }

                // F0 .. F7 — SysEx (non-IT-internal; `F0 F0` handled
                // above). Collect bytes until F7 or end of macro.
                0xF0 => {
                    // Accumulate the SysEx including the leading F0
                    // but stopping before F7.
                    let start = i;
                    let mut end = start + 1;
                    while end < bytes.len() && bytes[end] != 0xF7 {
                        end += 1;
                    }
                    let payload = bytes[start..end].to_vec();
                    out_events.push((ch_index, MidiEvent::SysEx(payload)));
                    // Skip past F7 if present, else end-of-stream.
                    i = if end < bytes.len() { end + 1 } else { end };
                }

                // Non-status bytes (< 0x80) or unrecognised — skip
                // one byte forward. Some macros use arbitrary data
                // bytes as padding between commands.
                _ => i += 1,
            }
        }
    }

    /// Expand an IT MIDI-macro text string to a binary byte stream.
    ///
    /// Macros are stored in the IT file as 32-byte ASCII strings
    /// (NUL-padded). Schism's `csf_process_midi_macro`
    /// (`effects.c:1031-1180`) is the canonical reference. The
    /// substitution table:
    ///
    /// | Char    | Meaning                       | Form       |
    /// |---------|-------------------------------|------------|
    /// | `0`-`9` | hex nibble                    | nibble     |
    /// | `A`-`F` | hex nibble                    | nibble     |
    /// | `c`     | MIDI channel (instrument)     | nibble     |
    /// | `n`     | last triggered note           | full byte  |
    /// | `v`     | note velocity (Zxx ⇒ 1)       | full byte  |
    /// | `u`     | channel running volume        | full byte  |
    /// | `x`     | channel pan                   | full byte  |
    /// | `y`     | final channel pan             | full byte  |
    /// | `a`     | MIDI bank high (instrument)   | full byte  |
    /// | `b`     | MIDI bank low (instrument)    | full byte  |
    /// | `p`     | MIDI program (instrument)     | full byte  |
    /// | `z`     | Zxx parameter                 | full byte  |
    /// | `h`     | host (tracker) channel        | full byte  |
    /// | `m`     | sample loop direction         | full byte  |
    /// | `o`     | sample offset high byte       | full byte  |
    ///
    /// Hex nibbles pack two-per-byte (high nibble first); a
    /// substitution that's a full byte arriving while a single
    /// nibble is pending flushes the half-byte as-is (matching
    /// schism's `write_pos++` without OR-folding). Unrecognised
    /// bytes are silently dropped (also matching schism's `default:
    /// continue`).
    fn expand_macro_text(&self, bytes: &[u8], z_value: u8, ch_index: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(bytes.len());
        let mut pending_low_nibble: Option<u8> = None;

        for &b in bytes {
            // NUL terminator: schism's outer loop tests
            // `macro[read_pos]` (C-string sentinel) so any 0 byte
            // ends the macro. IT files NUL-pad each 32-byte slot,
            // and stopping at the first NUL keeps the trailing
            // padding from emitting a string of zero bytes.
            if b == 0 {
                break;
            }

            let (data, is_nibble) = match b {
                b'0'..=b'9' => (b - b'0', true),
                b'A'..=b'F' => (b - b'A' + 0x0A, true),
                b'c' => (self.macro_subst_midi_channel(), true),
                b'n' => (self.macro_subst_note(), false),
                b'v' => (self.macro_subst_velocity(), false),
                b'u' => (self.macro_subst_volume(), false),
                b'x' => (self.macro_subst_pan(), false),
                b'y' => (self.macro_subst_final_pan(), false),
                b'a' => (self.macro_subst_bank_high(), false),
                b'b' => (self.macro_subst_bank_low(), false),
                b'p' => (self.macro_subst_program(), false),
                b'z' => (z_value, false),
                b'h' => ((ch_index as u8) & 0x7F, false),
                b'm' => (self.macro_subst_loop_direction(), false),
                b'o' => (self.macro_subst_offset_high(), false),
                // Anything else (whitespace, lower-case letters not
                // listed, punctuation): skip silently like schism.
                _ => continue,
            };

            if is_nibble {
                let nib = data & 0x0F;
                if let Some(low) = pending_low_nibble {
                    // Second nibble of a pair: existing low half
                    // moves to the high position, new nibble takes
                    // the low half.
                    out.push((low << 4) | nib);
                    pending_low_nibble = None;
                } else {
                    pending_low_nibble = Some(nib);
                }
            } else {
                // Full-byte substitution. If a half-byte is pending,
                // commit it as a low-nibble byte (schism's behaviour
                // is `write_pos++` with no shift — the pending nibble
                // is already stored in `outbuffer[write_pos]`'s low
                // half from the first encounter).
                if let Some(low) = pending_low_nibble {
                    out.push(low);
                    pending_low_nibble = None;
                }
                out.push(data);
            }
        }

        // Trailing single nibble: schism flushes it the same way
        // (`if (nibble_pos == 1) write_pos++;`). Mirror it.
        if let Some(low) = pending_low_nibble {
            out.push(low);
        }

        out
    }

    // ------------------------------------------------------------------
    // MIDI-macro letter substitutions
    //
    // These mirror schism's `csf_process_midi_macro` substitution
    // dispatch (`effects.c:1061-1156`), each returning the value to
    // splice into the macro's binary expansion. Any substitution
    // that depends on state xmrsplayer doesn't currently surface
    // (final-pan post-envelope, live ping-pong direction, sticky
    // Oxx memory) returns a documented placeholder rather than
    // breaking the macro byte-stream.
    // ------------------------------------------------------------------

    /// Look up the current instrument's [`InstrMidi`] payload, if
    /// the cell's instrument index points at an `InstrumentType::
    /// Midi` variant. Returns `None` for sampled / OPL / SID /
    /// missing instruments — the caller substitutes 0 in that
    /// case, which is what schism does when `penv->midi_*` is
    /// unset.
    fn current_instr_midi(&self) -> Option<&InstrMidi> {
        let idx = self.current_track_instrument?;
        let instr = self.module.instrument.get(idx)?;
        match &instr.instr_type {
            InstrumentType::Midi(im) => Some(im),
            _ => None,
        }
    }

    /// `c` — MIDI channel for the active instrument, masked to
    /// 0..15. Schism falls back to 15 when no instrument has a
    /// MIDI-channel mask; we match that for non-Midi instruments.
    fn macro_subst_midi_channel(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| im.channel & 0x0F)
            .unwrap_or(15)
    }

    /// `n` — last-triggered note. xmrs's `Pitch::value()` is
    /// already 0-based MIDI-aligned (`Pitch::C0 = 0`,
    /// `Pitch::C5 = 60`), matching schism's `note - 1`
    /// transformation of the 1-based cell-stored byte.
    fn macro_subst_note(&self) -> u8 {
        self.current_note.map(|p| p.value()).unwrap_or(0)
    }

    /// `v` — note velocity. xmrsplayer reaches the macro
    /// interpreter only via `Zxx` / `SFx`; schism's caller passes
    /// `velocity = 0` in that path, then `CLAMP(0, 0x01, 0x7F)`
    /// promotes it to 1. Mirror that: the canonical "no
    /// explicit velocity" value is 1.
    fn macro_subst_velocity(&self) -> u8 {
        1
    }

    /// `u` — channel running volume mapped to the MIDI 1..127
    /// range. Schism does a four-register `_muldiv` with a known-
    /// approximate result (the comment at `effects.c:1091` flags it
    /// as not-quite-right); using the channel's already-resolved
    /// normalised volume produces the same musically-meaningful
    /// value for any correctly-authored macro.
    fn macro_subst_volume(&self) -> u8 {
        // Q-format integer conversion: Volume Q1.15 raw `v_q` ∈
        // `[0, 32767]` → `(v_q × 127) >> 15`, clamp `[1, 127]`.
        let v_q = self.volume.as_q15_i16().max(0) as u32;
        let v = ((v_q * 127) >> 15) as u8;
        v.clamp(1, 127)
    }

    /// `x` — channel pan, saturated at 127. xmrs stores pan as
    /// 0..1 normalised; schism stores it 0..256 with a `MIN(_, 127)`
    /// at substitution time. The musical mapping is the same once
    /// the saturation point lands at "fully right".
    fn macro_subst_pan(&self) -> u8 {
        // Q-format integer conversion: Panning Q1.15 raw `p_q` ∈
        // `[0, 32767]` → `(p_q × 127) >> 15`, clamp `[0, 127]`.
        let p_q = self.panning.as_q15_i16().max(0) as u32;
        let p = ((p_q * 127) >> 15) as u8;
        p.min(127)
    }

    /// `y` — "final" channel pan after pan envelope and pan-swing
    /// modulation. xmrsplayer's mixer keeps separate left/right
    /// gains in `actual_volume[0..2]` rather than a single post-
    /// envelope pan, so reconstructing the MIDI value would require
    /// a lossy inverse. Approximated by the static channel pan;
    /// macros relying on tight tracking of `y` will see the pre-
    /// envelope value.
    fn macro_subst_final_pan(&self) -> u8 {
        self.macro_subst_pan()
    }

    /// `a` — MIDI bank high byte: `(bank >> 7) & 0x7F`.
    fn macro_subst_bank_high(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| ((im.bank >> 7) & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `b` — MIDI bank low byte: `bank & 0x7F`.
    fn macro_subst_bank_low(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| (im.bank & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `p` — MIDI program, masked to 7 bits.
    fn macro_subst_program(&self) -> u8 {
        self.current_instr_midi()
            .map(|im| (im.program & 0x7F) as u8)
            .unwrap_or(0)
    }

    /// `m` — sample-loop direction (`1` if currently playing
    /// reverse on a ping-pong loop, `0` otherwise). xmrsplayer's
    /// `Channel` does not surface live voice loop state at the
    /// macro-interpreter call site — the live voice's playback
    /// direction lives inside `StateSample` behind a `pool` look-
    /// up that would require mutable borrowing during what is
    /// otherwise a read-only expansion pass. Returning 0 keeps the
    /// macro byte-stream well-formed; macros using `m` are an
    /// OpenMPT-era extension that real-world IT files almost never
    /// exercise.
    fn macro_subst_loop_direction(&self) -> u8 {
        0
    }

    /// `o` — sample offset high byte. Schism reads this from the
    /// channel's `mem_offset` register, which is the cumulative
    /// Oxx + SAx high-nibble memorised across rows (used by
    /// "ZxxSecrets.it" and similar OpenMPT test cases). xmrsplayer
    /// resolves Oxx inline when it fires and does not retain a
    /// sticky high-nibble register, so we substitute 0.
    fn macro_subst_offset_high(&self) -> u8 {
        0
    }
}
