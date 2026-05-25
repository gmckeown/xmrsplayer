//! Song cursor + tick clock — TimelineMap-driven.
//!
//! Since DAW migration Phase 3c.2 the sequencer is a thin
//! state machine over [`xmrs::core::daw::timeline::TimelineMap`]:
//! each `TimelineEntry` already carries `(order, pattern, row,
//! loop_iter, tick, speed_at_row, bpm_at_row)`, with all navigation
//! effects (`Bxx` / `Dxx` / `E6y` / `EEy`) absorbed at import time
//! by the linearisation walker. The sequencer's only job at
//! row-start is to:
//!
//! * advance `timeline_idx` (either linearly, to entry+1, or via
//!   the end-of-song wrap dictated by [`xmrs::core::module::Module::song_loop_to`]),
//! * publish the new `(playing_pattern, playing_row, tempo, bpm)`
//!   to observers and voices,
//! * latch `BpmSlide` from the row's cells so non-row-start ticks
//!   can ramp the BPM.
//!
//! Everything else (`Bpm`/`Speed`/`Volume`/`VolumeSlide` cell-side
//! globals, `MidiMacro`) is either pre-baked into the entry (`Bpm`,
//! `Speed`) or read separately by [`crate::voices::Voices`]
//! (`Volume`, `VolumeSlide`, `MidiMacro`).
//!
//! The sequencer does **not** touch audio. The facade drives it
//! one tick at a time via `Sequencer::advance_tick` (crate-private).

use alloc::{vec, vec::Vec};
use xmrs::prelude::*;

/// Outcome of a single tick advance.
#[derive(Debug, Clone, Copy)]
pub enum TickOutcome {
    /// A regular (non-row-start) tick. `tick` is the tick index inside the
    /// current row (>= 1 — row-start ticks use `RowStart` instead).
    /// Voices should run their per-tick effect logic using this value.
    Tick { tick: usize },
    /// A row-start tick. `pattern` is the pattern index, `row` is the row
    /// index within that pattern. The caller is expected to forward
    /// `module.row_at(song, pattern, row)` to voices and to observers.
    ///
    /// `pattern_changed` is `true` when this row-start tick also crossed a
    /// pattern-order boundary (either naturally or via a jump); observers
    /// may want to refresh pattern-level UI on that event.
    RowStart {
        pattern: usize,
        row: usize,
        pattern_changed: bool,
    },
    /// Playback has ended (max loop count reached, or `TimelineMap`
    /// exhausted with no wrap target). No more events will fire
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
    /// IT `Txx` BPM slide: the signed delta latched on the last row
    /// that carried a `SongLevelEffect::BpmSlide`. Applied on every
    /// subsequent non-row-start tick until the next row either
    /// re-latches or clears it. Zero when inactive.
    bpm_slide_delta: isize,

    // --- Cursor (kept in sync with `timeline_idx` for legacy accessors) ---
    current_table_index: usize,
    /// Row cursor. Following FT2 historical behaviour this is the
    /// row *after* the one currently sounding (+1 bias). When the
    /// row currently sounding is the last visited row of its
    /// pattern, this points one past the end of the pattern.
    current_row: usize,
    /// Last row reported to the caller via [`TickOutcome::RowStart`].
    /// Stable throughout the row's tick cycle.
    playing_row: usize,
    /// Same, but for the pattern index.
    playing_pattern: usize,
    /// Cached at construction so [`Self::num_channels`] stays cheap
    /// even though the field-of-truth (`module.get_num_channels()`)
    /// is also O(1).
    num_channels: usize,

    // --- Loop-detection bookkeeping for `max_loop_count`. ---
    row_loop_count: Vec<Vec<usize>>,
    loop_count: usize,
    max_loop_count: usize,

    /// One-shot terminator latched by an explicit end-of-song
    /// signal. Cleared by `goto` so the caller can jump past
    /// the terminator and keep playing.
    song_end_requested: bool,

