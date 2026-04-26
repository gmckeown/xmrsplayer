// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;
use xmrs::waveform::WaveformState;

use crate::effect::*;
use crate::effect_arpeggio::EffectArpeggio;
use crate::effect_vibrato_tremolo::EffectVibratoTremolo;
use crate::triggerkeep::*;

use crate::helper::*;
use crate::midi_observer::MidiEvent;
use crate::state_instr_default::StateInstrDefault;
use crate::voice::Voice;
use crate::voice_pool::{VoiceId, VoicePool};
use xmrs::prelude::*;

/// Simplified DCT tag, extracted once from the nested
/// [`DuplicateCheckType`] enum so the per-voice match loop can test
/// a plain three-way variant without re-pattern-matching on every
/// iteration.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dct {
    Note,
    Sample,
    Instrument,
}

#[derive(Clone, PartialEq, Default)]
struct NoteRetrigState {
    note: CellNote,
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
    /// The last Pitch enum value triggered on this channel. Parallels
    /// `note` (which is a float that bakes in finetune); this one
    /// keeps the clean integer-semitone identity used by DCT::Note.
    /// `None` until the channel has seen any valid note trigger.
    current_note: Option<Pitch>,

    /// IT MIDI-macro selector: which parametric macro slot (0..15)
    /// is "active" on this channel. Modified by `SFx`; read by
    /// `Zxx` with `xx < 0x80`. Defaults to 0 (the `SF0` macro,
    /// typically the default filter-cutoff macro in IT files).
    midi_parametric_selector: usize,

    /// S91/S90 surround flag. When set, the right channel output
    /// is phase-inverted — the classic IT pseudo-stereo trick that
    /// folds to silence in mono and sounds spatially wide in stereo.
    /// Persists until explicitly toggled; not reset by note
    /// triggers.
    surround: bool,

    pub current: TrackUnit,

    period: f32,

    channel_volume: f32,
    volume: f32,  /* Ideally between 0 (muted) and 1 (loudest) */
    panning: f32, /* Between 0 (left) and 1 (right); 0.5 is centered */

    // Instrument
    /// 1.4 — live voice in the shared pool.
    live: Option<VoiceId>,
    /// Cached midi_mute_computer; refreshed at trigger to keep
    /// `is_muted` pool-free.
    instr_midi_mute: bool,

    effect_arpeggio: EffectArpeggio,
    effect_note_retrig_backup: NoteRetrigState,
    effect_note_retrig_counter: usize,
    effect_panbrello: EffectVibratoTremolo,
    effect_tone_portamento_goal: f32,
    effect_tremolo: EffectVibratoTremolo,
    effect_tremor: bool,
    effect_tremor_on: usize,
    effect_tremor_off: usize,
    /// S3M/ST3 tremor state machine (separate from the formula-based
    /// FT2/XM path above): number of ticks remaining before the
    /// on/off state toggles. Negative value = inactive (no Ixy has
    /// run yet on this channel since the last reset). Persists
    /// across rows so consecutive Ixy lines form a continuous cycle
    /// instead of restarting the phase at each row.
    effect_tremor_counter_s3m: i32,
    /// Paired with `effect_tremor_counter_s3m`: `true` when the
    /// current tremor slot is the *silent* half of the cycle (ST3
    /// `atreon == false`). Drives `effect_tremor` for the volume
    /// application path.
    effect_tremor_silent_s3m: bool,
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

    /// Index of this channel inside the `Voices` list. Set once at
    /// construction time (via [`Self::set_track_index`], called by
    /// `Voices::new`) and never modified afterwards. Used today to
    /// tag voices spawned by NNA with their originating track for
    /// the upcoming voice-pool migration (1.3) — see
    /// [`Voice::master_track`]. Ghosts spawned before
    /// `set_track_index` runs would carry index 0, which is correct
    /// for channel 0 and benign for the others (the field is purely
    /// informational at this stage).
    track_index: usize,

    pub muted: bool,

    actual_volume: [f32; 2],

    // --- IT New Note Action / past-note state ---
    //
    // Ghost voices: notes that were playing when a new note fired on
    // the same pattern channel and whose instrument's `NewNoteAction`
    // was not `Cut`. They continue to sound (via their own envelope +
    // fadeout) alongside the live note until they go silent.
    //
    // Voices themselves live in the `VoicePool` carried by the
    // owning `Voices`; this list holds opaque `VoiceId` handles into
    // that pool. The pool indirection (1.3) is the prerequisite for
    // step 1.5's voice-stealing — at that point the pool will reject
    // an allocation only after rejecting it against every voice in
    // every channel, lifting today's per-channel `MAX_GHOST_VOICES_
    // PER_CHANNEL` cap. Today the per-channel cap still bites first.
    //
    // No-op on MOD/XM/S3M because those importers leave NNA at its
    // `NoteCut` default — `spawn_ghost_*` short-circuits and this
    // list stays empty.
    ghosts: Vec<VoiceId>,
    /// S7x (S73–S76) lets the *pattern* override an instrument's
    /// stored NNA for the current channel, affecting only subsequent
    /// triggers on this channel. `None` = use the incoming
    /// instrument's own NNA value, which is the normal case.
    nna_override: Option<NewNoteAction>,
    /// PRNG for IT humanisation (random volume / pan variation).
    /// Seeded deterministically per-channel by `Voices::new` so the
    /// same module always produces the same WAV — IT humanisation
    /// is meant to de-robot static samples, not to introduce non-
    /// determinism into the render pipeline. Updated at every
    /// note-trigger that consults `random_*_variation`.
    rng: xmrs::xorshift::XorShift32,
}

impl<'a> Channel<'a> {
    /// Build a channel. Format-specific quirks (FT2 arpeggio LUT,
    /// FT2 arpeggio period clamp) are driven by `module.profile.format` via
    /// `EffectArpeggio` and `PeriodHelper`; Channel itself no longer
    /// carries an `Ft2Quirks` field, so there is only one source of
    /// truth for "is this an XM?".
    ///
    /// `tempo` is the initial song speed, forwarded to the arpeggio
    /// effect so its FT2 LUT is usable on the very first row. The
    /// player must subsequently call [`Self::set_tempo`] whenever the
    /// song tempo changes (Fxx effect) to keep the LUT index in sync.
    pub(crate) fn new(module: &'a Module, rate: f32, tempo: usize) -> Self {
        let period_helper = PeriodHelper::new(
            module.frequency_type,
            module.profile.quirks.ft2_arpeggio_note_clamp,
        );
        Self {
            module,
            period_helper: period_helper.clone(),
            rate,
            channel_volume: 1.0,
            volume: 1.0,
            panning: 0.5,
            note: 0.0,
            current_note: None,
            midi_parametric_selector: 0,
            surround: false,
            current: TrackUnit::default(),
            period: 0.0,
            live: None,
            instr_midi_mute: false,
            effect_arpeggio: EffectArpeggio::new(module.profile.quirks.ft2_arpeggio_lut, tempo),
            effect_note_retrig_backup: NoteRetrigState::default(),
            effect_note_retrig_counter: 0,
            effect_panbrello: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremolo: EffectVibratoTremolo::new(Waveform::Sine),
            effect_tremor: false,
            effect_tremor_on: 0,
            effect_tremor_off: 0,
            // ST3 starts with `atreon = false` ("in the silent half
            // of the cycle") and `atremor = 0`. On the first Ixy
            // tick the `atremor == 0` check triggers the toggle,
            // which flips `atreon` to `true` (playing) and reloads
            // the counter with `on_time`. Mirroring that here: start
            // with `silent_s3m = true` so the first toggle flips us
            // to "playing"; the counter sentinel `-1` acts like ST3's
            // initial `atremor = 0` (not > 0, so toggle branch).
            effect_tremor_counter_s3m: -1,
            effect_tremor_silent_s3m: true,
            effect_vibrato: EffectVibratoTremolo::new(Waveform::Sine),
            vibrato_retrig_on_new_note: true,
            tremolo_retrig_on_new_note: true,
            effect_tone_portamento_goal: 0.0,
            effect_semitone: false,
            effect_note_delay: 0,
            muted: false,
            track_index: 0,
            actual_volume: [0.0, 0.0],
            ghosts: Vec::new(),
            nna_override: None,
            // `XorShift32::default()` uses the type's canonical
            // non-zero seed (4294967291). `Voices::new` re-seeds
            // each channel with a per-channel-unique value after
            // construction so the streams stay independent.
            rng: xmrs::xorshift::XorShift32::default(),
        }
    }

