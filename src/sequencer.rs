//! Song cursor + tick clock.
//!
//! The `Sequencer` owns everything that has to do with *where we are in the
//! song* and *when the next row fires*:
//!
//! * song/pattern/row position and the pending-jump flags that the navigation
//!   effects (`Bxx` position jump, `Dxx` pattern break, `E6y` pattern loop,
//!   `EEy` pattern delay) manipulate,
//! * `tempo` / `bpm` / `current_tick` — the song-side clock, expressed in
//!   ticks per row and ticks per second (the sample clock itself lives on
//!   the facade),
//! * the per-channel pattern-loop state (`pattern_loop_origin`,
//!   `pattern_loop_count`) — it reads as "per channel" but semantically
//!   belongs to navigation, so it migrates here from `Channel`,
//! * the loop-detection bookkeeping (`row_loop_count`, `loop_count`,
//!   `max_loop_count`) used to stop playback after N loops.
//!
//! The sequencer does **not** touch audio. It reads `GlobalEffect`s from the
//! module's pattern cells (directly, without going through `Channel`) and
//! applies the ones that affect navigation/tempo. Volume-side global effects
//! (`GlobalEffect::Volume`, `GlobalEffect::VolumeSlide`) are read separately
//! by `Voices`.
//!
//! The facade drives the sequencer one tick at a time via [`Sequencer::advance_tick`].

use alloc::{vec, vec::Vec};
use xmrs::prelude::*;

/// Per-channel pattern-loop state (FT2 `E6y` / S3M `SBy` / IT `SBy`).
///
/// `origin` is the row the loop starts from (set by `E60` on that channel),
/// `count` is how many times the loop body has run so far.
#[derive(Clone, Default)]
struct PatternLoopSlot {
    origin: usize,
    count: usize,
}

/// Outcome of a single tick advance.
#[derive(Debug, Clone, Copy)]
pub enum TickOutcome {
    /// A regular (non-row-start) tick. `tick` is the tick index inside the
    /// current row (>= 1 — row-start ticks use `RowStart` instead).
    /// Voices should run their per-tick effect logic using this value.
    Tick { tick: usize },
    /// A row-start tick. The sequencer has already applied any pending
    /// navigation jumps and consumed the new row's navigation global effects.
    /// `pattern` is the pattern index in `module.pattern`, `row` is the row
    /// index within that pattern. The caller is expected to forward the
    /// cells at `module.pattern[pattern][row]` to voices and to observers.
    ///
    /// `pattern_changed` is `true` when this row-start tick also crossed a
    /// pattern-order boundary (either naturally or via a jump); observers
    /// may want to refresh pattern-level UI on that event.
    RowStart {
        pattern: usize,
        row: usize,
        pattern_changed: bool,
    },
    /// Playback has ended (max loop count reached). No more events will fire
    /// until the caller resets the sequencer (e.g. via `goto`).
    SongEnd,
}

pub struct Sequencer<'a> {
    module: &'a Module,
    current_song: usize,

    // --- Clock (song-side) ---
    tempo: usize,
    bpm: usize,
    /// Current tick inside the current row (0 = row-start tick, 1..tempo-1 =
    /// sustained-effect ticks, wraps back to 0 to load the next row).
    current_tick: usize,
    /// Extra ticks before advancing to the next row — used for `EEy`
    /// pattern-delay. Consumed once per row.
    extra_ticks: usize,
    /// IT `Txx` BPM slide: the signed delta latched on the last row
    /// that carried a `GlobalEffect::BpmSlide`. Applied on every
    /// subsequent non-row-start tick until the next row either
    /// re-latches or clears it. Zero when inactive.
    bpm_slide_delta: isize,

    // --- Cursor ---
    current_table_index: usize,
    /// Row cursor. Following FT2 historical behaviour this is incremented at
    /// the END of the row-start tick, so between row-start ticks it points to
    /// the *next* row to play rather than to the row currently sounding.
    /// `Sequencer::playing_row()` gives the opposite view when needed.
    current_row: usize,
    /// Last row reported to the caller via [`TickOutcome::RowStart`] — kept
    /// separate from `current_row` because `current_row` gets incremented
    /// before the outcome is returned.
    playing_row: usize,
    /// Same, but for the pattern index.
    playing_pattern: usize,

    // --- Pending jumps (set by navigation effects on a row, consumed at the
    //     start of the next row-start tick). ---
    position_jump: bool,
    pattern_break: bool,
    jump_dest: usize,
    jump_row: usize,

    // --- Per-channel pattern-loop state (E6y / SBy). ---
    pattern_loop: Vec<PatternLoopSlot>,

    // --- Loop-detection bookkeeping for `max_loop_count`. ---
    row_loop_count: Vec<Vec<usize>>,
    loop_count: usize,
    max_loop_count: usize,
}

