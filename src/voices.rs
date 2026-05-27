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
//! (`SongLevelEffect::Volume`, `SongLevelEffect::VolumeSlide`) — the sequencer
//! leaves those alone because they belong to the mixer, not to navigation.

use crate::channel::Channel;
use crate::midi_observer::MidiEvent;
use crate::triggerkeep::TRIGGER_KEEP_PERIOD;
use crate::voice_pool::VoicePool;
use alloc::{vec, vec::Vec};
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{Amplification, SampleRate, Volume};
use xmrs::prelude::*;

/// Default voice pool capacity. Mirrors schismtracker's
/// `MAX_VOICES = 256` (`include/player/sndfile.h:40`) — large
/// enough that even modules using NNA = Continue heavily on
/// multiple channels rarely hit the cap.
///
/// Exposed publicly so users can pass it to
/// [`crate::xmrsplayer::XmrsPlayer::new_with_voice_pool_capacity`] when they
/// want the default explicitly, or compare against it before
/// passing a custom value.
pub const DEFAULT_VOICE_POOL_CAPACITY: usize = 256;

/// The audio engine. Built once per player and driven via
/// `process_row` / `process_tick` (crate-private; the facade calls
/// them on each row/tick boundary).
pub struct Voices<'a> {
    sample_rate: SampleRate,

    /// Module reference, kept so the global-volume / global-volume-
    /// slide lane lookups in [`Self::process_row`] can introspect
    /// `module.automation` without the facade having to thread it
    /// through every call.
    module: &'a Module,

    channel: Vec<Channel<'a>>,

    /// Pool of voices, both live and NNA-detached. Channels
    /// reference their voices by `VoiceId` — see
    /// [`Channel::ghosts`] and [`Channel::live`]. The pool lives at
    /// this level (rather than per-channel) so voice stealing
    /// happens across the whole population, mirroring schism's
    /// `csf_get_nna_channel`.
    pool: VoicePool<'a>,

    /// Global volume (0.0 ..= 1.0) Q1.15. Mutated by
    /// `SongLevelEffect::Volume` and by accumulating
    /// `SongLevelEffect::VolumeSlide`. Applied as a final gain in
    /// [`Voices::mix`].
    global_volume: Volume,
    /// Extra amplification applied after `global_volume` —
    /// caller-controlled, defaults to unity (`Q4.12 = 0x1000`).
    /// Stored as `Amplification` (Q4.12) so it can express up
    /// to 8× gain without leaving the i16 raw range.
    amplification: Amplification,

    /// Non-fine volume-slide speed latched on the last row that carried a
    /// `SongLevelEffect::VolumeSlide { fine: false }`. Applied once per tick as
    /// long as `row_has_global_volume_slide` stays true.
    volume_slide_speed: Q15,
    /// Whether the last latched slide was a fine one (fires once at tick 0
    /// only). Tracked so we know whether to keep sliding on subsequent
    /// ticks of this row.
    volume_slide_fine: bool,
    /// Cached at row-start: does any cell on the current row carry a global
    /// volume slide (fine or not)? Set during `process_row`, read during
    /// `process_tick` to decide whether to apply the rolling slide. The
    /// fine/non-fine distinction is handled through `volume_slide_fine`.
    row_has_global_volume_slide: bool,

    /// MIDI events emitted by macros on the current row. Filled by
    /// `apply_row_per_channel_effects` and drained by the facade,
    /// which dispatches to any registered
    /// [`crate::midi_observer::MidiObserver`]. The buffer is re-used
    /// across rows — cleared on entry to each `process_row`.
    pending_midi_events: Vec<(usize, MidiEvent)>,

    /// Humanisation mode for `Track::Euclidean` pulses.
    /// Default [`crate::humanize::HumanizeMode::Disabled`] keeps
    /// playback strictly cycle-exact.
    humanize_mode: crate::humanize::HumanizeMode,
}