    /// Signed uniform random in `[-1.0, 1.0]`. Wraps
    /// `XorShift32::next_f32` (which is `[0, 1]`) and centres.
    #[inline]
    fn rng_bipolar(&mut self) -> f32 {
        self.rng.next_f32() * 2.0 - 1.0
    }

    /// Set the channel's resting panning before any pattern row has
    /// been processed. Used by [`Voices::new`] to apply S3M's
    /// historical L/R alternance default (channels 0/2/4/... soft-
    /// left, channels 1/3/5/... soft-right). For other formats the
    /// channel default stays at centre.
    #[inline(always)]
    pub(crate) fn set_initial_panning(&mut self, panning: f32) {
        self.panning = panning.clamp(0.0, 1.0);
    }

    /// Re-seed the channel's PRNG. Called by `Voices::new` with a
    /// per-channel value so each channel gets an independent (but
    /// deterministic) humanisation stream. Zero is rejected by
    /// `XorShift32::new` — the caller is responsible for passing
    /// a non-zero seed.
    pub(crate) fn reseed_rng(&mut self, seed: u32) {
        let seed = if seed == 0 { 0xDEADBEEF } else { seed };
        self.rng = xmrs::xorshift::XorShift32::new(Some(seed));
    }

    /// Record this channel's index inside the `Voices` list. Called
    /// by `Voices::new` immediately after construction. The index
    /// flows into voices spawned by NNA so the eventual voice pool
    /// (step 1.3 of the roadmap) can route past-note effects and
    /// DCT lookups via a single shared list.
    pub(crate) fn set_track_index(&mut self, idx: usize) {
        self.track_index = idx;
    }