impl<'a> Sequencer<'a> {
    /// Build a sequencer for the given module.
    ///
    /// `song` is clamped to a valid index into `module.pattern_order` (0 is
    /// used if out of range), matching the constructor's historical contract.
    pub(crate) fn new(module: &'a Module, song: usize) -> Self {
        let current_song = if song < module.pattern_order.len() {
            song
        } else {
            0
        };
        let num_channels = module.get_num_channels();
        let song_len = module.get_song_length(current_song);

        Self {
            module,
            current_song,
            tempo: module.default_tempo,
            bpm: module.default_bpm,
            current_tick: 0,
            extra_ticks: 0,
            bpm_slide_delta: 0,
            current_table_index: 0,
            current_row: 0,
            playing_row: 0,
            playing_pattern: module.pattern_order[current_song]
                .first()
                .copied()
                .unwrap_or(0),
            position_jump: false,
            pattern_break: false,
            jump_dest: 0,
            jump_row: 0,
            pattern_loop: vec![PatternLoopSlot::default(); num_channels],
            row_loop_count: vec![vec![0; MAX_NUM_ROWS]; song_len],
            loop_count: 0,
            max_loop_count: 0,
        }
    }

    // --- Accessors ---

    pub fn current_song(&self) -> usize {
        self.current_song
    }
    pub fn tempo(&self) -> usize {
        self.tempo
    }
    pub fn bpm(&self) -> usize {
        self.bpm
    }
    pub fn current_tick(&self) -> usize {
        self.current_tick
    }
    pub fn current_table_index(&self) -> usize {
        self.current_table_index
    }
    /// Row index of the *next* row to play (matches historical
    /// `get_current_row()` semantics — there is a +1 bias relative to the
    /// row currently sounding).
    pub fn current_row(&self) -> usize {
        self.current_row
    }
    /// Pattern index (into `module.pattern`) that the current table cursor
    /// points to.
    pub fn current_pattern(&self) -> usize {
        self.module.pattern_order[self.current_song][self.current_table_index]
    }
    /// Row that is *currently sounding*, i.e. the row most recently loaded by
    /// a [`TickOutcome::RowStart`]. Stable throughout the row's tick cycle.
    pub fn playing_row(&self) -> usize {
        self.playing_row
    }
    /// Pattern of the currently sounding row.
    pub fn playing_pattern(&self) -> usize {
        self.playing_pattern
    }
    pub fn loop_count(&self) -> usize {
        self.loop_count
    }
    pub fn max_loop_count(&self) -> usize {
        self.max_loop_count
    }
    pub fn set_max_loop_count(&mut self, v: usize) {
        self.max_loop_count = v;
    }
    pub fn num_channels(&self) -> usize {
        self.pattern_loop.len()
    }

    /// Ticks-per-second at the current BPM. FT2 manual: `tps = BPM * 0.4`.
    /// Useful to compute "samples per tick" on the facade side
    /// (`sample_rate / tps`).
    pub fn ticks_per_second(&self) -> f32 {
        self.bpm as f32 * 0.4
    }

