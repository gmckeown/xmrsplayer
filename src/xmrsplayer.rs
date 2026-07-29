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
use crate::humanize::HumanizeMode;
use crate::midi_observer::MidiObserver;
use crate::observer::{
    ObserverList, PatternChangeContext, PlayerObserver, RowContext, TickContext,
};
use crate::sequencer::{Sequencer, TickOutcome};
use crate::voices::{Voices, DEFAULT_VOICE_POOL_CAPACITY};

pub struct XmrsPlayer<'a> {
    /// The module being played. Kept public because it's an immutable
    /// borrow supplied by the caller — `&Module` has no invariant the
    /// player owns, so there's nothing to guard.
    pub module: &'a Module,

    sequencer: Sequencer<'a>,
    voices: Voices<'a>,

    observers: ObserverList<dyn PlayerObserver + Send>,

    /// Audio observers subscribed to the final (post-sum, pre-gain) stereo
    /// mix. Called once per produced sample.
    audio_mix_observers: ObserverList<dyn MixObserver + Send>,

    /// Audio observers subscribed to the per-channel pre-sum slice. Called
    /// once per produced sample, but only if at least one is registered —
    /// the capture path in `Voices::mix` is gated on this list being
    /// non-empty to avoid paying the per-channel write cost otherwise.
    audio_channels_observers: ObserverList<dyn ChannelsObserver + Send>,

    /// MIDI observers subscribed to MIDI events emitted by the macro
    /// interpreter. Called per-event during `process_row` dispatch.
    /// Per-row frequency is very low (at most a handful of macros
    /// per row); no hot-path gating needed.
    midi_observers: ObserverList<dyn MidiObserver + Send>,

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

    /// Humanisation mode for `Track::Euclidean` pulses.
    /// Default [`crate::humanize::HumanizeMode::Disabled`] preserves cycle-exact
    /// strict playback.
    humanize_mode: HumanizeMode,
}

/// The nominal ticks-per-row of one sub-song.
///
/// `Module::default_tempo` is a single scalar and it belongs to **sub-song 0**.
/// That was harmless while every multi-sub-song format that reached the chip
/// synthesisers had a uniform cadence, and it stopped being harmless when a
/// `.sid` holding several tunes became one module: Commando's three sub-songs
/// run at 3, 4 and 3 ticks per row, and the SID driver sizes its drum burst
/// from this number.
///
/// Sub-song 0 therefore answers with the scalar itself — it is that sub-song's
/// authoritative value, and nothing here may second-guess it. Only the
/// sub-songs that have no scalar of their own are derived, from the timeline:
/// the **smallest** row gap. Not the first row's, and not the mean — a tune
/// with a *fractional* cadence alternates between `R+1` and `R+2` ticks per row
/// (the dual counter robs one tick when it wraps), and the nominal the replayer
/// was written around is the lower of the two. For a uniform sub-song every gap
/// is equal, so the minimum is just the cadence.
///
/// Keeping sub-song 0 on the scalar is not timidity, it is correctness for the
/// other formats: a tracker module that slides its speed down mid-song
/// (`F03` after starting at 6) has a smaller minimum than its initial tempo,
/// and that minimum is not what the channel state should start from.
fn nominal_tempo_of_song(module: &Module, song: usize) -> usize {
    if song == 0 {
        return module.default_tempo;
    }
    module
        .timeline_map
        .entries
        .iter()
        .filter(|e| e.song as usize == song)
        .map(|e| e.speed_at_row as usize)
        .filter(|&s| s > 0)
        .min()
        .unwrap_or(module.default_tempo)
}

