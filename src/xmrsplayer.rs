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

use crate::observer::{PatternChangeContext, PlayerObserver, RowContext, TickContext};
use crate::sequencer::{Sequencer, TickOutcome};
use crate::voices::Voices;

pub struct XmrsPlayer<'a> {
    /// The module being played. Kept public because it's an immutable
    /// borrow supplied by the caller — `&Module` has no invariant the
    /// player owns, so there's nothing to guard.
    pub module: &'a Module,

    sequencer: Sequencer<'a>,
    voices: Voices<'a>,

    observers: Vec<Box<dyn PlayerObserver + Send>>,

    /// Facade-owned sample clock: fractional samples remaining before the
    /// next song tick boundary. Decremented once per sample; when it
    /// reaches 0 we step the sequencer.
    remaining_samples_in_tick: f32,
    sample_rate: f32,

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
    right_sample: Option<f32>,

    pause: bool,

    /// `on_song_end` must fire at most once per song-termination event.
    song_end_notified: bool,
}

impl<'a> XmrsPlayer<'a> {
    /// Build a player for the given module.
    ///
    /// Format-specific FT2/XM behaviour — arpeggio LUT, E60 pattern-
    /// loop bug, arpeggio period clamp, portamento-down signed-
    /// overflow, K00-eats-note, vol-col `Bx` advancing the vibrato —
    /// is driven entirely by `module.format`. `ModuleFormat::Xm`
    /// activates the canonical FT2 reference behaviour, all other
    /// formats use their own native semantics.
    pub fn new(module: &'a Module, sample_rate: f32, song: usize) -> Self {
        let sequencer = Sequencer::new(module, song);
        let voices = Voices::new(module, sample_rate, module.default_tempo);

        Self {
            module,
            last_propagated_tempo: sequencer.tempo(),
            sequencer,
            voices,
            observers: Vec::new(),
            remaining_samples_in_tick: 0.0,
            sample_rate,
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

    pub fn get_sample_rate(&self) -> f32 {
        self.sample_rate
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

    pub fn global_volume(&self) -> f32 {
        self.voices.global_volume()
    }

    pub fn set_global_volume(&mut self, v: f32) {
        self.voices.set_global_volume(v);
    }

    pub fn amplification(&self) -> f32 {
        self.voices.amplification()
    }

    pub fn set_amplification(&mut self, a: f32) {
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
        self.remaining_samples_in_tick = 0.0;
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
        if self.remaining_samples_in_tick <= 0.0 {
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

            /* FT2 manual: number of ticks / second = BPM * 0.4 */
            self.remaining_samples_in_tick += self.sample_rate / self.sequencer.ticks_per_second();
        }
        self.remaining_samples_in_tick -= 1.0;
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
    pub fn samples_from_channels(&mut self) -> Option<Vec<(f32, f32)>> {
        if self.pause {
            return Some(vec![(0.0, 0.0); self.voices.num_channels()]);
        }
        if !self.step() {
            return None;
        }
        let samples = self.voices.samples_from_channels();
        self.generated_samples += 1;
        Some(samples)
    }

    /// Sum per-channel samples into a stereo pair, without gain.
    pub fn samples_to_sample(&self, samples: &[(f32, f32)]) -> (f32, f32) {
        samples
            .iter()
            .fold((0.0, 0.0), |(acc_l, acc_r), (l, r)| (acc_l + l, acc_r + r))
    }

    /// Apply `global_volume * amplification` to a per-channel sample set and
    /// fold to a single stereo pair. Intended to be paired with
    /// [`samples_from_channels`](Self::samples_from_channels).
    pub fn samples_apply_volume(&self, samples: &[(f32, f32)]) -> (f32, f32) {
        let g = self.voices.global_volume() * self.voices.amplification();
        let (l, r) = self.samples_to_sample(samples);
        (l * g, r * g)
    }

    /// Return the sum of the samples from all channels, optionally scaled by
    /// `global_volume * amplification`. This is the hot path used by the
    /// `Iterator` impl.
    ///
    /// Semantically identical to `samples_from_channels().map(samples_apply_volume)`,
    /// but walks channels once with an `(f32, f32)` accumulator and never
    /// allocates a per-sample `Vec`.
    pub fn sample(&mut self, apply_volume: bool) -> Option<(f32, f32)> {
        if self.pause {
            return Some((0.0, 0.0));
        }
        if !self.step() {
            return None;
        }
        let s = self.voices.mix(apply_volume);
        self.generated_samples += 1;
        Some(s)
    }

    /// Returns samples one after the other, starting with the left channel.
    fn sample_one(&mut self) -> Option<f32> {
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
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        self.sample_one()
    }
}
