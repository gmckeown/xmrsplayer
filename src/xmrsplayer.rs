//! Top-level player facade.
//!
//! `XmrsPlayer` is a thin wrapper that glues three orthogonal pieces together:
//!
//! * a [`Sequencer`] — owns the song cursor, the tick clock, and every
//!   navigation/tempo effect;
//! * a [`Voices`] engine — owns the per-channel audio state and the
//!   mixer. Structurally it is the *default row observer*, but it's held
//!   concretely (not through `dyn`) so the audio hot path keeps every
//!   opportunity for the compiler to inline;
//! * a vector of user-registered [`PlayerObserver`] trait objects — the
//!   "anyone can subscribe" slot: UI synchronisers, MIDI bridges, probes…
//!
//! The public API mirrors pre-0.10 `XmrsPlayer` method-for-method so existing
//! callers don't need to change anything beyond a handful of field accesses
//! that have moved to accessors (see `OBSERVERS.md`).

use alloc::{boxed::Box, vec, vec::Vec};
use xmrs::prelude::*;

use crate::audio_observer::{ChannelsContext, ChannelsObserver, MixContext, MixObserver};
use crate::midi_observer::MidiObserver;
use crate::observer::{PatternChangeContext, PlayerObserver, RowContext, TickContext};
use crate::sequencer::{Sequencer, TickOutcome};
use crate::voices::{Voices, DEFAULT_VOICE_POOL_CAPACITY};

pub struct XmrsPlayer<'a> {
    /// The module being played. Kept public because it's an immutable
    /// borrow supplied by the caller — `&Module` has no invariant the
    /// player owns, so there's nothing to guard.
    pub module: &'a Module,

    sequencer: Sequencer<'a>,
    voices: Voices<'a>,

    observers: Vec<Box<dyn PlayerObserver + Send>>,

    /// Audio observers subscribed to the final (post-sum, pre-gain) stereo
    /// mix. Called once per produced sample.
    audio_mix_observers: Vec<Box<dyn MixObserver + Send>>,

    /// Audio observers subscribed to the per-channel pre-sum slice. Called
    /// once per produced sample, but only if at least one is registered —
    /// the capture path in `Voices::mix` is gated on this list being
    /// non-empty to avoid paying the per-channel write cost otherwise.
    audio_channels_observers: Vec<Box<dyn ChannelsObserver + Send>>,

    /// MIDI observers subscribed to MIDI events emitted by the macro
    /// interpreter. Called per-event during `process_row` dispatch.
    /// Per-row frequency is very low (at most a handful of macros
    /// per row); no hot-path gating needed.
    midi_observers: Vec<Box<dyn MidiObserver + Send>>,

    /// Scratch buffer that `Voices::mix` fills when the channels-observer
    /// list is non-empty. Pre-allocated to `num_channels` at construction
    /// and reused on every `sample()` so the hot path never allocates.
    /// Per-channel pre-gain mix snapshot in `i16` PCM —
    /// what observers and `samples_from_channels` return.
    /// Pre-allocated to `num_channels` at construction and
    /// reused on every `sample()` so the hot path never
    /// allocates.
    per_channel_buffer: Vec<(i16, i16)>,

    /// Facade-owned sample clock. Q.16 sample accumulator —
    /// decremented by `1 << 16` each output sample, refilled
    /// by `samples_per_tick_q16` on tick boundaries. Keeps the
    /// fractional samples-per-tick (for BPMs that don't divide
    /// the sample rate evenly) without ever touching f32.
    remaining_samples_in_tick: i64,
    /// Cached integer sample rate. Used both by the user-facing
    /// `f32` getter (lossless widen) and by the integer Q.16
    /// clock arithmetic.
    sample_rate_hz: u32,

    /// Running total of stereo samples produced. Exposed read-only — writing
    /// it from outside would desync the internal clock, which is why the
    /// pre-0.10 public field has become an accessor.
    generated_samples: u64,

    /// Last tempo value propagated to per-channel arpeggio state. The per-
    /// step propagation loop is skipped when tempo has not actually changed.
    last_propagated_tempo: usize,

    /// `None` when the next `Iterator::next()` should pull a new stereo
    /// sample; `Some(r)` when we already produced the left half and owe
    /// the caller the right.
    right_sample: Option<i16>,

    pause: bool,

    /// `on_song_end` must fire at most once per song-termination event.
    song_end_notified: bool,
}

