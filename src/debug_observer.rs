//! A built-in observer that prints row/pattern changes, useful to replace the
//! scattered `debug(bool)` toggle the pre-0.10 player carried inline.
//!
//! Gated on the `std` feature because it uses `println!`. On `no_std` targets
//! you can still write your own observer and print through whatever log
//! facility is available on the platform.

use crate::observer::{PatternChangeContext, PlayerObserver, RowContext, TickContext};

/// Prints pattern-order moves and per-row notes on every channel.
///
/// Register with [`crate::xmrsplayer::XmrsPlayer::add_observer`]:
///
/// ```no_run
/// use xmrs::core::module::Module;
/// use xmrsplayer::prelude::*;
///
/// let module = Module::default();
/// let mut player = XmrsPlayer::new(&module, 48_000, 0);
/// player.add_observer(Box::new(DebugObserver::new()));
/// ```
#[derive(Default)]
pub struct DebugObserver {
    /// When `true`, also print on every sustained tick (very verbose).
    /// Off by default.
    pub trace_ticks: bool,
}

impl DebugObserver {
    pub fn new() -> Self {
        Self::default()
    }

    /// Enable per-tick tracing. Off by default.
    pub fn with_trace_ticks(mut self, on: bool) -> Self {
        self.trace_ticks = on;
        self
    }
}

impl PlayerObserver for DebugObserver {
    fn on_row(&mut self, ctx: &RowContext<'_>) {
        print!("{:03X} ", ctx.row);
        for cell in ctx.cells {
            print!("{:?} ", cell.event);
        }
        println!();
    }

    fn on_pattern_change(&mut self, ctx: &PatternChangeContext<'_>) {
        println!(
            "pattern_order[0x{:03x}] = 0x{:03x}",
            ctx.new_table_index, ctx.new_pattern
        );
    }

    fn on_tick(&mut self, ctx: &TickContext<'_>) {
        println!(
            "  tick {:02}  (pat {:03X} row {:03X})",
            ctx.tick, ctx.pattern, ctx.row
        );
    }

    fn subscribes_to_ticks(&self) -> bool {
        self.trace_ticks
    }

    fn on_song_end(&mut self) {
        println!("(song end)");
    }
}
