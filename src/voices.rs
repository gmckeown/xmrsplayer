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
// Only the coupled-SID driver is heap-allocated; `Box` is unused (and absent
// from the `no_std` prelude) when the SID synthesiser is not compiled in.
#[cfg(feature = "synth_sid")]
use alloc::boxed::Box;
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
    pub(crate) interpolation: crate::interpolation::Interpolation,

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
    /// The module's master mix volume (`Module::mix_volume`, Q1.15),
    /// read once at construction. Applied as a final constant gain in
    /// [`Voices::apply_final_gain`], alongside `global_volume` and
    /// `amplification`. `Volume::FULL` (identity) for formats that
    /// carry no master volume, so they are unaffected.
    mix_volume: Volume,

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

    /// RFC §3B — runtime return/aux bus chains, cloned from
    /// `module.buses` so stateful devices keep their state. Channels
    /// feed them via `module.channel_sends`; their processed output is
    /// summed into the master mix. Empty for every tracker import.
    bus_chains: Vec<xmrs::core::daw::device::DeviceChain>,
    /// RFC §3B — runtime master-bus insert chain, cloned from
    /// `module.master_chain`. Applied to the final summed mix before
    /// the output gain. Empty ⇒ pass-through ⇒ bit-identical.
    master_chain: xmrs::core::daw::device::DeviceChain,

    /// RFC §3C — runtime playback state for the module's audio clips.
    /// Empty for every tracker import.
    audio_clips: Vec<AudioClipRuntime>,

    /// Reusable scratch buffers for [`Self::mix`], kept alive across
    /// frames so the realtime audio path performs **no per-sample heap
    /// allocation**. `mix` runs once per output frame; allocating these
    /// each time churned the allocator on the audio thread (a latency /
    /// priority-inversion hazard) and, for every tracker import (no
    /// audio clips, no buses), was pure waste. They are `mem::take`-n
    /// out during `mix` to dodge the self-borrow against `self.channel`
    /// / `self.pool`, then restored before returning. Sized lazily via
    /// `clear()` + `resize(.., (0, 0))`, which is bit-identical to the
    /// former `vec![(0, 0); len]`.
    audio_inject_scratch: Vec<(i32, i32)>,
    bus_acc_scratch: Vec<(i32, i32)>,

    /// One shared OPL2 FM synthesiser, instantiated only when the module
    /// carries at least one `InstrumentType::Opl` instrument. `None` for
    /// every other module ⇒ the FM drive/render passes are measured
    /// no-ops ⇒ bit-identical. See `OPL_SYNTHESIS_RFC.md`.
    #[cfg(feature = "synth_opl")]
    opl: Option<xmrs::generators::opl::driver::OplDriver>,

    /// Per-instrument SID bank, instantiated only when the module carries at
    /// least one `InstrumentType::RobSid` instrument. `None` for every other
    /// module ⇒ the SID drive/fold passes are measured no-ops ⇒ bit-identical.
    #[cfg(feature = "synth_sid")]
    sid: Option<xmrs::generators::sid::driver::SidDriver>,
}

/// RFC §3C — runtime playback state for one audio clip on the
/// timeline. Empty `audio_clips` (every tracker import) ⇒ the audio
/// pass is a measured no-op ⇒ bit-identical.
struct AudioClipRuntime {
    /// Index into `module.tracks` — the [`Track::Audio`] this clip plays.
    track: usize,
    /// Mixer channel this clip is routed to. Its resampled output is
    /// injected **pre-insert** into that channel so it flows through the
    /// same insert chain + sends as the channel's notes. Out-of-range ⇒
    /// the clip falls back to a direct master sum.
    target_channel: u8,
    song: u16,
    position_tick: u32,
    end_tick: u32,
    /// Source→output resample step, Q32.32 (`1 << 32` = 1.0).
    step_q32: u64,
    /// Current source read position, Q32.32.
    cursor_q32: u64,
    active: bool,
}