impl<'a> XmrsPlayer<'a> {
    /// Build a player for the given module, with the default voice
    /// pool capacity (256 voices, matching schism's `MAX_VOICES`).
    /// For modules that stack many NNA voices simultaneously
    /// (`Another Life` and the like), use
    /// [`Self::new_with_voice_pool_capacity`] to raise this limit.
    ///
    /// Format-specific FT2/XM behaviour — arpeggio LUT, E60 pattern-
    /// loop bug, arpeggio period clamp, portamento-down signed-
    /// overflow, K00-eats-note, vol-col `Bx` advancing the vibrato —
    /// is driven entirely by `module.profile.format`. `ModuleFormat::Xm`
    /// activates the canonical FT2 reference behaviour, all other
    /// formats use their own native semantics.
    pub fn new(module: &'a Module, sample_rate: u32, song: usize) -> Self {
        Self::new_with_voice_pool_capacity(module, sample_rate, song, DEFAULT_VOICE_POOL_CAPACITY)
    }

    /// Like [`Self::new`] but with a caller-chosen voice pool
    /// capacity. The pool must hold at least one voice (`capacity >=
    /// 1`) and below 65535 (`VoiceId`'s packed-handle ceiling); the
    /// constructor panics outside that range.
    ///
    /// Reasonable values:
    ///
    /// - `256` (default) is what schism uses and what most IT
    ///   modules expect.
    /// - `512`–`1024` for tracker compositions that intentionally
    ///   layer dozens of NNA tails per channel — the cost is just a
    ///   larger working set; the mix loop isn't sensitive to pool
    ///   size.
    /// - `64`–`128` if you're targeting embedded hardware where
    ///   even 256 voices is too much state to keep around.
    pub fn new_with_voice_pool_capacity(
        module: &'a Module,
        sample_rate: u32,
        song: usize,
        voice_pool_capacity: usize,
    ) -> Self {
        let sample_rate = sample_rate.max(1);
        let sequencer = Sequencer::new(module, song);
        let voices = Voices::new_with_voice_pool_capacity(
            module,
            sample_rate,
            module.default_tempo,
            voice_pool_capacity,
        );
        let num_channels = voices.num_channels();

        Self {
            module,
            last_propagated_tempo: sequencer.tempo(),
            sequencer,
            voices,
            observers: Vec::new(),
            audio_mix_observers: Vec::new(),
            audio_channels_observers: Vec::new(),
            midi_observers: Vec::new(),
            per_channel_buffer: vec![(0, 0); num_channels],
            remaining_samples_in_tick: 0,
            sample_rate_hz: sample_rate,
            generated_samples: 0,
            right_sample: None,
            pause: false,
            song_end_notified: false,
        }
    }

    // --- Observer registration ---

    /// Register an observer that will receive row/tick/pattern-change events
    /// until the player is dropped or [`clear_observers`](Self::clear_observers)
    /// is called. Observers are invoked in registration order.
    ///
    /// The `Send` bound comes from the typical deployment pattern — the
    /// player is wrapped in `Arc<Mutex<_>>` and shared with an audio
    /// callback thread — which requires every owned field to cross thread
    /// boundaries. See the `PlayerObserver` trait docs for details.
    pub fn add_observer(&mut self, observer: Box<dyn PlayerObserver + Send>) {
        self.observers.push(observer);
    }

    /// Remove every registered observer. Does not affect the built-in voices
    /// engine, which is not an observer from the type system's perspective —
    /// it is always driven directly by the facade.
    pub fn clear_observers(&mut self) {
        self.observers.clear();
    }