    // --- Live-voice accessors (1.4) -------------------------------
    fn live<'p>(&self, pool: &'p VoicePool<'a>) -> Option<&'p StateInstrDefault<'a>> {
        self.live.and_then(|id| pool.get(id)).map(|v| &v.instr)
    }
    fn live_mut<'p>(&self, pool: &'p mut VoicePool<'a>) -> Option<&'p mut StateInstrDefault<'a>> {
        self.live
            .and_then(|id| pool.get_mut(id))
            .map(|v| &mut v.instr)
    }
    fn drop_live(&mut self, pool: &mut VoicePool<'a>) {
        if let Some(id) = self.live.take() {
            pool.release(id);
        }
    }
    /// Move the live voice into the channel's ghost list without
    /// reallocating in the pool — the slot just changes role from
    /// "live" to "detached". Returns the moved `VoiceId` so the
    /// caller can apply NNA. The ghost list grows unboundedly per
    /// channel (1.5+); the shared pool's capacity is the only cap.
    fn promote_live_to_ghost(&mut self, _pool: &mut VoicePool<'a>) -> Option<VoiceId> {
        let id = self.live.take()?;
        self.ghosts.push(id);
        Some(id)
    }
    /// Allocate a fresh live voice in the pool. Releases any
    /// previously-held live id (the caller should already have
    /// promoted it to a ghost if NNA != Cut). Caches the
    /// instrument's midi_mute_computer so `is_muted` stays
    /// pool-free.
    fn install_live(
        &mut self,
        pool: &mut VoicePool<'a>,
        state: StateInstrDefault<'a>,
        midi_mute: bool,
    ) {
        if let Some(old) = self.live.take() {
            pool.release(old);
        }
        let voice = Voice::new_live(state, self.track_index);
        self.live = Some(pool.allocate_live(voice));
        self.instr_midi_mute = midi_mute;
    }

    /// Forward a tempo change to the arpeggio effect. Called by the
    /// player on every step so the FT2 arpeggio LUT is indexed from
    /// the current speed rather than a stale copy.
    pub(crate) fn set_tempo(&mut self, tempo: usize) {
        self.effect_arpeggio.set_tempo(tempo);
    }

    pub fn is_muted(&self) -> bool {
        self.muted || self.instr_midi_mute
    }

    fn cut_pitch(&mut self) {
        /* NB: this is not the same as Key Off */
        self.volume = 0.0;
    }

    /// Release the currently-sustaining note.
    ///
    /// Behaviour depends on the module's canonical format (`module.profile.format`),
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
    fn key_off(&mut self, _tick: usize, pool: &mut VoicePool<'a>) {
        let Some(i) = self.live_mut(pool) else {
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
        if self.module.profile.quirks.keyoff_cuts_without_vol_env && !had_vol_env {
            self.cut_pitch();
        }
    }

    /// IT-only "note fade" (`~~~` in the pattern, raw byte 246).
    /// Distinct from `key_off`: a fade starts the volume-fadeout
    /// counter without entering the envelope's release segment, so
    /// envelopes keep ticking from where they were and only
    /// `volume_fadeout` decays. Schism (`effects.c:1508`) treats
    /// any unknown / out-of-range note byte as fade too, which is
    /// why the IT pattern decoder folds 120..=252 here.
    ///
    /// Implementation note: this reuses `instr.key_off()` because
    /// the player's `key_off` already does the right thing for
    /// "fade behaviour" — clears `sustained`, which is what makes
    /// `volume_fadeout` tick down. The release segment of the
    /// volume envelope, which `key_off` also enters, is benign for
    /// instruments without an explicit release branch (it just
    /// continues from the current point); for instruments with one,
    /// it produces an audible difference vs schism that would need
    /// a finer-grained API to fix. Acceptable rendering compromise
    /// — IT NOTE_FADE is used overwhelmingly to fade out tails on
    /// instruments without dedicated release shapes.
    fn note_fade(&mut self, pool: &mut VoicePool<'a>) {
        let Some(i) = self.live_mut(pool) else {
            self.cut_pitch();
            return;
        };
        i.key_off();
    }

    /// Drop every ghost voice on this channel. Used by the facade
    /// when the song is seeked (goto) — stale ghosts from the
    /// previous playback position must not bleed into the new one.
    pub(crate) fn clear_ghosts(&mut self, pool: &mut VoicePool<'a>) {
        for id in self.ghosts.drain(..) {
            pool.release(id);
        }
        self.nna_override = None;
    }

    // ------------------------------------------------------------------
    // IT MIDI-macro dispatch
    // ------------------------------------------------------------------

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
    /// drains that buffer to any subscribed [`MidiObserver`]. SysEx
    /// (`F0 .. F7`, except the IT-internal `F0 F0` prefix) is passed
    /// through as [`MidiEvent::SysEx`].
    ///
    /// The literal byte `z` (0x7A) anywhere in a macro is replaced
    /// by the `Zxx` parameter before frame parsing.
    pub(crate) fn apply_midi_macro(
        &mut self,
        macro_type: MidiMacroType,
        ch_index: usize,
        out_events: &mut alloc::vec::Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        match macro_type {
            MidiMacroType::SelectParametric(idx) => {
                // SFx — just stash the selector. The macro only
                // fires when a subsequent Zxx<0x80 picks it up.
                self.midi_parametric_selector = idx;
                return;
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
        mut bytes: alloc::vec::Vec<u8>,
        z_value: u8,
        ch_index: usize,
        out_events: &mut alloc::vec::Vec<(usize, MidiEvent)>,
        pool: &mut VoicePool<'a>,
    ) {
        // In-place `z` substitution — the macro byte 0x7A is the
        // ASCII letter `z`, which Schism Tracker / OpenMPT / IT use
        // as a parameter placeholder inside a macro string.
        for b in bytes.iter_mut() {
            if *b == b'z' {
                *b = z_value;
            }
        }

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

    // ------------------------------------------------------------------
    // IT New Note Action helpers
    // ------------------------------------------------------------------

    /// Resolve which NNA to apply when a new note displaces the
    /// currently-held voice. `S73`–`S76` override via
    /// `self.nna_override`; otherwise the NNA comes from the
    /// *outgoing* instrument (not the incoming one — IT's replay
    /// reads the old voice's NNA when deciding what happens to it).
    /// Returns `NoteCut` if no voice is currently held (nothing to
    /// ghost).
    fn effective_nna(&self, pool: &VoicePool<'a>) -> NewNoteAction {
        if let Some(nna) = self.nna_override {
            return nna;
        }
        let Some(instr) = self.live(pool) else {
            return NewNoteAction::NoteCut;
        };
        match &instr.behavior.duplicate_check {
            DuplicateCheckType::Off(nna) => *nna,
            DuplicateCheckType::Note(dca)
            | DuplicateCheckType::Sample(dca)
            | DuplicateCheckType::Instrument(dca) => match dca {
                DuplicateCheckAction::NoteCut(nna)
                | DuplicateCheckAction::NoteOff(nna)
                | DuplicateCheckAction::NoteFadeOut(nna) => *nna,
            },
        }
    }

    /// Handle the NNA dispatch for a note that's about to be
    /// displaced. Called from the replace-instrument path just
    /// before `self.instr` is overwritten with a fresh voice.
    ///
    /// The fork snapshots the channel-level gain / pan at the
    /// moment of displacement so subsequent tremolo / slides on the
    /// live channel don't perturb the ghost.
    fn spawn_ghost_for_outgoing(&mut self, pool: &mut VoicePool<'a>) {
        let nna = self.effective_nna(pool);
        if matches!(nna, NewNoteAction::NoteCut) {
            // Legacy / XM / MOD / S3M path: the fresh note cuts the
            // old one (today's behaviour). No ghost.
            return;
        }
        // Live-voice → ghost promotion. The voice itself stays in
        // its pool slot; only the channel-side bookkeeping changes
        // (live → ghosts list). Before parking, snapshot the
        // channel's current gain / pan / note / period into the
        // voice's frozen-* fields, since the channel won't update
        // them after detachment.
        let Some(id) = self.promote_live_to_ghost(pool) else {
            return;
        };
        if let Some(ghost) = pool.get_mut(id) {
            ghost.vol_frozen = self.volume;
            ghost.channel_volume_frozen = self.channel_volume;
            ghost.panning_frozen = self.panning;
            ghost.note = self.current_note;
            ghost.sample_num = ghost.instr.current_sample_num;
            ghost.period_at_fork = self.period;
            match nna {
                NewNoteAction::Continue => ghost.apply_nna_continue(),
                NewNoteAction::NoteOff => ghost.apply_nna_note_off(),
                NewNoteAction::NoteFadeOut => ghost.apply_nna_note_fade_out(),
                NewNoteAction::NoteCut => unreachable!(), // handled above
            }
        }
    }

    /// Ghost-note variant: clone the live voice into a ghost
    /// WITHOUT taking ownership, because `self.instr` is about to
    /// be retriggered in place (note column carries a fresh pitch
    /// with no accompanying instrument column, so the same
    /// `StateInstrDefault` is reused).
    ///
    /// Called from `tick0_load_pitch` on the note-only path.
    /// Default NNA = Cut → no-op (matches XM/MOD/S3M ghost-note
    /// semantics: retrigger cuts the old sound).
    fn spawn_ghost_clone(&mut self, pool: &mut VoicePool<'a>) {
        let nna = self.effective_nna(pool);
        if matches!(nna, NewNoteAction::NoteCut) {
            return;
        }
        // Read the live state via the pool, clone it, drop the
        // borrow, then allocate a fresh ghost holding the clone.
        // Two-step pattern because `pool.allocate` re-borrows the
        // pool mutably and would conflict with the `pool.get`.
        let (cloned, sample_num) = {
            let Some(live) = self.live(pool) else {
                return;
            };
            (live.clone(), live.current_sample_num)
        };
        let mut ghost = Voice::new_ghost(
            cloned,
            self.track_index,
            self.volume,
            self.channel_volume,
            self.panning,
            self.current_note,
            sample_num,
            self.period,
        );
        match nna {
            NewNoteAction::Continue => ghost.apply_nna_continue(),
            NewNoteAction::NoteOff => ghost.apply_nna_note_off(),
            NewNoteAction::NoteFadeOut => ghost.apply_nna_note_fade_out(),
            NewNoteAction::NoteCut => unreachable!(),
        }
        self.push_ghost(pool, ghost);
    }

    /// Add a voice to the channel's ghost list. Goes through the
    /// pool's `allocate_ghost` which can refuse the allocation when
    /// every existing voice is sustained-and-audible — in that
    /// case we drop the new ghost on the floor (it would just have
    /// been unaudible noise added to an already-saturated mix), and
    /// the channel keeps playing whatever live note triggered the
    /// NNA. Mirrors schism's `csf_get_nna_channel` returning 0:
    /// the NNA spawn is silently abandoned.
    fn push_ghost(&mut self, pool: &mut VoicePool<'a>, v: Voice<'a>) {
        if let Some(id) = pool.allocate_ghost(v) {
            self.ghosts.push(id);
        }
    }

    /// Advance every ghost's envelopes / fadeout by one tick, and
    /// drop any that have decayed to silence. Voices that go silent
    /// are released back to the pool here, freeing their slot for
    /// future forks.
    fn tick_ghosts(&mut self, pool: &mut VoicePool<'a>) {
        // First pass: tick every live handle. Stale handles (from
        // double-release races, which 1.3 shouldn't produce but the
        // pool tolerates) are silently skipped via `get_mut`.
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.tick();
            }
        }
        // Second pass: drop dead voices from the channel's list and
        // release them in the pool. Done as a `retain` over the
        // local list so the order of survivors is preserved.
        self.ghosts.retain(|id| {
            let alive = pool.get(*id).map_or(false, |v| v.is_alive());
            if !alive {
                pool.release(*id);
            }
            alive
        });
    }

    /// S70 / S71 / S72: apply a past-note effect to every ghost on
    /// this channel. The live note is unaffected (S7x targets only
    /// detached voices, per ITTECH).
    fn past_note_cut_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_cut();
            }
        }
    }
    fn past_note_off_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_off();
            }
        }
    }
    fn past_note_fade_all(&mut self, pool: &mut VoicePool<'a>) {
        for id in &self.ghosts {
            if let Some(v) = pool.get_mut(*id) {
                v.past_fade_out();
            }
        }
    }

    /// Apply the Duplicate Check Type / Action pair from the
    /// *incoming* instrument's header to existing voices on this
    /// channel (live + ghosts). Called at trigger time, **before**
    /// the NNA ghost-spawn path — so a DCA that cuts / off / fades
    /// a duplicate runs first, and NNA then makes its own decision
    /// about the (now potentially already-affected) outgoing voice.
    ///
    /// The new-note identity tuple `(note, new_instrument_index,
    /// new_sample_index)` is taken from the pattern cell being
    /// processed; it is what DCT tests existing voices against.
    fn apply_dct(
        &mut self,
        pool: &mut VoicePool<'a>,
        new_instrument_index: usize,
        new_note: Option<Pitch>,
        new_sample_index: Option<usize>,
    ) {
        // Resolve the incoming instrument's DCT/DCA pair. `Off` is
        // the early-exit (most common) case — no duplicate check.
        let Some(incoming) = self.module.instrument.get(new_instrument_index) else {
            return;
        };
        let InstrumentType::Default(id) = &incoming.instr_type else {
            return;
        };
        let (dct, dca) = match &id.behavior.duplicate_check {
            DuplicateCheckType::Off(_) => return,
            DuplicateCheckType::Note(dca) => (Dct::Note, dca.clone()),
            DuplicateCheckType::Sample(dca) => (Dct::Sample, dca.clone()),
            DuplicateCheckType::Instrument(dca) => (Dct::Instrument, dca.clone()),
        };

        // --- Live voice check. ---
        // Applying DCA to the live voice is equivalent to the usual
        // note-cut / key-off / fade semantics — just triggered by
        // DCT match rather than by effect column.
        let live_matches = self.live(pool).is_some_and(|live| match dct {
            // DCT::Note matches when BOTH the note identity and the
            // instrument match. Schism (`effects.c:1729`) writes:
            //   apply_dna = (NOTE_IS_NOTE(note)
            //                && (int) p->note == note
            //                && ptr_instrument == p->ptr_instrument);
            // — the instrument check is essential, otherwise two
            // unrelated instruments that happened to play the same
            // note would clobber each other's voices on every
            // retrigger. Pre-1.5 xmrs missed the instrument check
            // entirely and was over-aggressive on this DCT axis.
            //
            // The note compared is the *input* note (pattern column),
            // not a remapped output — drum kits compare on input,
            // see bug 6 fix.
            Dct::Note => {
                live.num == new_instrument_index
                    && match (self.current_note, new_note) {
                        (Some(a), Some(b)) => a == b,
                        _ => false,
                    }
            }
            Dct::Sample => {
                live.num == new_instrument_index
                    && live.current_sample_num == new_sample_index
                    && new_sample_index.is_some()
            }
            Dct::Instrument => live.num == new_instrument_index,
        });
        if live_matches {
            match &dca {
                DuplicateCheckAction::NoteCut(_) => self.cut_pitch(),
                DuplicateCheckAction::NoteOff(_) => {
                    if let Some(i) = self.live_mut(pool) {
                        i.key_off();
                    }
                }
                DuplicateCheckAction::NoteFadeOut(_) => {
                    if let Some(i) = self.live_mut(pool) {
                        i.key_off();
                    }
                }
            }
        }

        // --- Ghost voices. ---
        for id in &self.ghosts {
            let Some(ghost) = pool.get_mut(*id) else {
                continue;
            };
            let matches = match dct {
                Dct::Note => {
                    // Same instrument-AND-note rule as for the live
                    // voice. See the comment on the live arm above.
                    ghost.instr.num == new_instrument_index
                        && match (ghost.note, new_note) {
                            (Some(a), Some(b)) => a == b,
                            _ => false,
                        }
                }
                Dct::Sample => {
                    ghost.instr.num == new_instrument_index
                        && ghost.sample_num == new_sample_index
                        && new_sample_index.is_some()
                }
                Dct::Instrument => ghost.instr.num == new_instrument_index,
            };
            if matches {
                match &dca {
                    DuplicateCheckAction::NoteCut(_) => ghost.past_cut(),
                    DuplicateCheckAction::NoteOff(_) => ghost.past_off(),
                    DuplicateCheckAction::NoteFadeOut(_) => ghost.past_fade_out(),
                }
            }
        }
    }

    pub(crate) fn trigger_pitch(&mut self, flags: TriggerKeep, pool: &mut VoicePool<'a>) {
        // A new note always starts un-muted: in both FT2 (resets
        // `realVol` from the instrument) and ST3 (`avol = aorgvol`
        // on trigger), any tremor mute from previous rows is
        // overridden by the fresh note's volume.
        //
        // For `tremor_state_persists` modules (ST3), the internal
        // counter state (`effect_tremor_counter_s3m` + `_silent_s3m`)
        // is intentionally NOT reset here — ST3's `atremor` / `atreon`
        // live across note triggers, so the next tick where Ixy runs
        // resumes the cycle from where it left off. Only the
        // immediate volume gate (`effect_tremor`) is cleared.
        self.effect_tremor = false;

        if let Some(instr) = self.live_mut(pool) {
            if !contains(flags, TRIGGER_KEEP_SAMPLE_POSITION) {
                instr.sample_reset();
            }

            if !contains(flags, TRIGGER_KEEP_ENVELOPE) {
                instr.envelopes_reset();
            }

            instr.vibrato_reset();

            if !contains(flags, TRIGGER_KEEP_VOLUME) {
                instr.volume_reset();
                // IT's volume cascade at note-trigger time:
                //   voice_start = Sample.volume * default_note_volume
                //                 (i.e., GvL * Vol in IT terms)
                // A V-column override later in effect processing
                // replaces `self.volume` wholesale, at which point
                // Vol is cleanly gone but GvL still scales the sample
                // on every sample-mixer tick via `StateInstrDefault`.
                // For non-IT formats, default_note_volume is 1.0 so
                // this folds to the previous behaviour.
                let dnv = instr.current_sample_default_note_volume();
                self.volume = (instr.volume * dnv).clamp(0.0, 1.0);
            }

            // Panning reset: IT is selective (only when the cell
            // carries an instrument column — a ghost-note retrigger
            // keeps the channel's current pan so in-flight Pxx
            // slides / panning envelope persist). S3M panning is
            // channel state, not note state — schism's `effects.c:
            // 249-253` only writes the instrument's pan when the
            // sample/instrument carries an explicit `CHN_PANNING`
            // flag. Mirror that: a plain note retrigger on S3M
            // must leave the channel's pan alone so the header-
            // derived L/R alternance survives.
            //
            // Pitch-pan separation goes inside the same branch: it's
            // a per-note offset relative to the instrument's stored
            // pan, so it makes sense only when we just landed on
            // that stored pan. A ghost retrigger inherits whatever
            // pps the previous note baked in.
            let do_pan_reset = if self.module.profile.format.is_s3m() {
                false
            } else if self.module.profile.format.is_it() {
                self.current.instrument.is_some()
            } else {
                true
            };
            if do_pan_reset {
                self.panning = instr.panning;

                let pps = instr.pitch_pan_separation();
                if pps != 0.0 {
                    let note_i = self.note as i32;
                    let ppc = instr.pitch_pan_center_semitones();
                    let shift = pps * ((note_i - ppc) as f32) / 60.0;
                    self.panning = (self.panning + shift).clamp(0.0, 1.0);
                }
            }

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

        // IT humanisation (random_volume_variation /
        // random_pan_variation). Applied AFTER the main instr block
        // closes so we can freely use `&mut self` for the PRNG
        // without the borrow checker fighting the `instr` read. Done
        // only on fresh triggers (not when `TRIGGER_KEEP_VOLUME` is
        // set — ghost-note retrigs and tone-portamento should not
        // re-roll). Non-IT formats have both variations at zero, so
        // this path is a measured no-op there.
        if !contains(flags, TRIGGER_KEEP_VOLUME) {
            let (rvv, rpv) = match self.live(pool) {
                Some(instr) => (
                    instr.random_volume_variation(),
                    instr.random_pan_variation(),
                ),
                None => (0.0, 0.0),
            };
            if rvv > 0.0 {
                let d = self.rng_bipolar() * rvv;
                self.volume = (self.volume * (1.0 + d)).clamp(0.0, 1.0);
            }
            if rpv > 0.0 {
                let d = self.rng_bipolar() * rpv;
                self.panning = (self.panning + d).clamp(0.0, 1.0);
            }
        }
    }

    fn tickn_update_instr(&mut self, pool: &mut VoicePool<'a>) {
        if let Some(instr) = self.live_mut(pool) {
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
            // Tracker panning is linear: 0.0 => full left, 1.0 => full right.
            // actual_volume[0] multiplies the left sample channel,
            // actual_volume[1] the right. This matches every historical
            // tracker (PT/ST3/FT2/IT) and schism's `sndmix.c:544`:
            //   chan->left_volume_new  = (realvol * pan) >> 8;
            //   chan->right_volume_new = (realvol * (256 - pan)) >> 8;
            self.actual_volume[0] = volume * (1.0 - panning);
            self.actual_volume[1] = volume * panning;

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

    pub(crate) fn tick(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
        if let Some(instr) = self.live_mut(pool) {
            instr.tick();
            self.tickn_effects(current_tick, pool);
            self.tickn_update_instr(pool);
        } else if self.current.has_delay() {
            self.tickn_effects(current_tick, pool);
            self.tickn_update_instr(pool);
        }
        // Advance ghost voices even if the live instrument is gone —
        // ghosts have independent envelope state and must keep
        // decaying regardless of what the pattern is doing.
        self.tick_ghosts(pool);
    }

    fn tickn_effects(&mut self, current_tick: usize, pool: &mut VoicePool<'a>) {
        if current_tick == 0 {
            // At every row-load tick, FT2 clears the tremor mute flag:
            // its Tremor effect never runs at tickZero (only in the
            // TickNonZero jump table), so the row always begins
            // "playing". If the current row also carries a Tremor
            // effect, its handler below will overwrite this; if it
            // doesn't, the previous row's last tremor state (which
            // might have left us on the "off" half) must not mute the
            // whole next row.
            //
            // ST3 does NOT clear at row load: `atreon` and `atremor`
            // persist across rows, so the cycle continues uninter-
            // rupted when consecutive rows carry Ixy. For S3M we
            // therefore skip the clear; the mute flag is driven
            // exclusively from the state machine in the Tremor arm
            // below.
            if !self.module.profile.quirks.tremor_state_persists {
                self.effect_tremor = false;
            }
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
                TrackEffect::ChannelVolumeSlide { speed, fine } => {
                    // Same tick scoping as VolumeSlide: fine slides
                    // fire once at tick 0; non-fine slides fire on
                    // ticks > 0 only. Channel volume stays clamped
                    // to [0, 1] (→ 0..64 in IT units).
                    if fine == (current_tick == 0) {
                        self.channel_volume = (self.channel_volume + speed).clamp(0.0, 1.0);
                    }
                }
                TrackEffect::Glissando(glissando) => {
                    if current_tick == 0 {
                        self.effect_semitone = glissando;
                    }
                }
                TrackEffect::InstrumentFineTune(finetune) => {
                    if current_tick == 0 {
                        if let Some(pitch) = self.current.note.pitch() {
                            if let Some(instr) = self.live_mut(pool) {
                                instr.set_finetune(finetune);
                                // Recomputing the playback frequency
                                // after a finetune change must use the
                                // remapped output note, same as the
                                // initial trigger. Otherwise a finetune
                                // event on a drum-kit voice would jump
                                // to the input-note pitch and break the
                                // remap.
                                let played = instr.played_pitch_for(pitch);
                                self.note = played.value() as f32 + instr.get_finetuned_pitch();
                                self.period = self.period_helper.note_to_period(self.note);
                            }
                        }
                    }
                }
                TrackEffect::InstrumentNewNoteAction(nna) => {
                    // S73 / S74 / S75 / S76 — override the NNA that
                    // will govern the NEXT trigger on this channel.
                    // Applied at tick 0 only; the override is single-
                    // shot and cleared inside `tick0_change_instr`
                    // the first time a new instrument is loaded.
                    if current_tick == 0 {
                        self.nna_override = Some(nna);
                    }
                }
                TrackEffect::InstrumentPanningEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_panning.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentPanningEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_panning.enabled = pe;
                        }
                    }
                }
                TrackEffect::InstrumentPitchEnvelope(pe) => {
                    // S7B / S7C: enable / disable the instrument's
                    // pitch envelope on this channel. Same tick-0
                    // scoping as the volume / panning envelope
                    // toggles above. Takes effect on the next tick
                    // via `StateInstrDefault::envelopes()`.
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_pitch.enabled = pe;
                        }
                    }
                }
                TrackEffect::InstrumentSampleOffset(seek) => {
                    if current_tick == 0 && self.current.note.pitch().is_some() {
                        if let Some(instr) = self.live_mut(pool) {
                            if let Some(sample) = &mut instr.state_sample {
                                let sample_len = sample.sample_len();
                                if seek >= sample_len {
                                    // Past-end behaviour branches on
                                    // the IT "Old Effects" flag
                                    // (ITTECH: "Oxx past the sample
                                    // end will be ignored, unless
                                    // 'Old Effects' is ON, in which
                                    // case the Oxx will play from
                                    // the end of the sample."). The
                                    // XM / MOD / S3M paths behave
                                    // like IT-default (ignore); a
                                    // proper-IT module with
                                    // old_effects ON will clamp.
                                    if self.module.profile.quirks.it_old_effects && sample_len > 0 {
                                        sample.set_position(sample_len - 1);
                                    }
                                    // else: ignore the offset; leave
                                    // the position where the natural
                                    // note trigger put it.
                                } else {
                                    sample.set_position(seek);
                                }
                            }
                        }
                    }
                }
                TrackEffect::InstrumentSurround(surround) => {
                    // S91 (surround) / S90 (off). Pseudo-stereo
                    // "surround" in IT is classically implemented by
                    // inverting the phase of the right channel's
                    // output — the inverted pair folds to silence in
                    // mono (a characteristic IT fingerprint) and
                    // sounds spatially wider than pure stereo in a
                    // two-speaker field. The actual inversion lives
                    // in `Channel::next`; this arm just toggles the
                    // flag. Persists until explicitly toggled;
                    // note triggers do NOT reset it.
                    if current_tick == 0 {
                        self.surround = surround;
                    }
                }
                TrackEffect::InstrumentVolumeEnvelopePosition(position) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_volume.counter = position;
                        }
                    }
                }
                TrackEffect::InstrumentVolumeEnvelope(pe) => {
                    if current_tick == 0 {
                        if let Some(instr) = self.live_mut(pool) {
                            instr.envelope_volume.enabled = pe;
                        }
                    }
                }
                TrackEffect::NoteCut { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S70: cut all ghost voices on this
                            // channel. Live voice is untouched per
                            // ITTECH (S7x targets only detached
                            // notes).
                            self.past_note_cut_all(pool);
                        } else {
                            self.cut_pitch();
                        }
                    }
                }
                TrackEffect::NoteDelay(delay) => {
                    if current_tick == 0 {
                        if matches!(self.current.note, CellNote::Empty) {
                            self.trigger_pitch(
                                TRIGGER_KEEP_SAMPLE_POSITION
                                    | TRIGGER_KEEP_VOLUME
                                    | TRIGGER_KEEP_PERIOD,
                                pool,
                            );
                        } else if matches!(self.current.note, CellNote::KeyOff) {
                            if self.current.instrument.is_none() {
                                self.key_off(0, pool);
                            } else {
                                self.trigger_pitch(
                                    TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE,
                                    pool,
                                );
                            }
                        }
                    } else if current_tick == delay {
                        self.tick0_load_instrument_and_pitch(pool);
                        self.tickn_effects(0, pool);

                        /* Special KeyOff cases */
                        if matches!(self.current.note, CellNote::KeyOff) {
                            if self.current.instrument.is_none() {
                                if let Some(i) = self.live_mut(pool) {
                                    i.volume_reset();
                                }
                            } else {
                                self.trigger_pitch(TRIGGER_KEEP_NONE, pool);
                            }
                        }
                    }
                }
                TrackEffect::NoteFadeOut { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S72: fade all ghost voices.
                            self.past_note_fade_all(pool);
                        } else if let Some(i) = self.live_mut(pool) {
                            // Unsustain the live voice so its
                            // `volume_fadeout` starts ticking down.
                            // `key_off` is the existing entry point
                            // for "release envelope + start fadeout".
                            i.key_off();
                        }
                    }
                }
                TrackEffect::NoteOff { tick: t, past } => {
                    if current_tick == t {
                        if past {
                            // S71: key-off all ghost voices.
                            self.past_note_off_all(pool);
                        } else {
                            self.key_off(t, pool);
                        }
                    }
                }
                TrackEffect::NoteRetrig {
                    speed: s,
                    volume_modifier: m,
                } => {
                    // NoteDelay interaction: when a row carries both
                    // `SDx` (NoteDelay) and `Qxy` (NoteRetrig), the
                    // retrig cadence is measured from the delayed
                    // trigger, not from tick 0. Before the delay
                    // fires, we simply don't count — the voice
                    // hasn't actually started yet.
                    if current_tick < self.effect_note_delay {
                        continue;
                    }

                    let current_state = NoteRetrigState {
                        note: self.current.note,
                        instr: self.current.instrument,
                        speed: s,
                        volume_modifier: m.clone(),
                    };

                    // Reset the retrig counter at the moment the
                    // effect becomes active on a new row — for a
                    // plain row that's tick 0, for a SDx-delayed
                    // row it's the tick the delay fires.
                    let counter_reset_tick = self.effect_note_delay;
                    if current_tick == counter_reset_tick
                        && self.effect_note_retrig_backup != current_state
                    {
                        self.effect_note_retrig_counter = 0;
                        self.effect_note_retrig_backup = current_state;
                    }

                    // Increment the counter FIRST, then test.
                    self.effect_note_retrig_counter += 1;

                    // If speed (s) is 0, retrig is effectively disabled.
                    if s != 0 && self.effect_note_retrig_counter.is_multiple_of(s) {
                        self.trigger_pitch(TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_ENVELOPE, pool);
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
                        let new_period = self.period + p;
                        // The FT2 signed-overflow quirk is **only** in
                        // pitchSlideDown (p > 0 = period grows). FT2's
                        // pitchSlideUp has a plain min-clamp, not a
                        // max-clamp, so a portamento up that happens
                        // to land in the [32000, 32767] window must
                        // NOT snap to 31999 — it just passes through.
                        // Gated on `p > 0.0`.
                        if self.module.profile.quirks.ft2_pitch_slide_overflow && p > 0.0 {
                            let wrapped = (new_period as i32).rem_euclid(65536) as i16;
                            if (32000..=32767).contains(&(wrapped as i32)) {
                                self.period = 31999.0;
                            } else {
                                // wrapped < 32000: still climbing
                                //   through the normal range.
                                // wrapped ≥ 32768 (i16-negative):
                                //   FT2's signed check fails, no clamp
                                //   fires, period keeps growing.
                                self.period = new_period.max(1.0);
                            }
                        } else if let Some((clamp_min, clamp_max)) =
                            self.module.profile.quirks.period_clamp
                        {
                            // The module requests a hard period clamp.
                            // ST3 uses this when `amigalimits` is on in
                            // the masterflags (`[453, 3424]` in ST3
                            // units = `[113.25, 856]` in xmrs Amiga
                            // units — dig.c:setmasterflags, setspd).
                            // A module authored with a clamp expects
                            // pitch slides to stop at those bounds
                            // rather than freewheel into ultrasonic or
                            // sub-audible territory.
                            self.period = new_period.clamp(clamp_min, clamp_max);
                        } else {
                            // No explicit clamp: fall back to the
                            // conservative `[1, 31999]` range (or
                            // `[0, 31999]` when the module opts into
                            // `allow_zero_period`, which lets a pitch
                            // slide reach "infinitely high note" —
                            // ST3 convention without amigalimits).
                            let min_period = if self.module.profile.quirks.allow_zero_period {
                                0.0
                            } else {
                                1.0
                            };
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
                    if self.module.profile.quirks.tremor_state_persists {
                        // ST3 tremor (digcmd.c:s_tremor line 803): a
                        // count-down `atremor` + on/off toggle
                        // `atreon`, both persistent across rows. On
                        // every tick where Ixy runs (including tick
                        // 0), if the counter is > 0 decrement it,
                        // else toggle the on/off state and reload the
                        // counter with the corresponding nibble.
                        //
                        // The on/off nibble *cache* is refreshed at
                        // tick 0 (so memory via GET_LAST_NFO is in
                        // sync) but the state machine is NOT reset —
                        // consecutive Ixy rows form a continuous
                        // cycle, matching ST3.
                        //
                        // Initial state: counter == -1 (inactive).
                        // On the first Ixy tick, counter is non-0
                        // positive? no — we treat -1 as "toggle now"
                        // and the first toggle lands on "playing"
                        // (silent=false, which matches ST3's first
                        // hit where atreon flips from false to true).
                        if current_tick == 0 {
                            self.effect_tremor_on = on;
                            self.effect_tremor_off = off;
                        }
                        if self.effect_tremor_counter_s3m > 0 {
                            self.effect_tremor_counter_s3m -= 1;
                        } else {
                            // Toggle on/off (or start "on" from the
                            // initial inactive state). ITTECH: the
                            // counter reloads with nibble + 1 so
                            // that `Ix0` still produces a one-tick
                            // on phase (reload = 1, fires once,
                            // toggles next tick). Parser stores raw
                            // nibble; we apply the `+1` here to
                            // keep import conventions simple.
                            self.effect_tremor_silent_s3m = !self.effect_tremor_silent_s3m;
                            let reload = if self.effect_tremor_silent_s3m {
                                self.effect_tremor_off + 1
                            } else {
                                self.effect_tremor_on + 1
                            } as i32;
                            self.effect_tremor_counter_s3m = reload;
                        }
                        self.effect_tremor = self.effect_tremor_silent_s3m;
                    } else {
                        // FT2/XM Txy: per-row retrigger with modular
                        // formula. `current_tick - 1` gives 0 on the
                        // first effect tick; `(on + off + 2)` is the
                        // cycle length (FT2 plays for `on + 1`
                        // ticks then silences for `off + 1`).
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
                }
                TrackEffect::Vibrato { speed: s, depth: d } => {
                    // IT "Old Effects" quirk: with the flag ON, Hxy
                    // and Uxy depth is doubled (`depth <<= 1` per
                    // IT_MUSIC.ASM's Hxy handler in ITTECH). The
                    // importer emits the normal-mode value; apply
                    // the 2× scale here on replay so the same
                    // pattern data sounds right in both modes.
                    let d = if self.module.profile.quirks.it_old_effects {
                        d * 2.0
                    } else {
                        d
                    };
                    if current_tick == 0 {
                        self.effect_vibrato.data.speed = s;
                        self.effect_vibrato.data.depth = d;
                        // ITTECH: in normal IT mode (NOT old-effects)
                        // the vibrato LFO is updated EVERY tick,
                        // including tick 0. Other formats (XM/MOD/
                        // S3M, and IT with old-effects on) tick only
                        // on non-row ticks. This one-liner adds the
                        // tick-0 advance under the right gate.
                        if self.module.profile.format.is_it()
                            && !self.module.profile.quirks.it_old_effects
                        {
                            self.effect_vibrato.tick();
                        }
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
                    } else if self.module.profile.quirks.volcol_b_advances_vibrato
                        && !self.current.has_vibrato()
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
    fn tick0_change_instr(&mut self, sample_only: bool, pool: &mut VoicePool<'a>) -> bool {
        let instrnr = self.current.instrument.unwrap();

        if let InstrumentType::Default(id) = &self.module.instrument[instrnr].instr_type {
            let was_same = self.live(pool).is_some_and(|i| i.num == instrnr);

            // Only proceed if the instrument has samples
            if !id.sample.is_empty() {
                if sample_only {
                    if let Some(i) = self.live_mut(pool) {
                        i.replace_instr(id);
                    }
                } else {
                    // IT DCT: the INCOMING instrument may ask the
                    // replayer to apply a Duplicate Check Action
                    // (cut / off / fade) to any already-playing
                    // voice whose identity clashes with the new
                    // trigger per its DCT axis (Note / Sample /
                    // Instrument). DCT runs BEFORE NNA: the new
                    // note's "clash preferences" are resolved first,
                    // then NNA decides what happens to whatever live
                    // voice is still standing afterwards.
                    let new_note = self.current.note.pitch();
                    let new_sample_index = new_note
                        .and_then(|n| {
                            id.keyboard
                                .sample_for_pitch
                                .get(n.value() as usize)
                                .copied()
                        })
                        .flatten();
                    self.apply_dct(pool, instrnr, new_note, new_sample_index);

                    // IT NNA: if the outgoing voice's NNA is anything
                    // but `Cut`, fork it into a ghost before we blow
                    // it away with the new trigger. On non-IT
                    // modules this is a cheap no-op because their
                    // importers leave NNA = NoteCut (the default).
                    self.spawn_ghost_for_outgoing(pool);
                    let state =
                        StateInstrDefault::new(id, instrnr, self.period_helper.clone(), self.rate);
                    self.install_live(pool, state, id.midi_mute_computer);
                    // Consume any one-shot S73–S76 override — it
                    // only affects this one trigger.
                    self.nna_override = None;
                }
            }

            was_same
        } else {
            // Non-Default instrument variant (InstrMidi / InstrOpl /
            // InstrSid / InstrRobSid / InstrEkn). xmrsplayer's
            // sample engine only handles `InstrumentType::Default`;
            // the specialised synth / MIDI-out paths are the
            // responsibility of downstream code that wraps the
            // player. Silently skipping here keeps playback running
            // on mixed-type modules — the voice just stays on
            // whatever sampled instrument was previously loaded.
            false
        }
    }

    /// Return true if it was the same instrument
    fn tick0_load_instrument(&mut self, pool: &mut VoicePool<'a>) -> bool {
        if let Some(instr) = self.current.instrument {
            if instr >= self.module.instrument.len() {
                // Invalid instrument, cut current note
                self.cut_pitch();
                self.drop_live(pool);
                return false;
            }
        } else {
            // No instrument to load
            return true;
        }

        if self.current.has_tone_portamento() {
            self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_SAMPLE_POSITION, pool);
            return self.tick0_change_instr(true, pool);
        }

        // Dispatch on the cell's note column. A real Play(_) falls
        // through to the regular instrument-change path; the other
        // variants apply their own trigger/return convention.
        match self.current.note {
            CellNote::Empty => {
                /* Ghost instrument, trigger note */
                let trigger_flags = if self.current.has_volume_slide() {
                    TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_PERIOD
                } else {
                    /* Sample position is kept, but envelopes are reset */
                    TRIGGER_KEEP_SAMPLE_POSITION | TRIGGER_KEEP_VOLUME | TRIGGER_KEEP_PERIOD
                };
                self.trigger_pitch(trigger_flags, pool);
                return self.tick0_change_instr(true, pool);
            }
            CellNote::KeyOff => {
                self.trigger_pitch(TRIGGER_KEEP_PERIOD, pool);
                return true; // Keyoff does not change instrument
            }
            CellNote::NoteFade => {
                // IT note-fade `~~~`: same instrument-handling rule
                // as KeyOff — the fade cell is not a "new note" and
                // must not load a different instrument or retrigger.
                // The actual fadeout work is done in
                // `tick0_load_pitch::note_fade`.
                return true;
            }
            CellNote::NoteCut | CellNote::Play(_) => {
                // Cut and Play both go through the normal path: a
                // Cut row will be handled inside
                // `tick0_change_instr` like an empty note (no
                // instrument change), and Play loads its instrument
                // and pitch.
            }
        }

        self.tick0_change_instr(false, pool)
    }

    fn tick0_load_pitch(&mut self, was_same_instr: bool, pool: &mut VoicePool<'a>) {
        // Extract the actual pitch from the cell. For non-Play
        // variants (KeyOff / NoteFade / NoteCut / Empty) this branch
        // dispatches the corresponding event and returns; only
        // Play(p) carries on to instrument/pitch loading.
        let pitch = match self.current.note {
            CellNote::Play(p) => p,
            CellNote::KeyOff => {
                if self.current.instrument.is_none() || was_same_instr {
                    self.key_off(0, pool);
                } else {
                    self.trigger_pitch(TRIGGER_KEEP_PERIOD | TRIGGER_KEEP_ENVELOPE, pool);
                }
                return;
            }
            CellNote::NoteFade => {
                // IT note-column `~~~`: start fadeout without
                // releasing the envelope. Falls through any
                // instrument change because fade has the same
                // semantics whether the instrument cell is set or
                // not — schism doesn't gate on `row_instr` for
                // NOTE_FADE either.
                self.note_fade(pool);
                return;
            }
            CellNote::NoteCut | CellNote::Empty => {
                // NoteCut handled elsewhere; Empty means there's
                // no note event on this row — nothing to do here.
                return;
            }
        };

        // Instr?
        if let Some(instr) = self.live_mut(pool) {
            // Portamento?
            if self.current.has_tone_portamento() {
                if let Some(s) = &instr.state_sample {
                    if s.is_enabled() {
                        // Same input/output split as in the SetNote
                        // branch below: the porta target's input
                        // note is what `current_note` records for
                        // DCT comparisons, but the period the slide
                        // aims at lives in frequency space, so it
                        // must use the remapped output pitch.
                        // Without this, a Gxx targeting a remapped
                        // key on a drum kit would slide to the
                        // wrong pitch.
                        let played = instr.played_pitch_for(pitch);
                        self.note = played.value() as f32 + s.get_finetuned_pitch();
                        self.current_note = Some(pitch);
                        return;
                    }
                }
                self.cut_pitch();
                return;
            }

            // IT ghost-note NNA. When a note column fires without an
            // instrument column, the live voice gets retriggered in
            // place with `TRIGGER_KEEP_VOLUME` — sample reset +
            // envelope reset happens inside `trigger_pitch`. For
            // NNA != Cut we need to snapshot the pre-retrigger state
            // now, before `set_pitch` below calls `select_sample`
            // (which can overwrite `state_sample`). XM/MOD/S3M reach
            // this path with NNA=Cut (the default) so `spawn_ghost_
            // clone` is an inert early return there.
            //
            // Gate on `instrument.is_none()` so the instrument-change
            // path (which already ran `spawn_ghost_for_outgoing` in
            // `tick0_change_instr`) doesn't double-ghost.
            if self.current.instrument.is_none() {
                self.spawn_ghost_clone(pool);
            }

            // SetNote
            if let Some(instr) = self.live_mut(pool) {
                if instr.set_pitch(pitch) {
                    if let Some(s) = &instr.state_sample {
                        // Two distinct notes are in play here:
                        //
                        //  - `pitch` is the **input note** the
                        //    pattern asked for. It stays in
                        //    `self.current_note` because that's
                        //    what DCT, NNA and porta-target lookups
                        //    compare against — pressing C-5 and D-5
                        //    on the same drum kit must produce
                        //    distinct voices even if both transpose
                        //    to the same output pitch.
                        //
                        //  - `played_pitch_for(input)` resolves the
                        //    IT keyboard-table remap to the **output
                        //    note** — the pitch the sample is
                        //    actually played at. That's what feeds
                        //    the period / frequency calculation, so
                        //    it goes into `self.note`.
                        //
                        // For non-IT formats, and for IT
                        // instruments without a remap on this key,
                        // `played_pitch_for` returns the input note
                        // unchanged and this matches the previous
                        // behaviour byte-for-byte.
                        let played = instr.played_pitch_for(pitch);
                        self.note = played.value() as f32 + s.get_finetuned_pitch();
                    }
                    self.current_note = Some(pitch);

                    let trigger_flag = if self.current.instrument.is_some() {
                        TRIGGER_KEEP_NONE
                    } else {
                        /* Ghost note: keep old volume */
                        TRIGGER_KEEP_VOLUME
                    };
                    self.trigger_pitch(trigger_flag, pool);
                    return;
                }
            }
        }

        self.cut_pitch();
    }

    fn tick0_load_instrument_and_pitch(&mut self, pool: &mut VoicePool<'a>) {
        // FT2 "K00 eats note" quirk. In FT2, K00 is handled specially at
        // tickZero via getNewNote (ft2_replayer.c:1418): it fires a keyoff
        // and then RETURNS, never running triggerNote — so the note column
        // of that row is effectively dropped.
        //
        // Key constraints:
        //   - This is canonical XM behaviour: `module.profile.format ==
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
        //   - Note-column `===` (`CellNote::KeyOff`) has a different semantic
        //     (see `key_off()`) and belongs on the normal tick-0 path
        //     via `tick0_load_pitch`, hence the helper distinguishes
        //     effect-Kxx from column-keyoff.
        if self.module.profile.quirks.k00_eats_note && self.current.has_note_off_at_tick_zero() {
            return;
        }

        // First, load instr. `was_same_instr` is true when the instrument
        // slot on this row matches the one currently held by the channel
        // (or when there's no instrument to load — see tick0_load_instrument
        // for the exact contract). This flag drives the keyoff branch in
        // tick0_load_pitch: a keyoff with a *different* instrument must
        // retrigger rather than just cut.
        let was_same_instr: bool = self.tick0_load_instrument(pool);
        // Next, choose sample from note
        self.tick0_load_pitch(was_same_instr, pool);
    }

    pub(crate) fn tick0(&mut self, pattern_slot: &TrackUnit, pool: &mut VoicePool<'a>) {
        self.current = pattern_slot.clone();

        let delay = self.current.get_delay();
        // Always write `effect_note_delay` — whether 0 or the row's
        // SDx value. Without this, a delay latched on a previous
        // row would linger and bias downstream code that keys off
        // "is this row delayed" (see `TrackEffect::NoteRetrig`'s
        // pre-delay skip).
        self.effect_note_delay = delay;
        if delay != 0 {
            // Delayed row: tick0 loads nothing; the delay fires in
            // tickn when current_tick reaches `delay`.
        } else {
            /* load instrument then note */
            self.tick0_load_instrument_and_pitch(pool);
            if let Some(instr) = self.live_mut(pool) {
                instr.tick();
            }
            self.tickn_effects(0, pool);

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
                let xm_volcol_b_alone = self.module.profile.quirks.volcol_b_advances_vibrato
                    && self.current.has_vibrato_depth();
                if !xm_volcol_b_alone {
                    // Only clear the output modulation.
                    self.effect_vibrato.clear_output();
                }
            }

            self.tickn_update_instr(pool);
        }
        // Tick ghosts every frame, whether the live voice is present,
        // delayed, or absent. Ghosts are driven purely by their own
        // envelope / fadeout state and must not stall just because
        // the live note is muted or dormant.
        self.tick_ghosts(pool);
    }
}

impl<'a> Channel<'a> {
    /// Produce one stereo sample for the channel: live voice + every
    /// still-singing ghost. Returns `None` only when both are silent
    /// — a channel whose live note has cut but whose NNA=Continue
    /// ghosts are still ringing keeps producing audio.
    ///
    /// Replaces the previous `impl Iterator for Channel` because the
    /// trait can't pass extra arguments and we now need the
    /// `VoicePool` to access ghost voices. Step 1.4 will move the
    /// live voice into the pool too, at which point this becomes a
    /// pure pool-driven mix loop.
    pub(crate) fn next_sample(&mut self, pool: &mut VoicePool<'a>) -> Option<(f32, f32)> {
        let mut left = 0.0f32;
        let mut right = 0.0f32;
        let mut any = false;

        // Live voice.
        if let Some(i) = self.live_mut(pool) {
            if let Some((l, r)) = i.next() {
                left += l * self.actual_volume[0];
                right += r * self.actual_volume[1];
                any = true;
            }
        }

        // Ghost voices. Each ghost owns its frozen gain / pan, so
        // we just accumulate. Stale handles (which 1.3 shouldn't
        // produce but the pool tolerates) are skipped silently.
        for id in &self.ghosts {
            if let Some(g) = pool.get_mut(*id) {
                if let Some((l, r)) = g.next() {
                    left += l;
                    right += r;
                    any = true;
                }
            }
        }

        if any {
            // S91 surround: invert the right channel's phase. This
            // is IT's canonical pseudo-stereo trick — the signal
            // folds to silence in mono and reads as unusually wide
            // in a stereo field. Applied post-mix so it covers both
            // live and ghost contributions with the same phase.
            if self.surround {
                right = -right;
            }
            Some((left, right))
        } else {
            None
        }
    }
}