impl<'a> XmrsPlayer<'a> {
    /// Build a player for the given module, with the default voice
    /// pool capacity (256 voices, matching schism's `MAX_VOICES`).
    /// For modules that stack many NNA voices simultaneously
    /// (`Another Life` and the like), use
    /// [`Self::new_with_voice_pool_capacity`] to raise this limit.
    ///
    /// Per-format runtime behaviour — FT2's arpeggio LUT and
    /// `E60` pattern-loop bug, the arpeggio period clamp,
    /// portamento-down signed overflow, `K00`-eats-note, vol-col
    /// `Bx` advancing the vibrato; IT's persistent tremor state,
    /// pan-reset policy, and tick-zero vibrato; the per-format
    /// `Sample.volume` semantics; and so on — is driven by
    /// individual flags on `module.quirks`
    /// ([`xmrs::core::compatibility::PlaybackQuirks`]). The
    /// `module.origin` field is metadata only (display, export,
    /// editor UI). Importers populate the quirks via the presets
    /// in `xmrs::tracker::profiles` (`ft2`, `it214`, `it215`,
    /// `st3`, `pt`); an editor-authored module can compose the
    /// quirks freely without lying about the source format.
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
            nominal_tempo_of_song(module, song),
            voice_pool_capacity,
        );
        let num_channels = voices.num_channels();

        Self {
            module,
            last_propagated_tempo: sequencer.tempo(),
            sequencer,
            voices,
            observers: ObserverList::new(),
            audio_mix_observers: ObserverList::new(),
            audio_channels_observers: ObserverList::new(),
            midi_observers: ObserverList::new(),
            per_channel_buffer: vec![(0, 0); num_channels],
            remaining_samples_in_tick: 0,
            sample_rate_hz: sample_rate,
            generated_samples: 0,
            right_sample: None,
            pause: false,
            song_end_notified: false,
            humanize_mode: HumanizeMode::default(),
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
        self.observers.add(observer);
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
        self.observers.count()
    }

    // --- Audio observer registration ---

    /// Register an observer on the final stereo mix (pre-gain). Called once
    /// per produced sample. See [`MixObserver`] for the contract.
    pub fn add_audio_mix_observer(&mut self, observer: Box<dyn MixObserver + Send>) {
        self.audio_mix_observers.add(observer);
    }

    /// Remove every registered mix observer.
    pub fn clear_audio_mix_observers(&mut self) {
        self.audio_mix_observers.clear();
    }

    /// Number of currently registered mix observers.
    pub fn audio_mix_observer_count(&self) -> usize {
        self.audio_mix_observers.count()
    }

    /// Register an observer on the per-channel pre-sum slice. Called once
    /// per produced sample. Registering at least one of these enables the
    /// per-channel capture path in the mixer — prefer a
    /// [`MixObserver`](Self::add_audio_mix_observer) if you only need the
    /// summed mix. See [`ChannelsObserver`] for the contract.
    pub fn add_audio_channels_observer(&mut self, observer: Box<dyn ChannelsObserver + Send>) {
        self.audio_channels_observers.add(observer);
    }

    /// Remove every registered channels observer.
    pub fn clear_audio_channels_observers(&mut self) {
        self.audio_channels_observers.clear();
    }

    /// Number of currently registered channels observers.
    pub fn audio_channels_observer_count(&self) -> usize {
        self.audio_channels_observers.count()
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
        self.midi_observers.add(observer);
    }

    /// Remove every registered MIDI observer.
    pub fn clear_midi_observers(&mut self) {
        self.midi_observers.clear();
    }

    /// Number of currently registered MIDI observers.
    pub fn midi_observer_count(&self) -> usize {
        self.midi_observers.count()
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

    /// Configure how `Track::Euclidean` pulses are
    /// humanized at playback. Default is [`crate::humanize::HumanizeMode::Disabled`]
    /// — cycle-exact strict.
    pub fn set_humanize_mode(&mut self, mode: HumanizeMode) {
        self.humanize_mode = mode;
        self.voices.set_humanize_mode(mode);
    }

    /// Currently active humanisation mode.
    pub fn humanize_mode(&self) -> HumanizeMode {
        self.humanize_mode
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

    /// Index, in `module.timeline_map.entries`, of the row currently
    /// sounding (the seek target after [`Self::seek_seconds`] /
    /// [`Self::goto_entry`]). Stable across a row's tick cycle.
    pub fn timeline_index(&self) -> usize {
        self.sequencer.timeline_idx()
    }

    // --- Global gain accessors (replace pre-0.10 `pub` fields) ---

    /// Q1.15 song-driven master volume.
    pub fn allocated_voice_count(&self) -> usize {
        self.voices.allocated_voice_count()
    }

    pub fn global_volume(&self) -> xmrs::core::fixed::units::Volume {
        self.voices.global_volume()
    }
    /// Set song-driven master volume.
    pub fn set_global_volume(&mut self, v: xmrs::core::fixed::units::Volume) {
        self.voices.set_global_volume(v);
    }

    /// Q4.12 user-driven amplification (up to 8×).
    pub fn amplification(&self) -> xmrs::core::fixed::units::Amplification {
        self.voices.amplification()
    }
    /// Set user-driven amplification.
    pub fn set_amplification(&mut self, a: xmrs::core::fixed::units::Amplification) {
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

    /// Seek to an absolute song tick. Resolves the tick through
    /// `module.timeline_map`; returns `false` when no row-start
    /// entry sits at exactly `abs_tick` for the current sub-song.
    ///
    /// Mirrors [`Self::goto`] for voices reset / tick reload /
    /// song-end clear. Useful for editor UIs whose cursor lives
    /// in the clip-tick space (`Clip.position_tick`).
    pub fn goto_tick(&mut self, abs_tick: u32) -> bool {
        if !self.sequencer.goto_tick(abs_tick) {
            return false;
        }
        self.voices.reset_for_goto();
        self.remaining_samples_in_tick = 0;
        self.song_end_notified = false;
        true
    }

    /// Jump to an explicit timeline entry by index. Like [`Self::goto`]
    /// / [`Self::goto_tick`] this RESETS voice state (no carried
    /// effect/envelope memory). The index space is
    /// `module.timeline_map.entries`; the entry must belong to the
    /// current sub-song. Unlike `goto_tick` it can target a pattern-loop
    /// body repeat (`loop_iter > 0`). Returns `false` if out of range.
    pub fn goto_entry(&mut self, entry_idx: usize) -> bool {
        if !self.sequencer.goto_entry(entry_idx) {
            return false;
        }
        self.voices.reset_for_goto();
        self.remaining_samples_in_tick = 0;
        self.song_end_notified = false;
        true
    }

    // --- Time / timeline seek ---

    /// Seconds occupied by one timeline entry (row) at its row-start
    /// tempo: `speed` ticks × `2.5 / bpm` s per tick. (Replayer ticks
    /// run at `bpm × 0.4` Hz, so one tick lasts `2.5 / bpm` s.) Uses the
    /// row-start BPM — an intra-row `Txx` BPM slide is not modelled, so
    /// this is a row-granular estimate.
    fn entry_seconds(e: &xmrs::core::daw::timeline::TimelineEntry) -> f64 {
        let bpm = (e.bpm_at_row as f64).max(1.0);
        (e.speed_at_row as f64) * 2.5 / bpm
    }

    /// Total play time of the current sub-song's timeline, in seconds,
    /// at its baked per-row tempo/BPM. For a song that loops forever via
    /// per-lane free-run (`channel_loops`) this is the length of a
    /// SINGLE pass. Row-granular estimate (intra-row BPM slides not
    /// modelled).
    pub fn duration_seconds(&self) -> f64 {
        let song = self.sequencer.current_song() as u16;
        self.module
            .timeline_map
            .entries
            .iter()
            .filter(|e| e.song == song)
            .map(Self::entry_seconds)
            .sum()
    }

    /// Current playback position within the sub-song, in seconds — the
    /// summed duration of every timeline row before the one sounding,
    /// plus the ticks elapsed inside it. Pairs with
    /// [`Self::duration_seconds`] for a progress bar, and resets to the
    /// seek target after [`Self::seek_seconds`]. (Distinct from
    /// [`Self::generated_samples`], which only ever counts forward and
    /// does not rewind on a backward seek.)
    pub fn position_seconds(&self) -> f64 {
        let song = self.sequencer.current_song() as u16;
        let cur = self.sequencer.timeline_idx();
        let mut acc = 0.0;
        for (i, e) in self.module.timeline_map.entries.iter().enumerate() {
            if i >= cur {
                break;
            }
            if e.song == song {
                acc += Self::entry_seconds(e);
            }
        }
        let tick_secs = 2.5 / (self.sequencer.bpm().max(1) as f64);
        acc + (self.sequencer.current_tick() as f64) * tick_secs
    }

    /// Map wall-clock `seconds` to the index (in `timeline_map.entries`)
    /// of the current sub-song's row that contains it, snapping DOWN to
    /// that row's start. Clamps past-the-end to the last row and
    /// negative to the first. `None` only when the sub-song has no
    /// entries.
    fn resolve_seconds_to_entry(&self, seconds: f64) -> Option<usize> {
        let song = self.sequencer.current_song() as u16;
        let target = seconds.max(0.0);
        let mut acc = 0.0;
        let mut chosen = None;
        for (i, e) in self.module.timeline_map.entries.iter().enumerate() {
            if e.song != song {
                continue;
            }
            // Remember the last seen entry so an overshoot clamps to the end.
            chosen = Some(i);
            let dur = Self::entry_seconds(e);
            if target < acc + dur {
                break; // `target` falls inside this row → snap to it
            }
            acc += dur;
        }
        chosen
    }

    /// Seek to wall-clock `seconds` into the current sub-song. Resolves
    /// the time to the timeline row that contains it (snapping DOWN to
    /// the row start, so playback always lands on a row boundary — never
    /// mid-tick) and jumps there. Times past the end clamp to the last
    /// row; negative times clamp to the start. Returns `false` only when
    /// the sub-song has no rows.
    ///
    /// Like [`Self::goto`] / [`Self::goto_tick`] this resets voice state
    /// at the landing row (no carried effect/envelope memory).
    pub fn seek_seconds(&mut self, seconds: f64) -> bool {
        match self.resolve_seconds_to_entry(seconds) {
            Some(idx) => self.goto_entry(idx),
            None => false,
        }
    }

    /// Euclidean humanisation scheduler — peek at the NEXT row
    /// and, for each armed channel, schedule a fire-early intent
    /// inside the current row's tick run.
    ///
    /// After `row_start_tick()`, `playing_pattern`/`playing_row`
    /// hold the row we just processed; `current_row` points to
    /// the next row to play *within the same pattern* (the
    /// sequencer's pending jumps for cross-pattern moves are
    /// out of scope for this lookahead — pulses that would
    /// jump patterns just don't humanise).
    fn schedule_humanize_intents(&mut self) {
        self.voices.schedule_humanize_intents(
            self.sequencer.current_song(),
            self.sequencer.playing_pattern(),
            self.sequencer.current_row(),
            self.sequencer.tempo(),
        );
    }

    // --- DW Tier-2 fidelity trace ---

    /// Advance the player by **exactly one replayer tick** for tracing,
    /// dispatching the tick's row-start / sustained-tick work (which
    /// updates every channel's period / volume / gate) **without
    /// rendering any audio samples**. Returns `false` at song end.
    ///
    /// This is the player-side counterpart of one `play_tick` call in
    /// the DW Paula oracle: forcing the sample clock to a tick boundary
    /// makes each call dispatch one — and only one — sequencer tick, so
    /// `player tick N` lines up with `golden tick N`. Pair it with
    /// [`Self::channel_snapshots`], capturing the snapshot *after* each
    /// call. Audio observers are intentionally not fired (no samples
    /// are produced). Used only by the oracle-diff tooling.
    pub fn advance_trace_tick(&mut self) -> bool {
        // Force a tick boundary so `step()` dispatches exactly one tick.
        self.remaining_samples_in_tick = 0;
        self.step()
    }

    /// Per-channel realized runtime snapshot (period / volume / gate) at
    /// the current tick — the pull-based counterpart of the `channels`
    /// slice delivered to observers. See [`crate::channel::ChannelSnapshot`]
    /// and [`Self::advance_trace_tick`].
    pub fn channel_snapshots(&self) -> Vec<crate::channel::ChannelSnapshot> {
        self.voices.channel_snapshots()
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
                        for obs in self.observers.iter_mut() {
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
                } => self.dispatch_row_start(pattern, row, pattern_changed),
                TickOutcome::Tick { tick } => self.dispatch_tick(tick),
            }

            // Propagate tempo change to voices if any global effect (or the
            // initial tempo on the very first tick) just shifted it.
            let cur_tempo = self.sequencer.tempo();
            if cur_tempo != self.last_propagated_tempo {
                self.voices.set_tempo(cur_tempo);
                self.last_propagated_tempo = cur_tempo;
            }

            let samples_per_tick_q16 = self.sequencer.samples_per_tick_q16(self.sample_rate_hz);
            self.remaining_samples_in_tick = self
                .remaining_samples_in_tick
                .saturating_add(samples_per_tick_q16 as i64);
        }
        // Decrement by one whole sample (= 1.0 in Q.16).
        self.remaining_samples_in_tick -= 1 << 16;
        true
    }

    /// Dispatch the row-start work: voices process the row, MIDI
    /// events drain to observers, then `on_pattern_change` / `on_row`
    /// fire and Euclidean lookahead schedules early fires.
    fn dispatch_row_start(&mut self, pattern: usize, row: usize, pattern_changed: bool) {
        // Fetch cells ONCE — voices and every observer share the
        // same buffer. Resolved through the DAW timeline.
        // Each entry is `(Cell, Option<usize>)` — the cell plus
        // the cached Track-level instrument.
        let song = self.sequencer.current_song();
        // Monotonic playhead: under per-lane free-run it keeps rising
        // across timeline replays so each voice keeps drifting; for
        // every tracker song it equals `current_abs_tick`. Threaded
        // identically to `row_at_with_playhead` (note content) and
        // `process_row` → `tick0` (per-lane automation fold) so the two
        // stay in lock-step.
        let abs_tick = self.sequencer.current_abs_tick_monotonic();
        let cells_vec: Vec<(xmrs::core::cell::Cell, Option<usize>)> = self
            .module
            .row_at_with_playhead(song, pattern, row, abs_tick);

        // 1. Voices (the built-in row consumer) processes the row.
        //    `abs_tick` + `song` are threaded so the Channel layer
        //    can resolve its active Clip → Track and read the
        //    extracted AutomationLanes.
        self.voices.process_row(&cells_vec, abs_tick, song as u16);

        // 1b. Dispatch any MIDI events the macro interpreter
        //     produced during the row. Done here (not per-tick)
        //     because macros only fire at row-start. The buffer
        //     is cleared at the next `process_row`, so we skip
        //     the drain entirely when no one is listening.
        if !self.midi_observers.is_empty() {
            for (src_ch, event) in self.voices.drain_midi_events() {
                for obs in self.midi_observers.iter_mut() {
                    obs.on_midi_event(&event, src_ch);
                }
            }
        }

        // 2. User observers: on_pattern_change first (if
        //    applicable), then on_row.
        if pattern_changed && !self.observers.is_empty() {
            let pc_ctx = PatternChangeContext {
                module: self.module,
                song,
                new_table_index: self.sequencer.current_table_index(),
                new_pattern: pattern,
            };
            for obs in self.observers.iter_mut() {
                obs.on_pattern_change(&pc_ctx);
            }
        }

        if !self.observers.is_empty() {
            // Observers consume `&[Cell]` directly — they don't
            // need the per-channel track instrument that the
            // audio path threads through. Materialise a
            // contiguous slice by stripping the tuple's second
            // field.
            let cells_obs: Vec<xmrs::core::cell::Cell> =
                cells_vec.iter().map(|(c, _)| c.clone()).collect();
            // Realized per-channel state after the row was processed.
            let channels = self.voices.channel_snapshots();
            let ctx = RowContext {
                module: self.module,
                song,
                pattern,
                row,
                cells: &cells_obs,
                channels: &channels,
                tempo: self.sequencer.tempo(),
                bpm: self.sequencer.bpm(),
            };
            for obs in self.observers.iter_mut() {
                obs.on_row(&ctx);
            }
        }

        // Euclidean humanisation lookahead: after processing the
        // current row, peek at the next row to see whether any
        // armed channel can fire its next pulse early.
        if self.humanize_mode != HumanizeMode::Disabled {
            self.schedule_humanize_intents();
        }
    }

    /// Dispatch the per-tick work: Euclidean early-fire, voices
    /// per-tick effects, then `on_tick` for subscribing observers.
    fn dispatch_tick(&mut self, tick: usize) {
        // Euclidean humanisation early-fire: consume any
        // scheduled fire-intents whose tick matches.
        if self.humanize_mode != HumanizeMode::Disabled {
            let abs_tick = self
                .sequencer
                .current_abs_tick_monotonic()
                .saturating_add(tick as u32);
            let song = self.sequencer.current_song() as u16;
            self.voices
                .fire_humanize_intents_at(tick as u8, abs_tick, song);
        }
        // 1. Voices run per-tick effects.
        self.voices.process_tick(tick);

        // 2. Observers that opt in get an on_tick callback. Walk
        //    the list twice only when at least one subscribes —
        //    a cheap short-circuit when the common case (UI
        //    observers on rows only) dominates.
        if self.observers.iter().any(|obs| obs.subscribes_to_ticks()) {
            let pat = self.sequencer.playing_pattern();
            let row = self.sequencer.playing_row();
            let song = self.sequencer.current_song();
            // Same monotonic playhead the voices used, so observers see
            // the folded (looping) content, not the un-looped row.
            let abs_tick = self.sequencer.current_abs_tick_monotonic();
            let cells_vec: Vec<(xmrs::core::cell::Cell, Option<usize>)> =
                self.module.row_at_with_playhead(song, pat, row, abs_tick);
            let cells_obs: Vec<xmrs::core::cell::Cell> =
                cells_vec.iter().map(|(c, _)| c.clone()).collect();
            let channels = self.voices.channel_snapshots();
            let ctx = TickContext {
                module: self.module,
                song,
                pattern: pat,
                row,
                tick,
                cells: &cells_obs,
                channels: &channels,
            };
            for obs in self.observers.iter_mut() {
                if obs.subscribes_to_ticks() {
                    obs.on_tick(&ctx);
                }
            }
        }
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
    pub fn samples_apply_volume(&self, samples: &[(i16, i16)]) -> (i16, i16) {
        let (l, r) = self.samples_to_sample(samples);
        self.voices.apply_final_gain((l as i32, r as i32))
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

        // RFC §3C: audio clips are positioned by the current playback
        // tick + sub-song. Constant across a tick's samples; the
        // per-clip cursor gives sample accuracy inside the tick.
        let audio_tick = self.sequencer.current_abs_tick_monotonic();
        let audio_song = self.current_song() as u16;

        let capture = !self.audio_channels_observers.is_empty();
        let mix_pre_gain_i32 = if capture {
            let buf = self.per_channel_buffer.as_mut_slice();
            self.voices.mix(audio_tick, audio_song, Some(buf))
        } else {
            self.voices.mix(audio_tick, audio_song, None)
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
        let global_volume = self.voices.global_volume();
        let amplification = self.voices.amplification();

        if !self.audio_channels_observers.is_empty() {
            let channels: &[(i16, i16)] = self.per_channel_buffer.as_slice();
            let ctx = ChannelsContext {
                channels,
                global_volume,
                amplification,
            };
            for obs in self.audio_channels_observers.iter_mut() {
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
            for obs in self.audio_mix_observers.iter_mut() {
                obs.on_mix(&ctx);
            }
        }
    }

    /// Same as `notify_audio_observers` but for the paused path:
    /// every per-channel entry is zero, and the mix is zero.
    fn notify_audio_observers_paused(&mut self) {
        if !self.audio_channels_observers.is_empty() {
            for entry in self.per_channel_buffer.iter_mut() {
                *entry = (0, 0);
            }
        }
        self.notify_audio_observers((0, 0));
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

#[cfg(all(test, feature = "import_xm"))]
mod seek_tests {
    use super::*;

    const SR: u32 = 48_000;

    fn note_module() -> Module {
        let data = include_bytes!("../../xmrs/examples/note.xm");
        Module::load_xm(data).expect("load note.xm")
    }

    /// Cumulative start time (s) of every entry of song 0, mirroring the
    /// player's `entry_seconds` math, for assertions.
    fn entry_start_times(module: &Module) -> Vec<f64> {
        let mut acc = 0.0;
        let mut out = Vec::new();
        for e in module.timeline_map.entries.iter().filter(|e| e.song == 0) {
            out.push(acc);
            let bpm = (e.bpm_at_row as f64).max(1.0);
            acc += (e.speed_at_row as f64) * 2.5 / bpm;
        }
        out
    }

    #[test]
    fn duration_and_zero_position() {
        let module = note_module();
        let mut p = XmrsPlayer::new(&module, SR, 0);
        assert!(p.duration_seconds() > 0.0, "song should have a duration");
        assert!(p.seek_seconds(0.0), "seek to 0 should succeed");
        assert!(p.position_seconds().abs() < 1e-9, "position at 0 s");
    }

    /// Resolving a time in the middle of a row snaps the position back to
    /// that row's start — playback lands on a row boundary, never mid-tick.
    #[test]
    fn seek_snaps_to_row_start() {
        let module = note_module();
        let starts = entry_start_times(&module);
        if starts.len() < 3 {
            return;
        }
        for i in 0..starts.len() {
            let row_dur = if i + 1 < starts.len() {
                starts[i + 1] - starts[i]
            } else {
                0.01
            };
            let mid = starts[i] + row_dur * 0.5;
            let mut p = XmrsPlayer::new(&module, SR, 0);
            assert!(p.seek_seconds(mid), "seek should succeed (i={i})");
            assert_eq!(p.timeline_index(), i, "snapped to containing row");
            assert!(
                (p.position_seconds() - starts[i]).abs() < 1e-6,
                "position == row start (i={i})",
            );
        }
    }

    /// Times beyond the end clamp to the last row; negatives clamp to the
    /// first.
    #[test]
    fn seek_clamps_out_of_range() {
        let module = note_module();
        let starts = entry_start_times(&module);
        if starts.is_empty() {
            return;
        }
        let last = starts.len() - 1;
        let mut p = XmrsPlayer::new(&module, SR, 0);
        assert!(p.seek_seconds(p.duration_seconds() * 4.0));
        assert_eq!(p.timeline_index(), last, "overshoot clamps to last row");
        assert!(p.seek_seconds(-10.0));
        assert_eq!(p.timeline_index(), 0, "negative clamps to first row");
    }

    /// The seek lands on the target row and resets voice state: nothing is
    /// allocated until the target row is actually dispatched on the next
    /// sample (same contract as `goto` / `goto_tick`).
    #[test]
    fn seek_lands_and_resets_voices() {
        let module = note_module();
        let n = module
            .timeline_map
            .entries
            .iter()
            .filter(|e| e.song == 0)
            .count();
        if n < 4 {
            return;
        }
        let target_idx = n / 2;
        let mut p = XmrsPlayer::new(&module, SR, 0);
        assert!(p.goto_entry(target_idx));
        assert_eq!(p.timeline_index(), target_idx);
        assert_eq!(p.allocated_voice_count(), 0, "seek resets voices");
    }

    /// `position_seconds` reflects the ticks elapsed inside the current
    /// row, not just the row start: one sample past a row boundary lands
    /// strictly between this row's start and the next row's start. Guards
    /// the `current_tick * (2.5 / bpm)` term (invisible at row start where
    /// `current_tick == 0`).
    #[test]
    fn position_seconds_advances_within_a_row() {
        let module = note_module();
        let entries: Vec<_> = module
            .timeline_map
            .entries
            .iter()
            .filter(|e| e.song == 0)
            .cloned()
            .collect();
        if entries.len() < 3 {
            return;
        }
        let starts = entry_start_times(&module);
        // A non-edge row with more than one tick, so a single sample leaves
        // us strictly inside it.
        let i = match (1..entries.len() - 1).find(|&i| entries[i].speed_at_row > 1) {
            Some(i) => i,
            None => return,
        };
        let mut p = XmrsPlayer::new(&module, SR, 0);
        assert!(p.seek_seconds(starts[i]));
        assert!(
            (p.position_seconds() - starts[i]).abs() < 1e-9,
            "exactly at row start before any sample",
        );
        assert!(p.sample(true).is_some());
        assert_eq!(p.timeline_index(), i, "still inside row {i}");
        let pos = p.position_seconds();
        assert!(
            pos > starts[i],
            "advanced past row start ({pos} > {})",
            starts[i]
        );
        assert!(
            pos < starts[i + 1],
            "still before the next row ({pos} < {})",
            starts[i + 1],
        );
    }

    /// Seeking to a time that falls exactly on a row boundary lands on
    /// THAT row (the snap is `<`, not `<=`): `starts[k]` → row `k`.
    #[test]
    fn seek_to_exact_row_boundary_lands_on_that_row() {
        let module = note_module();
        let starts = entry_start_times(&module);
        if starts.len() < 3 {
            return;
        }
        for (k, &start) in starts.iter().enumerate().skip(1) {
            let mut p = XmrsPlayer::new(&module, SR, 0);
            assert!(p.seek_seconds(start));
            assert_eq!(
                p.timeline_index(),
                k,
                "exact boundary time lands on row {k}"
            );
        }
    }
}