    /// Number of currently registered observers (excluding the built-in
    /// voices engine).
    pub fn observer_count(&self) -> usize {
        self.observers.len()
    }

    // --- Audio observer registration ---

    /// Register an observer on the final stereo mix (pre-gain). Called once
    /// per produced sample. See [`MixObserver`] for the contract.
    pub fn add_audio_mix_observer(&mut self, observer: Box<dyn MixObserver + Send>) {
        self.audio_mix_observers.push(observer);
    }

    /// Remove every registered mix observer.
    pub fn clear_audio_mix_observers(&mut self) {
        self.audio_mix_observers.clear();
    }

    /// Number of currently registered mix observers.
    pub fn audio_mix_observer_count(&self) -> usize {
        self.audio_mix_observers.len()
    }

    /// Register an observer on the per-channel pre-sum slice. Called once
    /// per produced sample. Registering at least one of these enables the
    /// per-channel capture path in the mixer — prefer a
    /// [`MixObserver`](Self::add_audio_mix_observer) if you only need the
    /// summed mix. See [`ChannelsObserver`] for the contract.
    pub fn add_audio_channels_observer(&mut self, observer: Box<dyn ChannelsObserver + Send>) {
        self.audio_channels_observers.push(observer);
    }

    /// Remove every registered channels observer.
    pub fn clear_audio_channels_observers(&mut self) {
        self.audio_channels_observers.clear();
    }

    /// Number of currently registered channels observers.
    pub fn audio_channels_observer_count(&self) -> usize {
        self.audio_channels_observers.len()
    }

    // --- MIDI observer registration ---

    /// Register an observer for MIDI events emitted by the macro
    /// interpreter. Called once per event, in order, after each row
    /// has been processed. See [`MidiObserver`] for the contract.
    ///
    /// MIDI events are generated only by IT modules that embed MIDI
    /// macros and trigger them via `Zxx` / `SFx`. Registering an
    /// observer on a non-IT module or an IT module without macros
    /// costs nothing at runtime — the buffer stays empty.
    pub fn add_midi_observer(&mut self, observer: Box<dyn MidiObserver + Send>) {
        self.midi_observers.push(observer);
    }

    /// Remove every registered MIDI observer.
    pub fn clear_midi_observers(&mut self) {
        self.midi_observers.clear();
    }

    /// Number of currently registered MIDI observers.
    pub fn midi_observer_count(&self) -> usize {
        self.midi_observers.len()
    }

    // --- Mute helpers (delegated to voices) ---

    pub fn set_mute_channel(&mut self, channel_num: usize, mute: bool) {
        self.voices.set_mute_channel(channel_num, mute);
    }

    pub fn mute_all(&mut self, mute: bool) {
        self.voices.mute_all(mute);
    }

    // --- Loop / pause ---

    pub fn set_max_loop_count(&mut self, max_loop_count: usize) {
        self.sequencer.set_max_loop_count(max_loop_count);
    }

    pub fn get_loop_count(&self) -> usize {
        self.sequencer.loop_count()
    }

    /// Force pause, returning (0.0, 0.0) samples. While paused, neither the
    /// sequencer nor voices advance, so observers see no events.
    pub fn pause(&mut self, pause: bool) {
        self.pause = pause;
    }

    // --- Clock / cursor accessors (preserve pre-0.10 API) ---

    /// Output sample-rate in Hz.
    pub fn get_sample_rate(&self) -> u32 {
        self.sample_rate_hz
    }

    /// Alias of [`Self::get_sample_rate`] returning the typed
    /// `SampleRate` newtype. Kept for the older `_q` naming
    /// convention.
    #[doc(hidden)]
    pub fn get_sample_rate_q(&self) -> xmrs::fixed::units::SampleRate {
        xmrs::fixed::units::SampleRate::from_hz(self.sample_rate_hz)
    }