    // --- Cursor manipulation ---

    /// Jump to `(table_position, row)` at `speed`; if `speed == 0`, reset to
    /// the module's default tempo. Returns `false` if the target is
    /// out of range.
    ///
    /// Semantically identical to the old `XmrsPlayer::goto`:
    /// the cursor is scheduled to jump on the *next* tick boundary (as if a
    /// `Bxx` had been applied), tempo and BPM are reset, and the row counter
    /// is forced to fire `tick0` on the next step.
    pub(crate) fn goto(&mut self, table_position: usize, row: usize, speed: usize) -> bool {
        if table_position >= self.module.get_song_length(self.current_song) {
            return false;
        }
        let num_row = self.module.pattern_order[self.current_song][table_position];
        if row >= self.module.get_num_rows(num_row) {
            return false;
        }

        self.jump_dest = table_position;
        self.jump_row = row;
        self.position_jump = true;

        self.tempo = if speed == 0 {
            self.module.default_tempo
        } else {
            speed
        };
        self.bpm = self.module.default_bpm;
        self.bpm_slide_delta = 0;

        // Force the next advance_tick to be treated as a row-start tick.
        self.current_tick = 0;

        true
    }

    // --- Tick advance (driven by the facade) ---

    /// Advance the sequencer by one song tick. The facade is responsible for
    /// computing the sample-clock gap between ticks — here we just bump the
    /// song-side tick counter and, when appropriate, apply row-change logic.
    ///
    /// The returned outcome tells the caller whether this tick was a
    /// row-start (meaning cells must be dispatched to voices + observers) or
    /// a regular sustained tick.
    pub(crate) fn advance_tick(&mut self) -> TickOutcome {
        if self.is_exhausted() {
            return TickOutcome::SongEnd;
        }

        if self.current_tick == 0 {
            let outcome = self.row_start_tick();
            // Re-check exhaustion — `row_start_tick` updates `loop_count`.
            if self.is_exhausted() {
                return TickOutcome::SongEnd;
            }
            self.advance_tick_counter();
            outcome
        } else {
            // Capture the tick number BEFORE advancing the counter — voices
            // and observers need the value of the tick they're currently
            // processing, not the next one.
            let tick_processed = self.current_tick;

            // Apply the per-tick BPM slide if one's latched. Each
            // non-row-start tick bumps the BPM by the stored delta,
            // clamped to the tracker-legal range. When the next row
            // arrives with no BpmSlide the latch is cleared in
            // `apply_row_navigation_effects` — until then, the slide
            // keeps ramping.
            if self.bpm_slide_delta != 0 {
                let new_bpm = (self.bpm as isize + self.bpm_slide_delta).clamp(32, 255) as usize;
                self.bpm = new_bpm;
            }

            self.advance_tick_counter();
            TickOutcome::Tick {
                tick: tick_processed,
            }
        }
    }

    fn is_exhausted(&self) -> bool {
        self.max_loop_count > 0 && self.loop_count >= self.max_loop_count
    }

