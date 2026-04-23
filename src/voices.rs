//! Audio-side rendering engine.
//!
//! `Voices` owns everything the sequencer does **not**: the per-channel
//! playback state, the global mixer gain, and the sample-generation hot
//! path. It consumes row/tick events (dispatched by the facade after the
//! [`crate::sequencer::Sequencer`] has advanced) and turns them into stereo
//! samples.
//!
//! In the observer model, `Voices` is the built-in row subscriber: the
//! facade always forwards row-start cells and sustained ticks here first,
//! then to any user-registered observers. The separation is purely
//! architectural — there is no `dyn` dispatch involved on the audio path.
//!
//! `Voices` also handles the two volume-side global effects
//! (`GlobalEffect::Volume`, `GlobalEffect::VolumeSlide`) — the sequencer
//! leaves those alone because they belong to the mixer, not to navigation.

use crate::channel::Channel;
use crate::triggerkeep::TRIGGER_KEEP_PERIOD;
use alloc::{vec, vec::Vec};
use xmrs::prelude::*;

/// The audio engine. Built once per player and driven via
/// [`Voices::process_row`] / [`Voices::process_tick`].
pub struct Voices<'a> {
    sample_rate: f32,

    channel: Vec<Channel<'a>>,

    /// Global volume (0.0 ..= 1.0). Mutated by `GlobalEffect::Volume` and by
    /// accumulating `GlobalEffect::VolumeSlide`. Applied as a final gain in
    /// [`Voices::mix`].
    global_volume: f32,
    /// Extra amplification applied after `global_volume` — caller-controlled,
    /// defaults to 1.0. Raise above 1.0 at your own clipping risk; lower to
    /// 0.25 on busy modules (see README).
    amplification: f32,

    /// Non-fine volume-slide speed latched on the last row that carried a
    /// `GlobalEffect::VolumeSlide { fine: false }`. Applied once per tick as
    /// long as `row_has_global_volume_slide` stays true.
    volume_slide_speed: f32,
    /// Whether the last latched slide was a fine one (fires once at tick 0
    /// only). Tracked so we know whether to keep sliding on subsequent
    /// ticks of this row.
    volume_slide_fine: bool,
    /// Cached at row-start: does any cell on the current row carry a global
    /// volume slide (fine or not)? Set during `process_row`, read during
    /// `process_tick` to decide whether to apply the rolling slide. The
    /// fine/non-fine distinction is handled through `volume_slide_fine`.
    row_has_global_volume_slide: bool,
}

impl<'a> Voices<'a> {
    pub(crate) fn new(module: &'a Module, sample_rate: f32, initial_tempo: usize) -> Self {
        let num_channels = module.get_num_channels();
        Self {
            sample_rate,
            channel: vec![Channel::new(module, sample_rate, initial_tempo); num_channels],
            global_volume: 1.0,
            amplification: 1.0,
            volume_slide_speed: 0.0,
            volume_slide_fine: true,
            row_has_global_volume_slide: false,
        }
    }

    // --- Accessors / mutators (used by the facade to expose public API) ---

    pub fn global_volume(&self) -> f32 {
        self.global_volume
    }
    pub fn set_global_volume(&mut self, v: f32) {
        self.global_volume = v.clamp(0.0, 1.0);
    }
    pub fn amplification(&self) -> f32 {
        self.amplification
    }
    pub fn set_amplification(&mut self, a: f32) {
        self.amplification = a;
    }
    pub fn sample_rate(&self) -> f32 {
        self.sample_rate
    }
    pub fn num_channels(&self) -> usize {
        self.channel.len()
    }
    pub fn set_mute_channel(&mut self, channel_num: usize, mute: bool) {
        if channel_num < self.channel.len() {
            self.channel[channel_num].muted = mute;
        }
    }
    pub fn mute_all(&mut self, mute: bool) {
        for c in &mut self.channel {
            c.muted = mute;
        }
    }

    /// Propagate a tempo change to each channel's arpeggio state. Called by
    /// the facade whenever the sequencer's tempo has actually changed —
    /// gating the N-channel loop on a real delta keeps the common case at a
    /// single compare.
    pub(crate) fn set_tempo(&mut self, tempo: usize) {
        for ch in &mut self.channel {
            ch.set_tempo(tempo);
        }
    }

    /// Called by the facade on a `goto` (external seek) so each channel
    /// clears what it safely can without touching pitch. Mirrors the
    /// previous behaviour of the old `XmrsPlayer::goto` cleanup loop.
    pub(crate) fn reset_for_goto(&mut self) {
        self.global_volume = 1.0;
        for ch in &mut self.channel {
            ch.clear_ghosts();
            ch.trigger_pitch(TRIGGER_KEEP_PERIOD);
        }
    }

    // --- Row / tick dispatch ---

    /// Forward the cells of a newly loaded row to each channel, applying the
    /// volume-side global effects as we go.
    ///
    /// Cell count is expected to match `self.channel.len()` — any extra
    /// cells are ignored, any missing cells are skipped (defensive against
    /// malformed modules).
    pub(crate) fn process_row(&mut self, cells: &[TrackUnit]) {
        // First, cache whether this row carries any global volume slide
        // (fine or not). Used by `process_tick` to decide whether to keep
        // sliding on subsequent ticks.
        self.row_has_global_volume_slide = cells.iter().any(|cell| cell.has_global_volume_slide());

        let n = self.channel.len().min(cells.len());
        for i in 0..n {
            let cell = &cells[i];
            self.channel[i].tick0(cell);
            self.apply_row_global_effects(i, cell);
        }
    }