    pub fn get_tempo(&self) -> usize {
        self.sequencer.tempo()
    }

    pub fn get_bpm(&self) -> usize {
        self.sequencer.bpm()
    }

    /// Returns current pattern number in pattern_order.
    pub fn get_current_pattern(&self) -> usize {
        self.sequencer.current_pattern()
    }

    /// Returns current index in pattern_order.
    pub fn get_current_table_index(&self) -> usize {
        self.sequencer.current_table_index()
    }

    /// Returns current row (matches pre-0.10 off-by-one semantic: this is
    /// the index of the *next* row to load once the current row's tick
    /// cycle wraps. For the row currently sounding, use
    /// [`playing_row`](Self::playing_row) instead).
    pub fn get_current_row(&self) -> usize {
        self.sequencer.current_row()
    }

    /// Row currently sounding — stable for the duration of the row's tick
    /// cycle. Prefer this over `get_current_row` when you want what the
    /// listener is hearing *right now*.
    pub fn playing_row(&self) -> usize {
        self.sequencer.playing_row()
    }

    /// Pattern of the row currently sounding.
    pub fn playing_pattern(&self) -> usize {
        self.sequencer.playing_pattern()
    }

    pub fn current_song(&self) -> usize {
        self.sequencer.current_song()
    }

    pub fn generated_samples(&self) -> u64 {
        self.generated_samples
    }

    // --- Global gain accessors (replace pre-0.10 `pub` fields) ---

    /// Q1.15 song-driven master volume.
    pub fn global_volume(&self) -> xmrs::fixed::units::Volume {
        self.voices.global_volume()
    }
    /// Set song-driven master volume.
    pub fn set_global_volume(&mut self, v: xmrs::fixed::units::Volume) {
        self.voices.set_global_volume(v);
    }
    /// Alias kept for the older `_q` naming convention.
    #[doc(hidden)]
    pub fn set_global_volume_q(&mut self, v: xmrs::fixed::units::Volume) {
        self.voices.set_global_volume(v);
    }

    /// Q4.12 user-driven amplification (up to 8×).
    pub fn amplification(&self) -> xmrs::fixed::units::Amplification {
        self.voices.amplification()
    }
    /// Set user-driven amplification.
    pub fn set_amplification(&mut self, a: xmrs::fixed::units::Amplification) {
        self.voices.set_amplification(a);
    }
    /// Alias kept for the older `_q` naming convention.
    #[doc(hidden)]
    pub fn set_amplification_q(&mut self, a: xmrs::fixed::units::Amplification) {
        self.voices.set_amplification(a);
    }

    // --- Navigation (goto) ---

    /// Jump to row at index `table_position` in pattern_order, at `speed`.
    /// If `speed == 0`, resets to the module's default speed.
    ///
    /// Returns `false` if the target is out of range.
    pub fn goto(&mut self, table_position: usize, row: usize, speed: usize) -> bool {
        if !self.sequencer.goto(table_position, row, speed) {
            return false;
        }
        self.voices.reset_for_goto();
        // Force next()/sample() to re-tick immediately.
        self.remaining_samples_in_tick = 0;
        // Clear the "song end" latch so a post-song goto re-enables playback
        // (if `max_loop_count` still gates it, SongEnd will re-fire; that's OK).
        self.song_end_notified = false;
        true
    }

    // --- Sample generation hot path ---