    /// Core of the row-start tick: apply pending jumps, consume the row's
    /// navigation global effects, snapshot `(playing_pattern, playing_row)`,
    /// update the loop counter, then advance `current_row` for next time.
    fn row_start_tick(&mut self) -> TickOutcome {
        let mut pattern_changed = false;

        // 1. Apply any pending jump/break from the previous row.
        if self.position_jump {
            self.current_table_index = self.jump_dest;
            self.current_row = self.jump_row;
            self.position_jump = false;
            self.pattern_break = false;
            self.jump_row = 0;
            self.post_pattern_change();
            pattern_changed = true;
        } else if self.pattern_break {
            self.current_table_index += 1;
            self.current_row = self.jump_row;
            self.pattern_break = false;
            self.jump_row = 0;
            self.post_pattern_change();
            pattern_changed = true;
        }

        // 2. Resolve pattern_idx, guarding against an out-of-range order entry
        //    (historical behaviour: fall back to table index 0).
        let pat_idx = {
            let raw = self.module.pattern_order[self.current_song][self.current_table_index];
            if raw < self.module.pattern.len() {
                raw
            } else {
                self.current_table_index = 0;
                self.module.pattern_order[self.current_song][self.current_table_index]
            }
        };

        let current_row = self.current_row;

        // 3. Consume the row's navigation global effects (across all channels).
        let in_a_loop = self.apply_row_navigation_effects(pat_idx, current_row);

        // 4. Loop-detection counter. Skip the increment when an E6y loop is
        //    currently active on at least one channel (FT2: `in_a_loop`
        //    suppresses max_loop_count interference).
        if !in_a_loop {
            self.loop_count = self.row_loop_count[self.current_table_index][current_row];
            self.row_loop_count[self.current_table_index][current_row] += 1;
        }

        // 5. Snapshot "playing" position so observers/voices see a stable
        //    (pattern, row) while `current_row` is about to be advanced.
        self.playing_pattern = pat_idx;
        self.playing_row = current_row;

        // 6. Advance `current_row`, handle natural end-of-pattern.
        //
        // `current_row` is a `usize`, so the wrap-to-0 branch below cannot
        // fire in practice (pattern lengths are bounded well below usize::MAX).
        // A previous comment speculated about shrinking this field to `u8`
        // and wanted the wrap to push us into the next pattern; if that ever
        // happens, reintroduce the `== 0` guard in the condition below.
        self.current_row += 1;
        let pattern_len = self.module.pattern[pat_idx].len();
        if !self.position_jump && !self.pattern_break && self.current_row >= pattern_len {
            self.current_table_index += 1;
            self.current_row = self.jump_row; /* usually 0, except when E60 set jump_row */
            self.jump_row = 0;
            self.post_pattern_change();
            pattern_changed = true;
        }

        TickOutcome::RowStart {
            pattern: pat_idx,
            row: current_row,
            pattern_changed,
        }
    }