impl<'a> Voices<'a> {
    pub(crate) fn new_with_voice_pool_capacity(
        module: &'a Module,
        sample_rate: u32,
        initial_tempo: usize,
        voice_pool_capacity: usize,
    ) -> Self {
        let num_channels = module.get_num_channels();
        let sr = SampleRate::from_hz(sample_rate.max(1));
        let mut channels = vec![Channel::new(module, sr, initial_tempo); num_channels];
        // Apply per-channel defaults from the module header.
        // Each entry can carry a pan, a volume override, a mute
        // flag, and a surround flag — populated by importers whose
        // format expresses these in its header (S3M's
        // `channel_settings`, IT's `initial_channel_pan` /
        // `initial_channel_volume`). XM/MOD leave the vector empty
        // and every channel keeps its centre/full/unmuted/non-
        // surround default.
        for (i, ch) in channels.iter_mut().enumerate() {
            if let Some(d) = module.channel_defaults.get(i) {
                if let Some(p) = d.panning {
                    ch.set_initial_panning(p);
                }
                if let Some(v) = d.volume {
                    ch.set_initial_channel_volume(v);
                }
                if d.muted {
                    ch.set_initial_muted(true);
                }
                if d.surround {
                    ch.set_initial_surround(true);
                }
            }
        }
        // Deterministic per-channel seed so each channel has an
        // independent IT-humanisation stream while the whole render
        // stays bit-reproducible. High bits give us a non-zero base;
        // low bits distinguish channels.
        for (i, ch) in channels.iter_mut().enumerate() {
            ch.reseed_rng(0xA5A5_0000 | (i as u32 + 1));
            ch.set_track_index(i);
        }
        Self {
            sample_rate: sr,
            module,
            channel: channels,
            pool: VoicePool::new(voice_pool_capacity),
            global_volume: Volume::FULL,
            amplification: Amplification::UNITY,
            volume_slide_speed: Q15::ZERO,
            volume_slide_fine: true,
            row_has_global_volume_slide: false,
            pending_midi_events: Vec::new(),
            humanize_mode: crate::humanize::HumanizeMode::default(),
        }
    }

    pub(crate) fn set_humanize_mode(&mut self, mode: crate::humanize::HumanizeMode) {
        self.humanize_mode = mode;
        // Reseed per-channel humanize RNGs when a deterministic seed
        // is requested. Live / Disabled keep the default per-channel
        // seeds (deterministic across runs without explicit input).
        if let crate::humanize::HumanizeMode::Deterministic(seed) = mode {
            for (i, ch) in self.channel.iter_mut().enumerate() {
                ch.humanize_ekn.reseed(seed.wrapping_add(i as u32 + 1));
            }
        }
    }

    /// Trigger any fire-intent on each channel whose scheduled
    /// tick matches `current_tick`. Called per-tick by the player
    /// (Euclidean humanisation early-fire path).
    ///
    /// `abs_tick` / `song` are threaded so the channel can resolve
    /// its current Clip → Track and read the per-tick lanes.
    pub(crate) fn fire_humanize_intents_at(&mut self, current_tick: u8, abs_tick: u32, song: u16) {
        for ch in &mut self.channel {
            if let Some((cell, track_instr)) = ch.humanize_ekn.take_fire_intent_at(current_tick) {
                ch.tick0(&cell, track_instr, abs_tick, song, &mut self.pool);
            }
        }
    }