    /// Advance one sample's worth of time: stepping the sequencer when a
    /// tick boundary is due, forwarding row/tick events to voices and
    /// observers, and decrementing the sample clock.
    ///
    /// Returns `false` to signal end-of-song.
    fn step(&mut self) -> bool {
        if self.remaining_samples_in_tick <= 0 {
            match self.sequencer.advance_tick() {
                TickOutcome::SongEnd => {
                    if !self.song_end_notified {
                        for obs in &mut self.observers {
                            obs.on_song_end();
                        }
                        self.song_end_notified = true;
                    }
                    return false;
                }
                TickOutcome::RowStart {
                    pattern,
                    row,
                    pattern_changed,
                } => {
                    // Fetch cells ONCE — voices and every observer share the
                    // same borrow.
                    let cells: &[TrackUnit] = &self.module.pattern[pattern][row];

                    // 1. Voices (the built-in row consumer) processes the row.
                    self.voices.process_row(cells);

                    // 1b. Drain MIDI events the macro interpreter
                    //     produced during the row and dispatch to
                    //     any registered MIDI observers. Done here
                    //     (not per-tick) because macros only fire
                    //     at row-start.
                    if !self.midi_observers.is_empty() {
                        for (src_ch, event) in self.voices.drain_midi_events() {
                            for obs in &mut self.midi_observers {
                                obs.on_midi_event(&event, src_ch);
                            }
                        }
                    } else {
                        // Still drain so the buffer doesn't grow —
                        // `process_row` clears at entry too, but an
                        // unsubscribed drain here keeps the two
                        // paths consistent.
                        self.voices.drain_midi_events().for_each(drop);
                    }

                    // 2. User observers: on_pattern_change first (if
                    //    applicable), then on_row.
                    if pattern_changed && !self.observers.is_empty() {
                        let pc_ctx = PatternChangeContext {
                            module: self.module,
                            song: self.sequencer.current_song(),
                            new_table_index: self.sequencer.current_table_index(),
                            new_pattern: pattern,
                        };
                        for obs in &mut self.observers {
                            obs.on_pattern_change(&pc_ctx);
                        }
                    }

                    if !self.observers.is_empty() {
                        let ctx = RowContext {
                            module: self.module,
                            song: self.sequencer.current_song(),
                            pattern,
                            row,
                            cells,
                            tempo: self.sequencer.tempo(),
                            bpm: self.sequencer.bpm(),
                        };
                        for obs in &mut self.observers {
                            obs.on_row(&ctx);
                        }
                    }
                }
                TickOutcome::Tick { tick } => {
                    // 1. Voices run per-tick effects.
                    self.voices.process_tick(tick);

                    // 2. Observers that opt in get an on_tick callback.
                    //    Walk the list twice only when at least one subscribes —
                    //    a cheap short-circuit when the common case (UI
                    //    observers on rows only) dominates.
                    if self.observers.iter().any(|obs| obs.subscribes_to_ticks()) {
                        let pat = self.sequencer.playing_pattern();
                        let row = self.sequencer.playing_row();
                        let cells: &[TrackUnit] = &self.module.pattern[pat][row];
                        let ctx = TickContext {
                            module: self.module,
                            song: self.sequencer.current_song(),
                            pattern: pat,
                            row,
                            tick,
                            cells,
                        };
                        for obs in &mut self.observers {
                            if obs.subscribes_to_ticks() {
                                obs.on_tick(&ctx);
                            }
                        }
                    }
                }
            }

            // Propagate tempo change to voices if any global effect (or the
            // initial tempo on the very first tick) just shifted it.
            let cur_tempo = self.sequencer.tempo();
            if cur_tempo != self.last_propagated_tempo {
                self.voices.set_tempo(cur_tempo);
                self.last_propagated_tempo = cur_tempo;
            }

            // FT2 manual: number of ticks / second = BPM × 0.4 =
            //   BPM × 2 / 5. So samples_per_tick = sample_rate ×
            //   5 / (BPM × 2). In Q.16 raw:
            //     samples_per_tick_q16 =
            //         (sample_rate × 5 << 16) / (bpm × 2)
            //       = (sample_rate << 15) × 5 / bpm
            // Worst case at 192 kHz / 1 BPM: ~3 × 10^10, fits u64.
            //
            // BPM is clamped to ≥ 1 to avoid divide-by-zero on
            // pathological modules. The OLD f32 path silently
            // returned `inf` and stalled the clock; the integer
            // path keeps stepping by holding the previous tick
            // duration.
            let bpm = self.sequencer.bpm().max(1) as u64;
            let samples_per_tick_q16: u64 = ((self.sample_rate_hz as u64) << 15) * 5 / bpm;
            self.remaining_samples_in_tick = self
                .remaining_samples_in_tick
                .saturating_add(samples_per_tick_q16 as i64);
        }
        // Decrement by one whole sample (= 1.0 in Q.16).
        self.remaining_samples_in_tick -= 1 << 16;
        true
    }