    /// Iterate the row's cells and apply every navigation-side global effect:
    /// `Speed` (tempo), `Bpm` / `BpmSlide`, `PatternBreak`, `PositionJump`,
    /// `PatternLoop`, `PatternDelay`, and the MIDI-macro stub.
    ///
    /// Volume-side effects (`GlobalEffect::Volume`, `GlobalEffect::VolumeSlide`)
    /// are handled by [`crate::voices::Voices`] — we leave them alone here.
    ///
    /// Returns `true` if at least one channel is currently inside an E6y
    /// pattern-loop body (used to gate the max-loop-count increment).
    fn apply_row_navigation_effects(&mut self, pat_idx: usize, row: usize) -> bool {
        let num_channels = self.pattern_loop.len();
        let mut in_a_loop = false;

        // Reset the BPM-slide latch before processing the new row.
        // A slide is only active on the row that carries it; the
        // next row either re-latches a new delta or stays silent.
        self.bpm_slide_delta = 0;

        for ch_index in 0..num_channels {
            // Defensive indexing: some malformed modules have short rows
            // relative to their declared channel count.
            let cell = match self.module.pattern[pat_idx][row].get(ch_index) {
                Some(c) => c,
                None => continue,
            };

            // `pattern_slot.global_effects` is a Vec on TrackUnit; we clone
            // it as the current implementation does (the vector is short,
            // typically 0..2 entries) so we can mutate `self` freely.
            for gfx in cell.global_effects.clone() {
                match gfx {
                    GlobalEffect::Bpm(bpm) => {
                        self.bpm = bpm;
                    }
                    GlobalEffect::BpmSlide(delta) => {
                        // IT Txy with x=0 is "slide BPM down by y", x=1
                        // is "slide BPM up by y". The xmrs importer
                        // normalises both into a single signed delta
                        // (positive = up). The slide fires on every
                        // non-row-start tick of the row; here we latch
                        // the delta, and `apply_per_tick_effects`
                        // applies it each tick.
                        //
                        // Note the latch is also reset just above the
                        // `for ch_index` loop (at row-start), so a
                        // slide on row N doesn't bleed into row N+1.
                        self.bpm_slide_delta = delta;
                    }
                    GlobalEffect::MidiMacro(_m) => {
                        // MIDI macros are per-channel-scoped despite
                        // the `GlobalEffect` typing. `Voices` picks
                        // them up in `apply_row_global_effects` with
                        // the channel index in hand — see
                        // `Channel::apply_midi_macro`. The sequencer
                        // ignores them here.
                    }
                    GlobalEffect::PatternBreak(position) => {
                        self.pattern_break = true;
                        self.jump_row = position;
                    }
                    GlobalEffect::PatternDelay {
                        quantity: q,
                        tempo: t,
                    } => self.extra_ticks = if t { q * self.tempo } else { q },
                    GlobalEffect::PatternLoop(value) => {
                        let slot = &mut self.pattern_loop[ch_index];
                        if value != 0 {
                            if value == slot.count {
                                // Loop is over.
                                slot.count = 0;
                                if self.module.profile.quirks.pattern_loop_resumes {
                                    // ST3 `s_patloop` (digcmd.c:1003):
                                    // when the loop finishes, the next
                                    // row becomes the new loop start.
                                    slot.origin = self.current_row + 1;
                                }
                            } else {
                                // Jump to the beginning of the loop.
                                slot.count += 1;
                                self.position_jump = true;
                                self.jump_row = slot.origin;
                                self.jump_dest = self.current_table_index;
                            }
                        } else {
                            // Set loop start point (E60 / SB0).
                            slot.origin = self.current_row;
                            if self.module.profile.quirks.e60_leaks_to_next_pattern {
                                // Replicate FT2 E60 bug: in FT2, E60 not
                                // only sets the per-channel loop origin,
                                // it ALSO leaks that row number into the
                                // song's pBreakPos. When the pattern then
                                // ends naturally, the next pattern starts
                                // at pBreakPos instead of row 0.
                                self.jump_row = slot.origin;
                            }
                        }
                    }
                    GlobalEffect::PositionJump(position) => {
                        if position < self.module.pattern_order[self.current_song].len() {
                            self.position_jump = true;
                            self.jump_dest = position;
                            self.jump_row = 0;
                        }
                    }
                    GlobalEffect::Speed(speed) => {
                        self.tempo = speed;
                    }
                    GlobalEffect::Volume(_) | GlobalEffect::VolumeSlide { .. } => {
                        // Volume-side effects belong to `Voices`, not here.
                    }
                }
            }

            if self.pattern_loop[ch_index].count > 0 {
                in_a_loop = true;
            }
        }

        in_a_loop
    }

    fn post_pattern_change(&mut self) {
        if self.current_table_index >= self.module.pattern_order[self.current_song].len() {
            self.current_table_index = self.module.restart_position;
        }
    }

    fn advance_tick_counter(&mut self) {
        self.current_tick += 1;
        // `F00` (speed = 0) halts row advance: tick() keeps firing so
        // sustained effects continue, but tick0() never runs again
        // until the tempo is set to a non-zero value.
        if self.tempo != 0 && self.current_tick >= self.tempo + self.extra_ticks {
            self.current_tick = 0;
            self.extra_ticks = 0;
        }
    }
}

// Tests for this module are intentionally omitted from this initial split —
// the `Sequencer` is now fully decoupled from audio, so unit tests can drive
// `advance_tick()` against synthetic `Module` instances without instantiating
// any channel/voice machinery. See `OBSERVERS.md` for the testing story.