    /// Peek at the next row and, for every channel armed by a prior
    /// Euclidean trigger, roll humanise + schedule a fire-early
    /// intent inside the current row's tick run.
    ///
    /// `song` / `next_pat` / `next_row` / `speed` are read from the
    /// sequencer by the facade and threaded in. Pulses that would
    /// jump patterns can't humanise (out of scope for the simple
    /// lookahead).
    pub(crate) fn schedule_humanize_intents(
        &mut self,
        song: usize,
        next_pat: usize,
        next_row: usize,
        speed: usize,
    ) {
        if speed < 2 {
            // Need at least one non-row-start tick to fire early.
            return;
        }
        let Some(entry) = self
            .module
            .timeline_map
            .find_entry(song, next_pat, next_row)
        else {
            return;
        };
        let next_abs_tick = entry.tick;
        let next_cells = self.module.row_at(song, next_pat, next_row);
        let n_channels = self.channel.len();
        for ch_idx in 0..n_channels {
            let Some(ch) = self.channel.get_mut(ch_idx) else {
                continue;
            };
            // `front` is now a Track index — see
            // `state_humanize_ekn::StateHumanizeEkn::note_euclidian_triggered`.
            let Some(front_track) = ch.humanize_ekn.armed_front() else {
                continue;
            };
            // The next pulse's track on this channel: look up the
            // active clip at `next_abs_tick`.
            let next_track = self
                .module
                .clips
                .active_at(song as u16, ch_idx as u8, next_abs_tick)
                .map(|(_, c)| c.track as usize);
            if next_track != Some(front_track) {
                continue;
            }
            // Resolve humanise params on the front Track::Euclidean.
            let (prob, max_adv) = match self.module.tracks.get(front_track) {
                Some(xmrs::core::daw::track::Track::Euclidean {
                    humanize_probability,
                    humanize_advance_max_ticks,
                    ..
                }) => (*humanize_probability, *humanize_advance_max_ticks),
                _ => continue,
            };
            // Only humanise a real NoteOn trigger.
            let Some((cell, track_instr)) = next_cells.get(ch_idx) else {
                continue;
            };
            if !matches!(cell.event, xmrs::core::cell::CellEvent::NoteOn { .. }) {
                continue;
            }
            // Roll humanise. On hit, fire at tick (speed - advance)
            // inside the current row, clamped to [1, speed-1].
            if let Some(advance) = ch.humanize_ekn.roll(prob, max_adv) {
                let s = speed as u8;
                let fire_tick = s.saturating_sub(advance).max(1).min(s.saturating_sub(1));
                ch.humanize_ekn
                    .schedule_fire(fire_tick, cell.clone(), *track_instr);
            }
        }
    }

    // --- Accessors / mutators (used by the facade to expose public API) ---

    /// Q1.15 song-driven master volume.
    pub fn global_volume(&self) -> Volume {
        self.global_volume
    }
    /// Set song-driven master volume.
    pub fn set_global_volume(&mut self, v: Volume) {
        self.global_volume = v;
    }

    /// Q4.12 user-driven amplification (up to 8×).
    pub fn amplification(&self) -> Amplification {
        self.amplification
    }
    /// Set user-driven amplification.
    pub fn set_amplification(&mut self, a: Amplification) {
        self.amplification = a;
    }

    /// Output sample-rate in Hz.
    pub fn sample_rate(&self) -> SampleRate {
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
        self.global_volume = Volume::FULL;
        // Split borrow: take `&mut self.channel` and `&mut self.pool`
        // separately so the loop body can mutate both.
        let pool = &mut self.pool;
        for ch in &mut self.channel {
            ch.clear_ghosts(pool);
            ch.trigger_pitch(TRIGGER_KEEP_PERIOD, pool);
        }
    }

    // --- Row / tick dispatch ---

    /// Forward the cells of a newly loaded row to each channel, applying the
    /// volume-side global effects as we go.
    ///
    /// `abs_tick` is the playback's absolute song tick at row-start; `song`
    /// is the current sub-song index. Channels use them to resolve their
    /// active Clip + Track and read the per-tick lanes.
    pub(crate) fn process_row(
        &mut self,
        cells: &[(Cell, Option<usize>)],
        abs_tick: u32,
        song: u16,
    ) {
        // Song-level global volume / volume slide come from the
        // `GlobalVolume` AutomationLanes.
        self.apply_global_volume_from_lane(abs_tick);
        self.apply_global_volume_slide_from_lane(abs_tick);

        // MIDI event buffer is per-row — the facade drains it after
        // processing. Clearing here so leftovers from a previous row
        // don't re-emit.
        self.pending_midi_events.clear();

        let n = self.channel.len().min(cells.len());
        for (i, (cell, track_instr)) in cells.iter().enumerate().take(n) {
            // Euclidean humanisation: if a fire-early already
            // triggered this pulse, suppress the natural row-start
            // trigger. Global effects on the row still apply.
            if !self.channel[i].humanize_ekn.take_suppress() {
                self.channel[i].tick0(cell, *track_instr, abs_tick, song, &mut self.pool);
            }
            self.apply_row_per_channel_effects(i, cell);
        }
    }