/// RFC §3C — resample every active audio clip and route it into the
/// mixer. v2: a clip is injected **pre-insert** into its
/// `target_channel`'s accumulator (`inject`), so it flows through that
/// channel's insert chain *and* its sends — the full channel strip,
/// exactly like the channel's notes. A clip whose `target_channel` is
/// out of range falls back to a direct master sum (`master`) so its
/// audio is never silently dropped (v1 reach preserved). A muted
/// `Track::Audio` advances its cursor (so un-muting resumes in phase)
/// but emits nothing. Activation is tick-granular: the cursor restarts
/// at the clip region's first tick. A no-op when there are no clips.
fn accumulate_audio_clips(
    module: &Module,
    clips: &mut [AudioClipRuntime],
    abs_tick: u32,
    song: u16,
    inject: &mut [(i32, i32)],
    master: &mut (i32, i32),
) {
    for c in clips.iter_mut() {
        let active_now = c.song == song && abs_tick >= c.position_tick && abs_tick < c.end_tick;
        if !active_now {
            c.active = false;
            continue;
        }
        if !c.active {
            // Rising edge into the clip region: (re)start from the top.
            c.active = true;
            c.cursor_q32 = 0;
        }
        let Some(track) = module.tracks.get(c.track) else {
            c.active = false;
            continue;
        };
        // RFC §4: resolve through the module so a `Pooled` source reads
        // the shared asset; `Inline` resolves to its own sample.
        let Some((sample, _rate)) = module.track_audio(c.track) else {
            c.active = false;
            continue;
        };
        let floor = (c.cursor_q32 >> 32) as usize;
        let frac = (c.cursor_q32 & 0xFFFF_FFFF) as i64;
        // `seek` honours the sample's loop; `None` = past the end of a
        // one-shot ⇒ the clip has finished even if the region hasn't.
        let Some(i0) = sample.seek(floor, false) else {
            c.active = false;
            continue;
        };
        let i1 = sample.seek(floor + 1, false).unwrap_or(i0);
        let (l0, r0) = sample.at(i0);
        let (l1, r1) = sample.at(i1);
        let lerp =
            |a: i16, b: i16| -> i32 { a as i32 + (((b as i64 - a as i64) * frac) >> 32) as i32 };
        let l = lerp(l0.as_q15_i16(), l1.as_q15_i16());
        let r = lerp(r0.as_q15_i16(), r1.as_q15_i16());
        // Advance the cursor whether or not the track is muted, so
        // un-muting resumes at the position the clip would have reached.
        c.cursor_q32 = c.cursor_q32.wrapping_add(c.step_q32);
        if track.muted() {
            continue;
        }
        match inject.get_mut(c.target_channel as usize) {
            Some(slot) => {
                slot.0 = slot.0.saturating_add(l);
                slot.1 = slot.1.saturating_add(r);
            }
            None => {
                master.0 = master.0.saturating_add(l);
                master.1 = master.1.saturating_add(r);
            }
        }
    }
}

