//! Subscription API for the player.
//!
//! A [`PlayerObserver`] is notified every time the player crosses a song-side
//! event boundary: a new row is loaded, a sustained tick fires, the pattern
//! cursor moves to a new entry in the pattern order, or playback ends.
//!
//! The voices engine (the default audio renderer) is registered as an
//! observer internally; any additional observer plugs in next to it via
//! [`crate::xmrsplayer::XmrsPlayer::add_observer`].
//!
//! Observers are called **synchronously** on the thread that drives sample
//! generation. Long-running work or I/O must be deferred (push to a channel
//! drained elsewhere) — otherwise it will stall audio. See `OBSERVERS.md`
//! for the detailed threading contract and recipes.
//!
//! [`PlayerObserver`]: crate::observer::PlayerObserver

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use xmrs::prelude::*;

/// Internal wrapper around `Vec<Box<T>>` for boxed-trait-object
/// observer lists. Centralises the four facade quadruplets
/// (`add_X / clear_X / X_count / Vec<Box<dyn _ + Send>>`).
/// Deref/DerefMut expose slice methods (`iter`, `iter_mut`,
/// `is_empty`, `len`) without re-exporting `push` / `clear` —
/// callers go through [`Self::add`] / [`Self::clear`].
pub(crate) struct ObserverList<T: ?Sized>(Vec<Box<T>>);

impl<T: ?Sized> ObserverList<T> {
    pub fn new() -> Self {
        Self(Vec::new())
    }

    pub fn add(&mut self, observer: Box<T>) {
        self.0.push(observer);
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn count(&self) -> usize {
        self.0.len()
    }
}

impl<T: ?Sized> Deref for ObserverList<T> {
    type Target = [Box<T>];
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<T: ?Sized> DerefMut for ObserverList<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Context passed to [`PlayerObserver::on_row`] when a new row is loaded.
///
/// `cells` is borrowed directly from the module — no allocation, no copy —
/// so observers can inspect per-channel notes/instruments/effects at zero
/// cost.
pub struct RowContext<'a> {
    pub module: &'a Module,
    pub song: usize,
    pub pattern: usize,
    pub row: usize,
    /// One `Cell` per channel (length = `module.get_num_channels()`).
    pub cells: &'a [Cell],
    /// Current song speed (ticks per row) at the moment this row starts.
    pub tempo: usize,
    /// Current song BPM at the moment this row starts.
    pub bpm: usize,
}

/// Context passed to [`PlayerObserver::on_tick`] for sustained (non-row-start)
/// ticks. Row-start ticks fire `on_row` instead; they never fire `on_tick`.
pub struct TickContext<'a> {
    pub module: &'a Module,
    pub song: usize,
    pub pattern: usize,
    pub row: usize,
    /// Tick index inside the current row. 0 is reserved for row-start ticks
    /// (which never reach `on_tick`), so this value is always >= 1.
    pub tick: usize,
    pub cells: &'a [Cell],
}

/// Context passed to [`PlayerObserver::on_pattern_change`].
///
/// Fired when the pattern-order cursor moves to a new entry — either
/// naturally at end-of-pattern or via a jump effect (`Bxx`, `Dxx`, or a call
/// to [`crate::xmrsplayer::XmrsPlayer::goto`]).
pub struct PatternChangeContext<'a> {
    pub module: &'a Module,
    pub song: usize,
    pub new_table_index: usize,
    pub new_pattern: usize,
}

/// A subscriber to player events.
///
/// Implement this trait for anything that wants to react to song progress —
/// a UI highlighting the current note, a MIDI bridge, a recording probe…
///
/// ## `Send` requirement
///
/// Observers are stored as `Box<dyn PlayerObserver + Send>` because the
/// typical deployment pattern — `Arc<Mutex<XmrsPlayer>>` handed to a cpal /
/// rodio audio callback — requires the player (and everything it owns) to
/// be sendable between threads. If your observer holds a `!Send` type
/// (raw pointer, `Rc<_>`, etc.), wrap it in `Arc`/`Mutex` or restructure.
/// `Sync` is not required: callbacks are delivered through `&mut self`
/// from a single thread at a time (the one holding the mutex).
///
/// ## Required vs. optional methods
///
/// [`on_row`](Self::on_row) is the only mandatory callback; all others have
/// defaulted no-op bodies, so a typical observer only needs to implement
/// `on_row`.
///
/// ## Opting into ticks
///
/// `on_tick` fires roughly 50 times per second — pointless for most UI use
/// cases. Observers that only care about row changes must leave
/// [`subscribes_to_ticks`](Self::subscribes_to_ticks) at its default `false`
/// value; the player gates `on_tick` delivery on that flag.
///
/// Observers that legitimately need tick-level granularity (per-tick visual
/// effects, vibrato visualisers, etc.) override it to return `true`.
///
/// ## Threading
///
/// Every method runs on the thread that pulls samples from the player. Do
/// not block, allocate on hot paths, or do I/O — push events to a queue
/// drained elsewhere. See `OBSERVERS.md` for concrete patterns.
pub trait PlayerObserver: Send {
    /// Called at the start of every new row. Mandatory.
    fn on_row(&mut self, ctx: &RowContext<'_>);

    /// Called on every sustained (non-row-start) tick, **only if**
    /// [`subscribes_to_ticks`](Self::subscribes_to_ticks) returns `true`.
    fn on_tick(&mut self, _ctx: &TickContext<'_>) {}

    /// Return `true` to receive `on_tick` callbacks. Default: `false` — most
    /// observers only care about row changes.
    fn subscribes_to_ticks(&self) -> bool {
        false
    }

    /// Called when the pattern-order cursor moves to a new entry.
    fn on_pattern_change(&mut self, _ctx: &PatternChangeContext<'_>) {}

    /// Called once when playback ends (e.g. `max_loop_count` reached). No
    /// further events fire until the player is reset.
    fn on_song_end(&mut self) {}
}