    /// Per-channel cell effects that need to fire at row-start.
    /// Currently only `TrackEffect::MidiMacro` — the macro byte stream
    /// substitutes the channel's note / volume / pan / instrument
    /// fields and may write through to the filter envelope or MIDI
    /// output.
    fn apply_row_per_channel_effects(&mut self, ch_index: usize, cell: &Cell) {
        for fx in &cell.effects {
            if let TrackEffect::MidiMacro(macro_type) = fx {
                if let Some(ch) = self.channel.get_mut(ch_index) {
                    ch.apply_midi_macro(
                        macro_type.clone(),
                        ch_index,
                        &mut self.pending_midi_events,
                        &mut self.pool,
                    );
                }
            }
        }
    }

    /// Look up the song-level `GlobalVolume` `Points` lane and apply
    /// the latest set whose tick falls *exactly* on `abs_tick`. A
    /// `value_at` query would re-apply the latest stored point every
    /// row, which would clobber any in-flight volume slide; the
    /// strict-equality check matches the legacy "set only on the row
    /// that carries the Volume effect" semantic.
    fn apply_global_volume_from_lane(&mut self, abs_tick: u32) {
        use xmrs::core::daw::automation::{AutomationTarget, AutomationValue, LaneEvent};
        for lane in self.module.lanes_for(AutomationTarget::GlobalVolume) {
            for ev in lane.events_in(abs_tick, abs_tick.saturating_add(1)) {
                if let LaneEvent::Point(p) = ev {
                    if let AutomationValue::Normalized(q) = p.value {
                        self.global_volume = Volume::from_q15(q);
                    }
                }
            }
        }
    }

    /// Look up the song-level `GlobalVolume` `Slide` lane and latch
    /// the per-tick slide rate. Mirrors the sequencer's
    /// `latch_bpm_slide_from_lane`: when armed, write `speed` / `fine`
    /// and, for fine slides, apply once at tick 0; when unarmed,
    /// clear the latch so `process_tick` stops sliding.
    fn apply_global_volume_slide_from_lane(&mut self, abs_tick: u32) {
        use xmrs::core::daw::automation::AutomationTarget;
        let mut armed = false;
        for lane in self.module.lanes_for(AutomationTarget::GlobalVolume) {
            if let Some(state) = lane.slide_state_at(abs_tick) {
                if state.armed {
                    if state.fine {
                        // Fine slide fires once at tick 0.
                        self.global_volume = self.global_volume.with_tremolo(state.rate);
                    }
                    self.volume_slide_speed = state.rate;
                    self.volume_slide_fine = state.fine;
                    armed = true;
                }
            }
        }
        self.row_has_global_volume_slide = armed;
        if !armed {
            self.volume_slide_speed = Q15::ZERO;
        }
    }