    /// Index in [`xmrs::core::daw::timeline::TimelineMap::entries`] of the
    /// row currently sounding. Valid only when `started` is `true`.
    timeline_idx: usize,
    /// Whether the very first row has been played. While `false`,
    /// [`Self::advance_to_next_entry`] seeds `timeline_idx` from the
    /// first entry of `current_song` instead of advancing.
    started: bool,
    /// Set by [`Self::goto`] / [`Self::goto_tick`] to redirect the
    /// next row-start to the entry already stored in `timeline_idx`,
    /// bypassing the linear advance. Consumed once.
    restart_row: bool,
}

impl<'a> Sequencer<'a> {
    /// Build a sequencer for the given module.
    ///
    /// `song` is clamped to a valid sub-song defined by
    /// `module.timeline_map`. Falls back to song 0 when out of range.
    pub(crate) fn new(module: &'a Module, song: usize) -> Self {
        let num_songs = module
            .timeline_map
            .entries
            .iter()
            .map(|e| e.song as usize)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        let current_song = if song < num_songs.max(1) { song } else { 0 };
        let num_channels = module.get_num_channels();
        let song_len = module.song_length(current_song);

        Self {
            module,
            current_song,
            tempo: module.default_tempo,
            bpm: module.default_bpm,
            current_tick: 0,
            bpm_slide_delta: 0,
            current_table_index: 0,
            current_row: 0,
            playing_row: 0,
            playing_pattern: module.order_pattern_idx(current_song, 0).unwrap_or(0),
            num_channels,
            row_loop_count: (0..song_len.max(1))
                .map(|order_idx| {
                    let rows = module
                        .order_pattern_idx(current_song, order_idx)
                        .map(|p| module.pattern_row_count(p))
                        .unwrap_or(0);
                    vec![0; rows]
                })
                .collect(),
            loop_count: 0,
            max_loop_count: 0,
            song_end_requested: false,
            timeline_idx: 0,
            started: false,
            restart_row: false,
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
    /// Pattern index that the current table cursor points to.
    pub fn current_pattern(&self) -> usize {
        self.module
            .order_pattern_idx(self.current_song, self.current_table_index)
            .unwrap_or(0)
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
        self.num_channels
    }

    /// Position in `module.timeline_map.entries` of the row
    /// currently sounding. Stable across the row's tick cycle.
    /// Returns `0` until the first row plays.
    pub fn timeline_idx(&self) -> usize {
        self.timeline_idx
    }

    /// Absolute song tick of the currently playing row, derived
    /// from the timeline_map entry at [`Self::timeline_idx`]. `0`
    /// when no row has played yet.
    pub fn current_abs_tick(&self) -> u32 {
        self.module
            .timeline_map
            .entries
            .get(self.timeline_idx)
            .map(|e| e.tick)
            .unwrap_or(0)
    }

    // --- Cursor manipulation ---

    /// Jump to `(table_position, row)` at `speed`; if `speed == 0`, reset to
    /// the module's default tempo. Returns `false` if the target is
    /// out of range.
    ///
    /// Semantically: the cursor is scheduled to fire on the *next*
    /// tick boundary. Tempo and BPM are reset and the song-end latch
    /// is cleared so the caller can jump past a `F00` terminator and
    /// keep playing — matches ProTracker's "press space to restart"
    /// behaviour after the song flag was cleared.
    pub(crate) fn goto(&mut self, table_position: usize, row: usize, speed: usize) -> bool {
        if table_position >= self.module.song_length(self.current_song) {
            return false;
        }
        let pat_idx = self
            .module
            .order_pattern_idx(self.current_song, table_position)
            .unwrap_or(0);
        if row >= self.module.pattern_row_count(pat_idx) {
            return false;
        }
        // The walker may not have visited `(table_position, row)`
        // directly (e.g. user picks a row never reached by playback).
        // Fall back to the order's entry row when no exact match
        // exists; otherwise return false.
        let song_u16 = self.current_song as u16;
        let entry_idx = self
            .module
            .timeline_map
            .entries
            .iter()
            .position(|e| {
                e.song == song_u16
                    && e.order_idx as usize == table_position
                    && e.row_idx as usize == row
                    && e.loop_iter == 0
            })
            .or_else(|| {
                self.module.timeline_map.entries.iter().position(|e| {
                    e.song == song_u16 && e.order_idx as usize == table_position && e.loop_iter == 0
                })
            });
        let entry_idx = match entry_idx {
            Some(i) => i,
            None => return false,
        };
        self.timeline_idx = entry_idx;
        self.restart_row = true;
        self.started = true;

        self.tempo = if speed == 0 {
            self.module.default_tempo
        } else {
            speed
        };
        self.bpm = self.module.default_bpm;
        self.bpm_slide_delta = 0;

        // Force the next advance_tick to be treated as a row-start tick.
        self.current_tick = 0;
        self.song_end_requested = false;

        true
    }

    /// Seek to an absolute song tick. Resolves `abs_tick` against
    /// `module.timeline_map.entries` to find the matching entry,
    /// then schedules a jump on the next tick. Returns `false` when
    /// no entry exists at exactly `abs_tick` for the current song
    /// (only canonical row-start ticks are valid — seeking inside a
    /// row is the caller's responsibility).
    pub fn goto_tick(&mut self, abs_tick: u32) -> bool {
        let song_u16 = self.current_song as u16;
        let entry_idx = self
            .module
            .timeline_map
            .entries
            .iter()
            .position(|e| e.song == song_u16 && e.tick == abs_tick && e.loop_iter == 0);
        let entry_idx = match entry_idx {
            Some(i) => i,
            None => return false,
        };
        let entry = self.module.timeline_map.entries[entry_idx];

        self.timeline_idx = entry_idx;
        self.restart_row = true;
        self.started = true;

        // Tempo / BPM come from the timeline_map entry — the walker
        // captured the effective row-start values, so the seek lands
        // on the same clock the linear playback would see.
        self.tempo = (entry.speed_at_row as usize).max(1);
        self.bpm = (entry.bpm_at_row as usize).max(1);
        self.bpm_slide_delta = 0;

        self.current_tick = 0;
        self.song_end_requested = false;
        true
    }

    // --- Tick advance (driven by the facade) ---

    /// Advance the sequencer by one song tick. The facade is responsible for
    /// computing the sample-clock gap between ticks — here we just bump the
    /// song-side tick counter and, when appropriate, apply row-change logic.
    pub(crate) fn advance_tick(&mut self) -> TickOutcome {
        if self.is_exhausted() {
            return TickOutcome::SongEnd;
        }

        if self.current_tick == 0 {
            let outcome = self.row_start_tick();
            // Re-check exhaustion — `row_start_tick` updates `loop_count`
            // and may set `song_end_requested` when no next entry exists.
            if self.is_exhausted() {
                return TickOutcome::SongEnd;
            }
            self.advance_tick_counter();
            outcome
        } else {
            let tick_processed = self.current_tick;
            // Per-tick BPM slide. Each non-row-start tick bumps the
            // BPM by the stored delta, clamped to the tracker-legal
            // range. When the next row arrives the latch is reset in
            // `row_start_tick` (a fresh `BpmSlide` may re-arm it).
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
        self.song_end_requested
            || (self.max_loop_count > 0 && self.loop_count >= self.max_loop_count)
    }

    /// Core of the row-start tick: pick the next entry to play
    /// from `module.timeline_map`, publish its `(pattern, row,
    /// tempo, bpm)`, and latch any `BpmSlide` on the row.
    fn row_start_tick(&mut self) -> TickOutcome {
        let prev_table_index = if self.started {
            Some(self.current_table_index)
        } else {
            None
        };

        let entry_idx = match self.advance_to_next_entry() {
            Some(i) => i,
            None => {
                // TimelineMap exhausted and no wrap target.
                self.song_end_requested = true;
                return TickOutcome::SongEnd;
            }
        };

        let entry = self.module.timeline_map.entries[entry_idx];
        let pat_idx = entry.pattern_idx as usize;
        let row = entry.row_idx as usize;
        let new_order = entry.order_idx as usize;

        self.timeline_idx = entry_idx;
        self.tempo = (entry.speed_at_row as usize).max(1);
        self.bpm = (entry.bpm_at_row as usize).max(1);
        self.bpm_slide_delta = 0;

        // Loop-detection counter. Suppress the bump when we're inside
        // a pattern-loop body (entry.loop_iter > 0): only canonical
        // first-iteration visits feed `max_loop_count`.
        if entry.loop_iter == 0 {
            if let Some(slot) = self
                .row_loop_count
                .get_mut(new_order)
                .and_then(|order_slot| order_slot.get_mut(row))
            {
                self.loop_count = *slot;
                *slot += 1;
            }
        }

        // Read `BpmSlide` from the Bpm/Slide AutomationLane.
        // DAW migration Phase 3c.3: BpmSlide no longer sources
        // from `cell.global_effects` — the lane built by
        // `extract_bpm_slide_events` is the authoritative form.
        // Other song-level / navigation effects are either pre-baked
        // into the entry (`Bpm`, `Speed`), consumed by `Voices`
        // (`Volume`, `VolumeSlide`, `MidiMacro`) or absorbed by the
        // walker (`PatternBreak`, `PositionJump`, `PatternLoop`,
        // `PatternDelay`).
        self.latch_bpm_slide_from_lane(entry.tick);

        self.playing_pattern = pat_idx;
        self.playing_row = row;
        self.current_table_index = new_order;
        // +1-biased to match historical `current_row()`.
        self.current_row = row + 1;

        let pattern_changed = prev_table_index != Some(new_order);

        TickOutcome::RowStart {
            pattern: pat_idx,
            row,
            pattern_changed,
        }
    }

    /// Choose the next entry index to play. Honours, in order:
    ///
    /// 1. First-ever firing: pick the first entry of `current_song`.
    /// 2. A pending `restart_row` from `goto`/`goto_tick`: reuse
    ///    `self.timeline_idx` as-is.
    /// 3. Linear advance: `timeline_idx + 1` if still within `current_song`.
    /// 4. End-of-song wrap: the entry matching `module.song_loop_to`
    ///    if set, otherwise the first entry of `current_song`.
    /// 5. Nothing left (e.g. empty `timeline_map`): `None`.
    fn advance_to_next_entry(&mut self) -> Option<usize> {
        if !self.started {
            self.started = true;
            return self.first_entry_of_current_song();
        }
        if self.restart_row {
            self.restart_row = false;
            return Some(self.timeline_idx);
        }
        let entries = &self.module.timeline_map.entries;
        let song_u16 = self.current_song as u16;
        let next = self.timeline_idx + 1;
        if let Some(e) = entries.get(next) {
            if e.song == song_u16 {
                return Some(next);
            }
        }
        // End-of-song wrap.
        if let Some(loop_tick) = self.module.song_loop_to {
            if let Some(idx) = entries
                .iter()
                .position(|e| e.song == song_u16 && e.tick == loop_tick && e.loop_iter == 0)
            {
                return Some(idx);
            }
        }
        self.first_entry_of_current_song()
    }

    fn first_entry_of_current_song(&self) -> Option<usize> {
        let song_u16 = self.current_song as u16;
        self.module
            .timeline_map
            .entries
            .iter()
            .position(|e| e.song == song_u16)
    }

    /// Latch the per-tick BPM-slide delta from the `Bpm` `Slide`
    /// [`AutomationLane`] at `abs_tick`. When the lane is unarmed
    /// (or absent), the latch is cleared to 0 — already done by the
    /// caller right above, so this method only writes when armed.
    fn latch_bpm_slide_from_lane(&mut self, abs_tick: u32) {
        for lane in self
            .module
            .lanes_for(xmrs::core::daw::automation::AutomationTarget::Bpm)
        {
            if let Some(state) = lane.slide_state_at(abs_tick) {
                if state.armed {
                    self.bpm_slide_delta = state.rate.raw() as isize;
                }
            }
        }
    }

    fn advance_tick_counter(&mut self) {
        self.current_tick += 1;
        // `F00` (speed = 0) halts row advance: tick() keeps firing so
        // sustained effects continue, but tick0() never runs again
        // until the tempo is set to a non-zero value. In practice
        // the walker already truncated the timeline_map past the
        // `F00` row (`speed_zero_ends_song` quirk), so this branch
        // is mostly defensive for synthetic modules.
        if self.tempo != 0 && self.current_tick >= self.tempo {
            self.current_tick = 0;
        }
    }
}

#[cfg(all(test, any(feature = "import_xm", feature = "import")))]
mod phase_3c2_tests {
    use super::*;

    /// DAW migration Phase 3c.2 invariant: after each
    /// `RowStart`, `timeline_idx` points at the entry whose
    /// `(song, pattern, row)` matches the row just emitted.
    #[test]
    fn timeline_idx_tracks_navigation_on_note_xm() {
        let data = include_bytes!("../../xmrs/examples/note.xm");
        let module = Module::load_xm(data).expect("load");
        let mut seq = Sequencer::new(&module, 0);
        for _ in 0..256 {
            match seq.advance_tick() {
                TickOutcome::RowStart { pattern, row, .. } => {
                    let entry = module
                        .timeline_map
                        .entries
                        .get(seq.timeline_idx())
                        .expect("timeline_idx points past entries vec");
                    assert_eq!(entry.song as usize, seq.current_song());
                    assert_eq!(
                        entry.pattern_idx as usize, pattern,
                        "timeline_idx pattern mismatch"
                    );
                    assert_eq!(entry.row_idx as usize, row, "timeline_idx row mismatch");
                }
                TickOutcome::Tick { .. } => {}
                TickOutcome::SongEnd => break,
            }
        }
    }

    /// `goto_tick` resolves an absolute song tick to the matching
    /// TimelineMap entry and jumps. Round trips between
    /// `(table_index, row)` and `abs_tick`.
    #[test]
    fn goto_tick_lands_on_matching_entry() {
        let data = include_bytes!("../../xmrs/examples/note.xm");
        let module = Module::load_xm(data).expect("load");
        if module.timeline_map.entries.len() < 3 {
            return;
        }
        let target = module.timeline_map.entries[2];
        let mut seq = Sequencer::new(&module, 0);
        assert!(seq.goto_tick(target.tick), "goto_tick should succeed");
        assert_eq!(seq.timeline_idx(), 2);
        match seq.advance_tick() {
            TickOutcome::RowStart { pattern, row, .. } => {
                assert_eq!(pattern as u32, target.pattern_idx);
                assert_eq!(row as u32, target.row_idx);
            }
            _ => panic!("expected RowStart after goto_tick"),
        }
    }

    #[test]
    fn goto_tick_rejects_invalid_tick() {
        let data = include_bytes!("../../xmrs/examples/note.xm");
        let module = Module::load_xm(data).expect("load");
        let mut seq = Sequencer::new(&module, 0);
        assert!(!seq.goto_tick(u32::MAX));
    }

    /// End-of-song wrap honours `song_loop_to`: a synthetic
    /// 2-pattern module wraps to the matching entry instead of
    /// blindly resetting to order 0.
    #[test]
    fn song_loop_to_drives_end_of_song_wrap() {
        use xmrs::core::cell::Cell;
        use xmrs::core::daw::clip::Clip;
        use xmrs::core::daw::sorted_clips::SortedClips;
        use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
        use xmrs::core::daw::track::Track;

        let mut m = Module::default();
        m.tracks.push(Track {
            name: "t".into(),
            instrument: 0,
            rows: alloc::vec![Cell::default(); 4],
            muted: false,
        });
        let clips = alloc::vec![
            Clip {
                track: 0,
                song: 0,
                target_channel: 0,
                position_tick: 0,
                speed_at_start: 6,
                track_row_offset: 0,
                source_start_row: 0,
                end_tick: 24,
            },
            Clip {
                track: 0,
                song: 0,
                target_channel: 0,
                position_tick: 24,
                speed_at_start: 6,
                track_row_offset: 0,
                source_start_row: 0,
                end_tick: 48,
            },
        ];
        m.clips = SortedClips::from_unsorted(clips);
        let mut entries = alloc::vec::Vec::new();
        for (order_idx, base_tick) in [(0u32, 0u32), (1, 24)] {
            for row in 0..4u32 {
                entries.push(TimelineEntry {
                    song: 0,
                    order_idx,
                    pattern_idx: order_idx,
                    row_idx: row,
                    loop_iter: 0,
                    tick: base_tick + row * 6,
                    speed_at_row: 6,
                    bpm_at_row: 125,
                });
            }
        }
        m.timeline_map = TimelineMap { entries };
        m.song_loop_to = Some(24);

        let mut seq = Sequencer::new(&m, 0);
        let mut rows_visited: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        for _ in 0..200 {
            match seq.advance_tick() {
                TickOutcome::RowStart { pattern, row, .. } => {
                    rows_visited.push((pattern, row));
                    if rows_visited.len() >= 12 {
                        break;
                    }
                }
                TickOutcome::Tick { .. } => {}
                TickOutcome::SongEnd => break,
            }
        }
        assert_eq!(rows_visited[0..4], [(0, 0), (0, 1), (0, 2), (0, 3)]);
        assert_eq!(rows_visited[4..8], [(1, 0), (1, 1), (1, 2), (1, 3)]);
        assert_eq!(
            rows_visited[8],
            (1, 0),
            "song_loop_to should re-enter order 1"
        );
    }

    /// Without `song_loop_to`, the end-of-song wrap falls back to
    /// the first entry of the song (legacy "wrap to order 0").
    #[test]
    fn end_of_song_wraps_to_order_zero_by_default() {
        use xmrs::core::cell::Cell;
        use xmrs::core::daw::sorted_clips::SortedClips;
        use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
        use xmrs::core::daw::track::Track;

        let mut m = Module::default();
        m.tracks.push(Track {
            name: "t".into(),
            instrument: 0,
            rows: alloc::vec![Cell::default(); 2],
            muted: false,
        });
        m.clips = SortedClips::from_unsorted(alloc::vec::Vec::new());
        let mut entries = alloc::vec::Vec::new();
        for row in 0..2u32 {
            entries.push(TimelineEntry {
                song: 0,
                order_idx: 0,
                pattern_idx: 0,
                row_idx: row,
                loop_iter: 0,
                tick: row * 6,
                speed_at_row: 6,
                bpm_at_row: 125,
            });
        }
        m.timeline_map = TimelineMap { entries };
        // song_loop_to deliberately left None.

        let mut seq = Sequencer::new(&m, 0);
        let mut rows_visited: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        for _ in 0..200 {
            match seq.advance_tick() {
                TickOutcome::RowStart { row, .. } => {
                    rows_visited.push(row);
                    if rows_visited.len() >= 5 {
                        break;
                    }
                }
                TickOutcome::Tick { .. } => {}
                TickOutcome::SongEnd => break,
            }
        }
        assert_eq!(
            rows_visited,
            alloc::vec![0, 1, 0, 1, 0],
            "no song_loop_to → wrap to first entry of song"
        );
    }
}
