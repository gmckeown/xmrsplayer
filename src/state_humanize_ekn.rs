//! Per-channel state driving Euclidean humanisation.
//!
//! Lives on each [`crate::channel::Channel`]; coordinated by the
//! player's `step()` loop:
//!
//! 1. When a channel triggers an `InstrumentType::Euclidean`, the
//!    state arms (`note_euclidian_triggered`).
//! 2. At every row-start AFTER the first armed trigger, the player
//!    rolls the RNG (`roll`) and — if humanisation hits — schedules
//!    an early fire intent (`schedule_fire`) AND suppresses the
//!    next natural row-start trigger.
//! 3. At each non-row-start tick the player asks
//!    `take_fire_intent_at`; when the scheduled tick matches, the
//!    channel triggers the cell early.
//! 4. At the next row-start the player consults `take_suppress`;
//!    if set, the channel's cell is dropped (the pulse has already
//!    fired earlier).
//!
//! Default state (`Self::new`) is "no Euclidean armed, no intent,
//! no suppress". A channel that never sees an Euclidean carries
//! this state at near-zero cost.

use xmrs::fixed::fixed::Q15;
use xmrs::prelude::TrackUnit;
use xmrs::xorshift::XorShift32;

/// Per-channel Euclidean humanisation state.
#[derive(Clone)]
pub(crate) struct StateHumanizeEkn {
    /// Front instrument index of the last Euclidean trigger.
    /// `None` when no Euclidean is currently armed on this channel.
    last_front: Option<usize>,

    /// Per-channel RNG for humanize rolls. Seeded deterministically
    /// at channel construction; the seed can be refreshed via
    /// [`Self::reseed`] (typically by the facade when switching to
    /// [`crate::humanize::HumanizeMode::Live`] /
    /// [`Deterministic`](crate::humanize::HumanizeMode::Deterministic)).
    rng: XorShift32,

    /// Pending fire-early intent: `(tick_within_row, cell)`.
    /// `Some` means: when the channel reaches this tick inside the
    /// current row, trigger the cell early.
    fire_intent: Option<(u8, TrackUnit)>,

    /// `true` when the next row-start trigger should be skipped on
    /// this channel because the pulse was already fired early.
    suppress_next_natural: bool,
}

impl StateHumanizeEkn {
    pub fn new(seed: u32) -> Self {
        Self {
            last_front: None,
            rng: XorShift32::new(Some(seed.max(1))),
            fire_intent: None,
            suppress_next_natural: false,
        }
    }

    pub fn reseed(&mut self, seed: u32) {
        self.rng = XorShift32::new(Some(seed.max(1)));
    }

    /// Record that an Euclidean wrapper has just fired its wrapped
    /// sample on this channel. The channel is now "armed" — the
    /// next natural-arrival pulse of the same Euclidean becomes
    /// eligible for humanisation.
    pub fn note_euclidian_triggered(&mut self, front_instr: usize) {
        self.last_front = Some(front_instr);
    }

    /// Clear the armed state (called when a non-Euclidean trigger
    /// fires on the same channel: a new instrument takes over,
    /// humanisation continuity is lost).
    pub fn clear_euclidian(&mut self) {
        self.last_front = None;
        self.fire_intent = None;
    }

    /// `Some(front_instr_idx)` when the channel is armed.
    pub fn armed_front(&self) -> Option<usize> {
        self.last_front
    }

    /// Roll humanize. Returns `Some(advance)` (in ticks) when the
    /// player should fire the upcoming pulse `advance` ticks earlier
    /// than its natural tick. `None` means natural fire.
    ///
    /// `Q15::ZERO` probability and `0` advance_max_ticks both yield
    /// `None` immediately — the cycle-exact short-circuit.
    pub fn roll(&mut self, probability: Q15, advance_max_ticks: u8) -> Option<u8> {
        if probability == Q15::ZERO || advance_max_ticks == 0 {
            return None;
        }
        // Unsigned Q15 in [0, 0x7FFF] from the low 15 bits.
        let r = (self.rng.next().unwrap() & 0x7FFF) as i16;
        if r < probability.raw() {
            let advance = 1u8 + (self.rng.next().unwrap() % advance_max_ticks as u32) as u8;
            Some(advance)
        } else {
            None
        }
    }

    /// Schedule a fire-early at `tick_within_row` with the given
    /// cell. Also marks `suppress_next_natural` so the natural
    /// row-start trigger of the same pulse is skipped.
    pub fn schedule_fire(&mut self, tick_within_row: u8, cell: TrackUnit) {
        self.fire_intent = Some((tick_within_row, cell));
        self.suppress_next_natural = true;
    }

    /// If the fire-intent's scheduled tick matches `current_tick`,
    /// consume it and return the cell to trigger now. Otherwise
    /// `None`.
    pub fn take_fire_intent_at(&mut self, current_tick: u8) -> Option<TrackUnit> {
        match &self.fire_intent {
            Some((scheduled_tick, _)) if *scheduled_tick == current_tick => {
                self.fire_intent.take().map(|(_, cell)| cell)
            }
            _ => None,
        }
    }

    /// Returns and clears the suppress flag.
    pub fn take_suppress(&mut self) -> bool {
        core::mem::replace(&mut self.suppress_next_natural, false)
    }
}