    /// Drain all MIDI events emitted during the most recent row.
    /// Returns an iterator yielding `(source_channel, event)` tuples.
    /// The facade calls this after `process_row` and forwards each
    /// event to every registered [`crate::midi_observer::MidiObserver`].
    pub(crate) fn drain_midi_events(&mut self) -> alloc::vec::Drain<'_, (usize, MidiEvent)> {
        self.pending_midi_events.drain(..)
    }

    /// Advance every channel by one sustained (non-row-start) tick.
    /// `current_tick` is the sequencer's tick counter at the moment of
    /// the call — guaranteed to be >= 1.
    pub(crate) fn process_tick(&mut self, current_tick: usize) {
        let pool = &mut self.pool;
        for ch in &mut self.channel {
            ch.tick(current_tick, pool);
        }
        // Apply rolling global volume slide, if the current row carries a
        // non-fine one. Fine slides were already applied at row-start.
        if self.row_has_global_volume_slide && !self.volume_slide_fine {
            self.global_volume = self.global_volume.with_tremolo(self.volume_slide_speed);
        }
    }

    // --- Sample generation ---

    /// Fold each channel's sample into a single stereo output, applying the
    /// final mixer gain (`global_volume * amplification`) when requested.
    ///
    /// Hot path: walks channels once with an `i32` Q1.15
    /// accumulator and never allocates. Returns the
    /// **un-clamped** stereo pre-gain sum so the caller can
    /// apply final gain (Volume × Amplification × MixVolume)
    /// and clamp to `i16` in a single step without losing
    /// headroom that the gain stage would have brought back
    /// in range.
    ///
    /// `per_channel_out`, when `Some`, is filled with each
    /// channel's saturated `i16` pair (already clamped to the
    /// Q1.15 range — observers don't get the un-saturated
    /// accumulator; if a single channel ever saturates it's
    /// the channel's own gain mistake, not a mix-bus stacking
    /// issue).
    pub(crate) fn mix(&mut self, per_channel_out: Option<&mut [(i16, i16)]>) -> (i32, i32) {
        let mut left: i32 = 0;
        let mut right: i32 = 0;

        let pool = &mut self.pool;
        match per_channel_out {
            None => {
                for ch in &mut self.channel {
                    if let Some((l, r)) = ch.next_sample(pool) {
                        if !ch.is_muted() {
                            l.accumulate_into(&mut left);
                            r.accumulate_into(&mut right);
                        }
                    }
                }
            }
            Some(buf) => {
                for (idx, ch) in self.channel.iter_mut().enumerate() {
                    let val = match ch.next_sample(pool) {
                        Some((l, r)) if !ch.is_muted() => {
                            l.accumulate_into(&mut left);
                            r.accumulate_into(&mut right);
                            (l.as_q15_i16(), r.as_q15_i16())
                        }
                        _ => (0, 0),
                    };
                    if idx < buf.len() {
                        buf[idx] = val;
                    }
                }
            }
        }

        (left, right)
    }

    /// Apply `global_volume × amplification` to a pre-gain
    /// accumulator and saturate to `i16` Q1.15. Matches the OLD
    /// f32 iterator path's gain composition: `mix_volume` is
    /// kept in `self` for the rarely-used `voices.mix(true, _)`
    /// path but is **NOT** folded in here, because the OLD
    /// iterator path didn't apply it either — folding it now
    /// would attenuate the output by `module.mix_volume / 8`
    /// (≈ 21× quieter / −26 dB on XM modules where
    /// `mix_volume = 48/128`).
    pub(crate) fn apply_final_gain(&self, sum: (i32, i32)) -> (i16, i16) {
        let gv = self.global_volume.as_q15_i32() as i64; // Q1.15
        let amp = self.amplification.as_q4_12_i32() as i64; // Q4.12

        // Q1.15 × Q4.12 = Q5.27. Apply to sample (i32 Q1.15) →
        // Q6.42. Narrow `>> 27` round-half.
        let g_q27 = gv.max(0) * amp.max(0); // Q5.27

        let bias: i64 = 1 << 26;
        let apply = |s: i32| -> i16 {
            let prod: i64 = (s as i64).wrapping_mul(g_q27);
            let r = if prod >= 0 {
                (prod + bias) >> 27
            } else {
                -(((-prod) + bias) >> 27)
            };
            r.clamp(i16::MIN as i64, i16::MAX as i64) as i16
        };
        (apply(sum.0), apply(sum.1))
    }

    /// Saturate a pre-gain mix accumulator to Q1.15 `i16`
    /// without applying gain. Used for the pre-gain observer
    /// dispatch.
    #[inline]
    pub(crate) fn saturate_to_i16(sum: (i32, i32)) -> (i16, i16) {
        (
            sum.0.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
            sum.1.clamp(i16::MIN as i32, i16::MAX as i32) as i16,
        )
    }

    /// Return one `(left, right)` sample per channel, pre-mix,
    /// pre-gain. Q1.15 `i16` PCM. Allocates a `Vec`; used by
    /// the rarely-hit `XmrsPlayer::samples_from_channels` API
    /// for per-channel graphic effects.
    pub(crate) fn samples_from_channels(&mut self) -> Vec<(i16, i16)> {
        let pool = &mut self.pool;
        self.channel
            .iter_mut()
            .map(|ch| match ch.next_sample(pool) {
                Some((l, r)) if !ch.is_muted() => (l.as_q15_i16(), r.as_q15_i16()),
                _ => (0, 0),
            })
            .collect()
    }
}