    /// Consume the channel-routed entries of `cell.global_effects` for
    /// a single channel. Covers:
    /// * `Volume` / `VolumeSlide` — mix-level gain control
    /// * `MidiMacro` — filter automation (per-channel, despite the
    ///   `GlobalEffect` typing — see `Channel::apply_midi_macro`)
    ///
    /// The navigation-side arms (`Bpm`, `BpmSlide`, `PatternBreak`,
    /// `PatternLoop`, `PatternDelay`) are owned by the sequencer and
    /// skipped here.
    fn apply_row_global_effects(&mut self, ch_index: usize, cell: &TrackUnit) {
        for gfx in cell.global_effects.clone() {
            match gfx {
                GlobalEffect::Volume(volume) => {
                    self.global_volume = volume.clamp(0.0, 1.0);
                }
                GlobalEffect::VolumeSlide { speed: s, fine: f } => {
                    if f {
                        // Fine slide: apply once at tick 0.
                        self.global_volume = (self.global_volume + s).clamp(0.0, 1.0);
                    }
                    // Latch the speed so non-fine slides apply on subsequent ticks.
                    self.volume_slide_speed = s;
                    self.volume_slide_fine = f;
                }
                GlobalEffect::MidiMacro(macro_type) => {
                    if let Some(ch) = self.channel.get_mut(ch_index) {
                        ch.apply_midi_macro(macro_type);
                    }
                }
                // Everything else is owned by the sequencer.
                _ => {}
            }
        }
    }

    /// Advance every channel by one sustained (non-row-start) tick.
    /// `current_tick` is the sequencer's tick counter at the moment of
    /// the call — guaranteed to be >= 1.
    pub(crate) fn process_tick(&mut self, current_tick: usize) {
        for ch in &mut self.channel {
            ch.tick(current_tick);
        }
        // Apply rolling global volume slide, if the current row carries a
        // non-fine one. Fine slides were already applied at row-start.
        if self.row_has_global_volume_slide && !self.volume_slide_fine {
            self.global_volume = (self.global_volume + self.volume_slide_speed).clamp(0.0, 1.0);
        }
    }

    // --- Sample generation ---

    /// Fold each channel's sample into a single stereo output, applying the
    /// final mixer gain (`global_volume * amplification`) when requested.
    ///
    /// Hot path: walks channels once with a `(f32, f32)` accumulator and
    /// never allocates. At 48 kHz stereo this is the function that runs
    /// 48 000 times a second.
    ///
    /// When `per_channel_out` is `Some(buffer)`, each channel's post-mute
    /// `(left, right)` pair is also written into the buffer at its channel
    /// index — used by the facade to feed `ChannelsObserver`s without a
    /// second pass over the channel list. The buffer length must be at
    /// least `self.channel.len()`; any trailing entries are left untouched.
    /// Pass `None` to skip the capture (the common path, when no channels
    /// observer is registered).
    pub(crate) fn mix(
        &mut self,
        apply_volume: bool,
        per_channel_out: Option<&mut [(f32, f32)]>,
    ) -> (f32, f32) {
        let (mut left, mut right) = (0.0f32, 0.0f32);

        // Split the capture path from the plain path so the no-capture case
        // keeps its tight loop without a per-iteration `Option` check.
        match per_channel_out {
            None => {
                for ch in &mut self.channel {
                    if let Some((l, r)) = ch.next() {
                        if !ch.is_muted() {
                            left += l;
                            right += r;
                        }
                    }
                    // `None` from a channel (no active instrument) contributes
                    // zero; muted channels are advanced (so position tracking
                    // stays correct when they unmute) but their output is
                    // discarded.
                }
            }
            Some(buf) => {
                for (idx, ch) in self.channel.iter_mut().enumerate() {
                    let val = match ch.next() {
                        Some((l, r)) if !ch.is_muted() => {
                            left += l;
                            right += r;
                            (l, r)
                        }
                        _ => (0.0, 0.0),
                    };
                    // Index is bounded by self.channel.len(); the facade
                    // guarantees the buffer was sized to match.
                    if idx < buf.len() {
                        buf[idx] = val;
                    }
                }
            }
        }

        if apply_volume {
            let g = self.global_volume * self.amplification;
            (left * g, right * g)
        } else {
            (left, right)
        }
    }

    /// Return one `(left, right)` sample per channel, pre-mix, pre-gain.
    /// Allocates a `Vec`; used by the rarely-hit
    /// `XmrsPlayer::samples_from_channels` API for per-channel graphic
    /// effects. Kept for compatibility with pre-refactor callers.
    pub(crate) fn samples_from_channels(&mut self) -> Vec<(f32, f32)> {
        self.channel
            .iter_mut()
            .map(|ch| match ch.next() {
                Some(fval) => {
                    if ch.is_muted() {
                        (0.0, 0.0)
                    } else {
                        fval
                    }
                }
                None => (0.0, 0.0),
            })
            .collect()
    }
}