    /// Returns samples from each channel before applying global volume and
    /// amplification. If the function returns `None`, no more samples are
    /// available (end of song reached).
    ///
    /// In conjunction with [`samples_apply_volume`](Self::samples_apply_volume),
    /// this function can be used to replace the iterator or the `sample()`
    /// function if you want to control each channel in fine detail — for
    /// example, to create per-channel graphic effects.
    ///
    /// Note: this is an **alternative** consumption path to
    /// [`sample`](Self::sample). Audio observers ([`MixObserver`] /
    /// [`ChannelsObserver`]) are fired only from `sample()` / the
    /// `Iterator` impl; they are **not** fired when samples are pulled
    /// through `samples_from_channels`, to avoid double dispatch if a
    /// caller mixes both APIs.
    pub fn samples_from_channels(&mut self) -> Option<Vec<(i16, i16)>> {
        if self.pause {
            return Some(vec![(0, 0); self.voices.num_channels()]);
        }
        if !self.step() {
            return None;
        }
        let samples = self.voices.samples_from_channels();
        self.generated_samples += 1;
        Some(samples)
    }

    /// Sum per-channel samples into a stereo pair, without gain.
    /// Saturating i32 accumulator so 64+ stacked channels can't
    /// overflow in pathological mixes — clamps to `i16` on the
    /// way out.
    pub fn samples_to_sample(&self, samples: &[(i16, i16)]) -> (i16, i16) {
        let (al, ar) = samples.iter().fold((0i32, 0i32), |(acc_l, acc_r), (l, r)| {
            (acc_l + *l as i32, acc_r + *r as i32)
        });
        (
            al.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            ar.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    /// Apply `global_volume * amplification` to a per-channel
    /// sample set and fold to a single stereo pair. Intended to
    /// be paired with [`samples_from_channels`](Self::samples_from_channels).
    /// Pure integer Q-format — `Volume` (Q1.15) and `Amplification`
    /// (Q4.12) compose to Q5.27 then narrow back to `i16`.
    pub fn samples_apply_volume(&self, samples: &[(i16, i16)]) -> (i16, i16) {
        let gv = self.voices.global_volume().as_q15_i32(); // Q1.15
        let amp = self.voices.amplification().as_q4_12_i32(); // Q4.12
        let (l, r) = self.samples_to_sample(samples);
        // sample × gv × amp: Q1.15 × Q4.12 → Q5.27, narrow with
        // round-half bias before clamp to i16.
        let scale_q27 = gv as i64 * amp as i64;
        let l64 = (l as i64).wrapping_mul(scale_q27);
        let r64 = (r as i64).wrapping_mul(scale_q27);
        let bias = 1i64 << 26;
        let li = if l64 >= 0 {
            (l64 + bias) >> 27
        } else {
            -(((-l64) + bias) >> 27)
        };
        let ri = if r64 >= 0 {
            (r64 + bias) >> 27
        } else {
            -(((-r64) + bias) >> 27)
        };
        (
            li.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
            ri.clamp(i16::MIN as i64, i16::MAX as i64) as i16,
        )
    }

    /// Return the sum of the samples from all channels,
    /// optionally scaled by `global_volume * amplification`.
    /// Hot path used by the `Iterator` impl. Output is `i16`
    /// PCM.
    ///
    /// While paused, returns `Some((0, 0))` and still fires
    /// audio observers with zero samples (documented contract —
    /// recorders capturing silence, oscilloscopes drawing
    /// flatlines). Player-side observers are not notified
    /// during pause because no song events happen.
    pub fn sample(&mut self, apply_volume: bool) -> Option<(i16, i16)> {
        if self.pause {
            self.notify_audio_observers_paused();
            return Some((0, 0));
        }
        if !self.step() {
            return None;
        }

        let capture = !self.audio_channels_observers.is_empty();
        let mix_pre_gain_i32 = if capture {
            let buf = self.per_channel_buffer.as_mut_slice();
            self.voices.mix(Some(buf))
        } else {
            self.voices.mix(None)
        };

        // Pre-gain mix as i16 for the observer dispatch — saturating
        // narrowing happens here. Observers see what would have hit
        // the gain stage if it were a no-op.
        let mix_pre_gain_i16 = Voices::saturate_to_i16(mix_pre_gain_i32);
        self.notify_audio_observers(mix_pre_gain_i16);

        let s = if apply_volume {
            // Apply final gain (gv × amp × mix_volume) to the
            // un-clamped i32 accumulator and saturate to i16.
            // Single-pass Q9.39 multiply — no intermediate
            // clamp that would lose mix-bus headroom.
            self.voices.apply_final_gain(mix_pre_gain_i32)
        } else {
            mix_pre_gain_i16
        };

        self.generated_samples += 1;
        Some(s)
    }

    /// Dispatch audio observers for a real (non-paused) sample.
    /// Channels observers first, then mix observers.
    fn notify_audio_observers(&mut self, mix: (i16, i16)) {
        let global_volume = self.voices.global_volume_q();
        let amplification = self.voices.amplification_q();

        if !self.audio_channels_observers.is_empty() {
            let channels: &[(i16, i16)] = self.per_channel_buffer.as_slice();
            let ctx = ChannelsContext {
                channels,
                global_volume,
                amplification,
            };
            for obs in &mut self.audio_channels_observers {
                obs.on_channels(&ctx);
            }
        }

        if !self.audio_mix_observers.is_empty() {
            let ctx = MixContext {
                left: mix.0,
                right: mix.1,
                global_volume,
                amplification,
            };
            for obs in &mut self.audio_mix_observers {
                obs.on_mix(&ctx);
            }
        }
    }

    /// Same as `notify_audio_observers` but for the paused path:
    /// every per-channel entry is zero, and the mix is zero.
    fn notify_audio_observers_paused(&mut self) {
        let global_volume = self.voices.global_volume_q();
        let amplification = self.voices.amplification_q();

        if !self.audio_channels_observers.is_empty() {
            for entry in self.per_channel_buffer.iter_mut() {
                *entry = (0, 0);
            }
            let channels: &[(i16, i16)] = self.per_channel_buffer.as_slice();
            let ctx = ChannelsContext {
                channels,
                global_volume,
                amplification,
            };
            for obs in &mut self.audio_channels_observers {
                obs.on_channels(&ctx);
            }
        }

        if !self.audio_mix_observers.is_empty() {
            let ctx = MixContext {
                left: 0,
                right: 0,
                global_volume,
                amplification,
            };
            for obs in &mut self.audio_mix_observers {
                obs.on_mix(&ctx);
            }
        }
    }

    /// Returns samples one after the other, starting with the
    /// left channel. Sample values are signed 16-bit PCM
    /// (`i16::MIN..=i16::MAX`) — the format every embedded
    /// audio codec speaks natively. Drivers that prefer a
    /// different format do the conversion themselves.
    fn sample_one(&mut self) -> Option<i16> {
        match self.right_sample {
            Some(right) => {
                self.right_sample = None;
                Some(right)
            }
            None => match self.sample(true) {
                Some((left, right)) => {
                    self.right_sample = Some(right);
                    Some(left)
                }
                None => None,
            },
        }
    }
}

impl<'a> Iterator for XmrsPlayer<'a> {
    type Item = i16;

    fn next(&mut self) -> Option<Self::Item> {
        self.sample_one()
    }
}