/// RFC §3B — add channel `idx`'s post-insert stereo sample, scaled by
/// each of its sends, into the corresponding return-bus accumulators.
/// A no-op for every tracker import (`channel_sends` empty).
#[inline]
fn route_sends(module: &Module, idx: usize, l: Amp, r: Amp, bus_acc: &mut [(i32, i32)]) {
    if let Some(sends) = module.channel_sends.get(idx) {
        for s in sends {
            if let Some(acc) = bus_acc.get_mut(s.bus as usize) {
                s.level.apply(l).accumulate_into(&mut acc.0);
                s.level.apply(r).accumulate_into(&mut acc.1);
            }
        }
    }
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
        // RFC §3C: precompute the runtime state for every audio clip
        // (a clip whose track is a `Track::Audio`). Empty for every
        // tracker import, so the audio pass in `mix` is a no-op.
        let out_rate = sr.hz().max(1) as u64;
        let audio_clips: Vec<AudioClipRuntime> = module
            .clips
            .iter()
            .filter_map(|c| {
                // RFC §4: pool-aware — resolves `Inline` and `Pooled`.
                let (_sample, rate) = module.track_audio(c.track as usize)?;
                let step_q32 = if rate == 0 || rate as u64 == out_rate {
                    1u64 << 32
                } else {
                    ((rate as u64) << 32) / out_rate
                };
                Some(AudioClipRuntime {
                    track: c.track as usize,
                    target_channel: c.target_channel,
                    song: c.song,
                    position_tick: c.position_tick,
                    end_tick: c.end_tick,
                    step_q32,
                    cursor_q32: 0,
                    active: false,
                })
            })
            .collect();
        // Captured before the struct literal moves `channels` into `channel`.
        #[cfg(feature = "synth_sid")]
        let num_channels = channels.len();
        let mut voices = Self {
            sample_rate: sr,
            interpolation: crate::interpolation::Interpolation::default(),
            module,
            channel: channels,
            pool: VoicePool::new(voice_pool_capacity),
            global_volume: Volume::FULL,
            amplification: Amplification::UNITY,
            mix_volume: module.mix_volume,
            volume_slide_speed: Q15::ZERO,
            volume_slide_fine: true,
            row_has_global_volume_slide: false,
            pending_midi_events: Vec::new(),
            humanize_mode: crate::humanize::HumanizeMode::default(),
            // RFC §3B: runtime clones of the bus / master chains
            // (empty for every tracker import).
            bus_chains: module.buses.iter().map(|b| b.chain.clone()).collect(),
            master_chain: module.master_chain.clone(),
            audio_clips,
            audio_inject_scratch: Vec::new(),
            bus_acc_scratch: Vec::new(),
            // OPL chip only when the module actually uses FM instruments.
            #[cfg(feature = "synth_opl")]
            opl: module
                .instrument
                .iter()
                .any(|i| matches!(i.instr_type, xmrs::prelude::InstrumentType::Opl(_)))
                .then(|| xmrs::generators::opl::driver::OplDriver::new(sr.hz().max(1))),
            // SID bank only when the module actually uses SID instruments.
            // Default model/region (6581 / PAL); the PSID/RSID header hint is
            // a later refinement.
            #[cfg(feature = "synth_sid")]
            sid: module
                .instrument
                .iter()
                .any(|i| matches!(i.instr_type, xmrs::prelude::InstrumentType::RobSid(_)))
                .then(|| {
                    use xmrs::generators::sid::{
                        coupled::CoupledSid,
                        driver::{SidBank, SidDriver},
                        SidModel, SidRegion,
                    };
                    use xmrs::prelude::InstrumentType;
                    let rate = sr.hz().max(1);
                    // Ticks-per-row → drum noise burst. `initial_tempo`, not
                    // `module.default_tempo`: the latter is sub-song 0's, and a
                    // `.sid` holding several tunes gives them different row
                    // cadences (Commando runs [3, 4, 3]). Reading the module
                    // scalar made every sub-song's drums as long as the first
                    // one's. The caller resolves it per sub-song.
                    let speed = initial_tempo as u8;
                    // Coupled (shared-chip) model when the song's voices
                    // hard-sync / ring-modulate each other OR any instrument
                    // drives the global SID filter — both need the 3 SID voices
                    // on one chip. Otherwise the per-instrument model (the
                    // default, all other tunes byte-identical).
                    let uses_coupled = module.instrument.iter().any(|i| {
                        if let InstrumentType::RobSid(irs) = &i.instr_type {
                            let v = &irs.voice;
                            v.ctrl_sync || v.ctrl_rm || irs.fx.filter.enable
                        } else {
                            false
                        }
                    });
                    if uses_coupled {
                        SidDriver::Coupled(Box::new(CoupledSid::new(
                            rate,
                            SidModel::Mos6581,
                            SidRegion::Pal,
                            speed,
                        )))
                    } else {
                        SidDriver::PerInstrument(SidBank::new(
                            rate,
                            module.instrument.len(),
                            num_channels,
                            speed,
                            SidModel::Mos6581,
                            SidRegion::Pal,
                        ))
                    }
                }),
        };
        // Native DSP devices: prepare the bus / master chains for the
        // output rate (size buffers, precompute coeffs). Per-channel
        // insert chains are prepared in `Channel::set_track_index`. No-op
        // for the empty chains every tracker import produces.
        let rate_hz = sr.hz();
        for chain in &mut voices.bus_chains {
            chain.prepare(rate_hz);
        }
        voices.master_chain.prepare(rate_hz);
        voices
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

    /// Sample interpolation mode (point sampling vs linear).
    pub fn interpolation(&self) -> crate::interpolation::Interpolation {
        self.interpolation
    }
    /// Set sample interpolation mode across all channels and active voices.
    pub fn set_interpolation(&mut self, interpolation: crate::interpolation::Interpolation) {
        self.interpolation = interpolation;
        for c in &mut self.channel {
            c.set_interpolation(interpolation);
        }
        self.pool.set_interpolation(interpolation);
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

    /// Per-channel realized runtime snapshot (period / volume / gate).
    /// See [`Channel::snapshot`].
    pub fn channel_snapshots(&self) -> alloc::vec::Vec<crate::channel::ChannelSnapshot> {
        self.channel.iter().map(|c| c.snapshot()).collect()
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
    pub(crate) fn allocated_voice_count(&self) -> usize {
        self.pool.allocated_count()
    }

    pub(crate) fn reset_for_goto(&mut self) {
        self.global_volume = Volume::FULL;
        // Split borrow: take `&mut self.channel` and `&mut self.pool`
        // separately so the loop body can mutate both.
        let pool = &mut self.pool;
        for ch in &mut self.channel {
            ch.clear_ghosts(pool);
            ch.trigger_pitch(TRIGGER_KEEP_PERIOD, pool);
            // Native DSP devices: a seek must clear insert-chain tails
            // (delay/reverb/filter history) so the jump is deterministic.
            // No-op for the empty chains every tracker import produces.
            ch.reset_insert_chain();
        }
        // RFC §3C: a seek invalidates the free-running audio-clip
        // cursors — they track *continuous* playback, so a jump would
        // leave them desynced from the timeline (the clip would keep
        // reading from a stale source position). Reset them so each
        // clip re-enters its region cleanly from the top on the next
        // tick that lands inside it. No audio clips ⇒ no-op.
        for c in &mut self.audio_clips {
            c.active = false;
            c.cursor_q32 = 0;
        }
        // Native DSP devices: clear bus / master tails on the same seek.
        for chain in &mut self.bus_chains {
            chain.reset();
        }
        self.master_chain.reset();
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
        self.apply_global_volume_from_lane(abs_tick, song);
        self.apply_global_volume_slide_from_lane(abs_tick, song);

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
        self.drive_opl();
        self.drive_sid();
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
    fn apply_global_volume_from_lane(&mut self, abs_tick: u32, song: u16) {
        use xmrs::core::daw::automation::{AutomationTarget, AutomationValue, LaneEvent};
        for lane in self.module.lanes_for(AutomationTarget::GlobalVolume, song) {
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
    fn apply_global_volume_slide_from_lane(&mut self, abs_tick: u32, song: u16) {
        use xmrs::core::daw::automation::AutomationTarget;
        let mut armed = false;
        for lane in self.module.lanes_for(AutomationTarget::GlobalVolume, song) {
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
        self.drive_opl();
        self.drive_sid();
    }

    /// Push each channel's current OPL gesture (trigger / key-off / cut, or
    /// a held note's live pitch / volume / pan) into the shared chip.
    /// No-op unless the module instantiated an `OplDriver`.
    #[cfg(feature = "synth_opl")]
    fn drive_opl(&mut self) {
        use crate::channel::opl::OplEdge;
        if self.opl.is_none() {
            return;
        }
        let driver = self.opl.as_mut().unwrap();
        let module = self.module;
        for (i, ch) in self.channel.iter_mut().enumerate() {
            let Some(upd) = ch.take_opl_update() else {
                continue;
            };
            match upd.edge {
                OplEdge::Cut => driver.note_cut(i),
                OplEdge::KeyOff => driver.note_off(i),
                OplEdge::Trigger => {
                    if let Some(h) = &upd.held {
                        if let Some(xmrs::prelude::InstrumentType::Opl(o)) =
                            module.instrument.get(h.instr).map(|x| &x.instr_type)
                        {
                            driver.note_on(i, o, h.milli_hz, h.vol63, h.pan_l, h.pan_r);
                        }
                    }
                }
                OplEdge::None => {
                    if let Some(h) = &upd.held {
                        driver.set_frequency(i, h.milli_hz);
                        driver.set_volume(i, h.vol63);
                        driver.set_pan(i, h.pan_l, h.pan_r);
                    }
                }
            }
        }
    }

    #[cfg(not(feature = "synth_opl"))]
    #[inline]
    fn drive_opl(&mut self) {}

    /// Push each channel's current SID gesture (trigger / key-off / cut, or a
    /// held note's live pitch) into the per-instrument bank. No-op unless the
    /// module instantiated a `SidBank`.
    #[cfg(feature = "synth_sid")]
    fn drive_sid(&mut self) {
        use crate::channel::sid::SidEdge;
        use xmrs::core::effect::ArmMode;
        use xmrs::core::instr_robsid::{ArpMode, ArpPhase, VibratoMode};
        if self.sid.is_none() {
            return;
        }
        let bank = self.sid.as_mut().unwrap();
        // Advance the replayer's global VBlank counter once per frame (drives
        // the shared vibrato / arpeggio phase).
        bank.begin_frame();
        for (i, ch) in self.channel.iter_mut().enumerate() {
            // Honour per-channel mute for the SID exactly like the sample
            // mixer does for ordinary voices. The chip is folded as a whole
            // by `mix_frame`, so there is no per-voice gain to zero at mix
            // time — instead we hard-cut a muted channel's SID voice every
            // frame, which lets `-c <n>` / `set_mute_channel` solo a single
            // SID voice for debugging by ear. We still drain the pending
            // update (`take_sid_update` advances the freq-slide accumulator)
            // so the channel's slide state stays consistent if it is later
            // unmuted; an unmuted voice resumes sounding on its next trigger.
            if ch.is_muted() {
                let _ = ch.take_sid_update();
                bank.note_cut(i);
                continue;
            }
            let Some(upd) = ch.take_sid_update() else {
                continue;
            };
            match upd.edge {
                SidEdge::Cut => bank.note_cut(i),
                SidEdge::KeyOff => {
                    // Ordinary note-end release: the replayer may "kill adsr"
                    // (gate-off + AD/SR=0) on this path for v10/v15 — is_fetch=false.
                    bank.note_off(i, false);
                    // The voice keeps sounding through its release — keep its
                    // per-frame modulation advancing.
                    bank.advance_fx(i);
                }
                SidEdge::KeyOffFetch => {
                    // Appended note-stream entry = a note-FETCH frame: the
                    // replayer skips the per-frame effects (PW sweep / vibrato /
                    // drum HOLD this frame), like a trigger. Release the gate but
                    // do NOT advance the effects — and NEVER zero AD/SR (the
                    // replayer re-fetches them from the instrument here, so a long
                    // note rings out on its release nibble). is_fetch=true.
                    bank.note_off(i, true);
                }
                SidEdge::Trigger => {
                    if let Some(h) = &upd.held {
                        if let Some(xmrs::prelude::InstrumentType::RobSid(irs)) =
                            self.module.instrument.get(h.instr).map(|x| &x.instr_type)
                        {
                            // `TrackEffect::NoteArming`: this note may ask for
                            // the instrument's modulators NOT to be re-armed, or
                            // to be armed elsewhere. `RobEffects` is `Copy`, so
                            // the override is a local tweak of the patch for this
                            // trigger — the instrument itself is untouched and
                            // the driver needs no new argument.
                            let mut fx = irs.fx;
                            let mut legato = false;
                            if let Some(a) = upd.arming {
                                legato = a.legato;
                                match a.filter {
                                    ArmMode::Reseed => {}
                                    ArmMode::Hold => fx.filter.reseed_each_note = false,
                                    ArmMode::At(v) => {
                                        fx.filter.cutoff_seed = v;
                                        fx.filter.reseed_each_note = true;
                                    }
                                }
                                match a.pulse_width {
                                    ArmMode::Reseed => {}
                                    ArmMode::Hold | ArmMode::At(_) => {
                                        fx.pulse_sweep.reseed_on_note = false
                                    }
                                }
                                // The per-note arpeggio. The replayer keeps its
                                // phase in a per-voice slot reset at note-on and
                                // steps it every FRAME for the note's whole
                                // length, so it belongs on the chip's own
                                // arpeggio (`ArpPhase::PerNote`), not on the
                                // row-tick tracker one. A zero low nibble
                                // shortens the cycle to two steps: the replayer
                                // wraps its phase counter at 2 (Ghidra
                                // `lh_play_voice_loop $10b7`), which is what
                                // makes an octave default shimmer every frame.
                                if let ArmMode::At(n) = a.arpeggio {
                                    let hi = ((n >> 4) & 0x0F) as i8;
                                    let lo = (n & 0x0F) as i8;
                                    fx.arpeggio = ArpMode::Cycle {
                                        steps: [0, hi, lo],
                                        len: if lo == 0 { 2 } else { 3 },
                                        phase: ArpPhase::PerNote,
                                        follow_climb: false,
                                    };
                                }
                                // A per-note vibrato replaces the patch's swing
                                // AND starts immediately: the marker suppresses
                                // the reload of the instrument's start delay.
                                if let ArmMode::At(v) = a.vibrato {
                                    let v = v as u8;
                                    if let VibratoMode::Semitone { flat, .. } = fx.vibrato {
                                        fx.vibrato = VibratoMode::Semitone {
                                            half_depth: (v & 0x78) >> 3,
                                            shift: v & 0x07,
                                            delay: 0,
                                            flat,
                                        };
                                    }
                                }
                            }
                            if legato {
                                // A tie: the voice keeps its envelope, its gate
                                // and every running sweep — only the pitch
                                // moves. Re-triggering here would invent an
                                // attack the score does not ask for.
                                //
                                // And no effect advance either: a tie is still a
                                // pattern-entry FETCH frame, and the replayer's
                                // fetch block ends in `goto lh_write_voice_regs`
                                // whatever the entry is, jumping over the whole
                                // effect chain. Confirmed against the replayer's
                                // own work-RAM (`sidplay --statedump`): the
                                // vibrato counter freezes on 151 of Lion_Heart
                                // voice 2's 166 note-fetches, and the remaining
                                // freezes fall on rests and ties.
                                //
                                // But a tie CAN change the instrument, and then
                                // the timbre must follow without the envelope
                                // restarting — see `retie`. Radio_ACE does it 12
                                // times, Sun_Never_Shines 9, Lakers once; the
                                // other three tunes never, so for them this is a
                                // no-op. Not doing it left Radio_ACE's voice 1
                                // holding a pulse through 90 frames the chip
                                // spent on triangle + ring modulation.
                                bank.retie(i, h.instr, &irs.voice, &fx, irs.program.as_ref());
                                bank.set_frequency(i, h.milli_hz);
                            } else {
                                // Trigger frame: no effect advance (state starts
                                // at frame 0).
                                bank.note_on(
                                    i,
                                    h.instr,
                                    &irs.voice,
                                    &fx,
                                    irs.program.as_ref(),
                                    upd.arming.map_or(0, |a| a.release_in),
                                    h.milli_hz,
                                );
                            }
                        }
                    }
                }
                SidEdge::None => {
                    if let Some(h) = &upd.held {
                        // Held note, one frame elapses: refresh the tracker
                        // pitch, then advance the Rob-Hubbard modulation.
                        bank.set_frequency(i, h.milli_hz);
                        bank.advance_fx(i);
                    }
                }
            }
        }
        // Diagnostic: snapshot this frame's SID register state (no-op unless a
        // capture is active).
        bank.capture_frame();
    }

    #[cfg(not(feature = "synth_sid"))]
    #[inline]
    fn drive_sid(&mut self) {}

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
    pub(crate) fn mix(
        &mut self,
        abs_tick: u32,
        song: u16,
        per_channel_out: Option<&mut [(i16, i16)]>,
    ) -> (i32, i32) {
        let mut left: i32 = 0;
        let mut right: i32 = 0;

        // RFC §3B: per-return-bus input accumulators. Empty for every
        // tracker import (no buses), so the send routing and the
        // bus/master passes below are measured no-ops ⇒ bit-identical.
        // Reuse a persistent buffer (taken out to dodge the self-borrow
        // below) instead of allocating per frame on the audio thread.
        let mut bus_acc = core::mem::take(&mut self.bus_acc_scratch);
        bus_acc.clear();
        bus_acc.resize(self.bus_chains.len(), (0, 0));
        let module = self.module;

        // RFC §3C: resolve the audio clips for this tick into a
        // per-channel **pre-insert** injection (so each clip runs its
        // target channel's insert chain + sends, just like that
        // channel's notes). `master_extra` collects clips whose target
        // channel is out of range (direct master sum, v1 reach). Empty
        // `audio_clips` (every tracker import) ⇒ all-zero injection ⇒
        // `next_sample_with_inject` ≡ `next_sample` ⇒ bit-identical.
        let mut audio_inject = core::mem::take(&mut self.audio_inject_scratch);
        audio_inject.clear();
        audio_inject.resize(self.channel.len(), (0, 0));
        let mut master_extra: (i32, i32) = (0, 0);
        if !self.audio_clips.is_empty() {
            accumulate_audio_clips(
                module,
                &mut self.audio_clips,
                abs_tick,
                song,
                &mut audio_inject,
                &mut master_extra,
            );
        }

        let pool = &mut self.pool;
        match per_channel_out {
            None => {
                for (idx, ch) in self.channel.iter_mut().enumerate() {
                    let inj = audio_inject.get(idx).copied().unwrap_or((0, 0));
                    if let Some((l, r)) = ch.next_sample_with_inject(pool, inj) {
                        if !ch.is_muted() {
                            l.accumulate_into(&mut left);
                            r.accumulate_into(&mut right);
                            route_sends(module, idx, l, r, &mut bus_acc);
                        }
                    }
                }
            }
            Some(buf) => {
                for (idx, ch) in self.channel.iter_mut().enumerate() {
                    let inj = audio_inject.get(idx).copied().unwrap_or((0, 0));
                    let val = match ch.next_sample_with_inject(pool, inj) {
                        Some((l, r)) if !ch.is_muted() => {
                            l.accumulate_into(&mut left);
                            r.accumulate_into(&mut right);
                            route_sends(module, idx, l, r, &mut bus_acc);
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

        // OPL (FM): render one chip frame and fold it into the master mix
        // (RFC Phase A — master-summed; per-channel routing is Phase B).
        // `None`/idle chip ⇒ skipped ⇒ bit-identical. The chip advances one
        // step per output frame, matching `mix`'s per-frame cadence.
        #[cfg(feature = "synth_opl")]
        if let Some(opl) = self.opl.as_mut() {
            if opl.any_active() {
                // Operator output is now ±4084 (the chip's 12-bit width); lift
                // into the Q1.15 mix domain. Gain calibrated against Schism's
                // OPL render of `and_this_is_ultrasound.s3m`: the level lands
                // within ~1 dB at ×9/4 (the previous ±1024-scale ×9, divided by
                // 4 for the 4× wider operator output).
                const OPL_OUTPUT_GAIN: i32 = 9;
                let (ol, or) = opl.render_frame();
                left = left.saturating_add(ol.saturating_mul(OPL_OUTPUT_GAIN) >> 2);
                right = right.saturating_add(or.saturating_mul(OPL_OUTPUT_GAIN) >> 2);
            }
        }

        // SID: advance every active per-instrument chip one frame and fold the
        // summed mono mix into the master (Phase A — master-summed; per-voice
        // routing is Phase B). Each chip's `mix` is already an i16-range,
        // 3-voice-normalised value, so summing ≤3 active chips lands near
        // full-scale — folded into the Q1.15 accumulator directly (the master
        // clamp catches the rare 3-loud-voice peak). `None`/idle ⇒ skipped ⇒
        // bit-identical.
        #[cfg(feature = "synth_sid")]
        if let Some(sid) = self.sid.as_mut() {
            if sid.any_active() {
                let s = sid.mix_frame();
                left = left.saturating_add(s);
                right = right.saturating_add(s);
            }
        }

        // RFC §3B: run each return bus's accumulated input through its
        // chain and sum back into the master mix. No buses ⇒ skipped.
        for (i, chain) in self.bus_chains.iter_mut().enumerate() {
            let input = (
                Amp::from_q15_i32_sat(bus_acc[i].0),
                Amp::from_q15_i32_sat(bus_acc[i].1),
            );
            let out = if chain.is_active() {
                chain.process(input)
            } else {
                input
            };
            out.0.accumulate_into(&mut left);
            out.1.accumulate_into(&mut right);
        }

        // RFC §3C: audio clips with an out-of-range target channel were
        // collected into `master_extra` above — fold them straight into
        // the master accumulator (before the master chain, so they still
        // get master processing). In-range clips already reached here via
        // their channel strip. (0, 0) for every tracker import.
        left = left.saturating_add(master_extra.0);
        right = right.saturating_add(master_extra.1);

        // RFC §3B: master-bus insert chain on the final summed mix.
        // Empty ⇒ left/right untouched ⇒ bit-identical to the pre-3B
        // accumulator that `apply_final_gain` consumes.
        if self.master_chain.is_active() {
            let m = self
                .master_chain
                .process((Amp::from_q15_i32_sat(left), Amp::from_q15_i32_sat(right)));
            left = m.0.as_q15_i16() as i32;
            right = m.1.as_q15_i16() as i32;
        }

        // Return the scratch buffers to their fields so next frame
        // reuses their capacity (no further allocation after warm-up).
        self.bus_acc_scratch = bus_acc;
        self.audio_inject_scratch = audio_inject;

        (left, right)
    }

    /// Apply `global_volume × mix_volume × amplification` to a
    /// pre-gain accumulator and saturate to `i16` Q1.15.
    ///
    /// `mix_volume` is the module's master mix scaling (IT/S3M read it
    /// from the file; XM/MOD use the format's conventional headroom;
    /// other formats leave it at `Volume::FULL` = identity, so they are
    /// unaffected). It is folded into the `global_volume` factor while
    /// both are still ≤ 1 in Q1.15, which keeps the multiply in the same
    /// bit budget as a two-factor gain (no overflow) and, crucially,
    /// attenuates **before** the i16 clamp so a hot mix can't clip first
    /// and then be scaled down into distortion.
    pub(crate) fn apply_final_gain(&self, sum: (i32, i32)) -> (i16, i16) {
        let gv = self.global_volume.as_q15_i32() as i64; // Q1.15
        let mv = self.mix_volume.as_q15_i32() as i64; // Q1.15
        let amp = self.amplification.as_q4_12_i32() as i64; // Q4.12

        // Fold the two ≤1 factors first, staying in Q1.15 (round-half):
        // `global_volume × mix_volume`. Then × amplification → Q5.27,
        // applied to the Q1.15 sample and narrowed `>> 27`.
        let gvm = (gv.max(0) * mv.max(0) + (1 << 14)) >> 15; // Q1.15
        let g_q27 = gvm * amp.max(0); // Q5.27

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
