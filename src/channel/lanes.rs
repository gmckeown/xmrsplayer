//! AutomationLane queries + LFO arming / advancing + per-tick
//! Slide / Glide application from the DAW timeline.

use xmrs::core::daw::automation::AutomationTarget;
use xmrs::core::fixed::units::{Period, PitchDelta};
use xmrs::core::waveform::WaveformState;

use super::Channel;

impl<'a> Channel<'a> {
    /// Arm the three per-Track LFO state machines (Vibrato,
    /// Tremolo, Panbrello) from their `AutomationLane`s. Called
    /// at every row-start tick (`Channel::tick0`) **before**
    /// `tickn_effects(0, pool)` so the cell-side arms see the
    /// already-armed `effect_*.data`. The cell-side arms no
    /// longer write the LFO params — they only drive the
    /// per-tick `.tick_q()` advance and the format-specific
    /// quirks (FT2 vol-col-B, IT old-effects depth doubling,
    /// IT 2.14 row-zero tick).
    ///
    /// Skipped on `NoteDelay` rows since the cell-side effects
    /// (and the LFO arm) wait for the delay tick.
    pub(super) fn arm_lfos_from_lanes(&mut self, abs_tick: u32, song: u16) {
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };

        // ---- Vibrato (TrackPitch) ---------------------------------------
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            if state.armed {
                let depth = if self.module.quirks.it_old_effects {
                    state.depth.raw().saturating_mul(2)
                } else {
                    state.depth.raw()
                };
                self.effect_vibrato.data.speed = state.speed;
                self.effect_vibrato.data.depth = depth;
                self.effect_vibrato.data.waveform = WaveformState::new(state.waveform);
                self.vibrato_retrig_on_new_note = state.retrig;
            }
        }

        // ---- Tremolo (TrackVolume) --------------------------------------
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackVolume(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            if state.armed {
                self.effect_tremolo.data.speed = state.speed;
                self.effect_tremolo.data.depth = state.depth.raw();
                self.effect_tremolo.data.waveform = WaveformState::new(state.waveform);
                self.tremolo_retrig_on_new_note = state.retrig;
            }
        }

        // ---- Panbrello (TrackPanning) -----------------------------------
        // The "newly armed" gate avoids re-zeroing the phase on
        // every row where the lane stays armed (matches tracker
        // semantics where the cell arm only fires when the row
        // carries the main effect).
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPanning(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick))
        {
            let row_has_main =
                self.lane_has_lfo_set_at(AutomationTarget::TrackPanning(track_idx), abs_tick);
            if state.armed && row_has_main {
                self.effect_panbrello.data.speed = state.speed;
                self.effect_panbrello.data.depth = state.depth.raw();
                self.effect_panbrello.data.waveform = WaveformState::new(state.waveform);
                self.effect_panbrello.retrigger_q();
            }
        }
    }

    /// Active-clip → Track index lookup. Used by
    /// [`Self::arm_lfos_from_lanes`].
    #[inline]
    pub(super) fn lookup_track_idx(&self, abs_tick: u32, song: u16) -> Option<u32> {
        self.module
            .clips
            .active_at(song, self.track_index as u8, abs_tick)
            .map(|(_, c)| c.track)
    }

    /// `lookup_track_idx` resolved at the row's cached
    /// `(abs_tick, song)`. Convenience for the `row_has_*` helpers.
    #[inline]
    fn current_track_idx(&self) -> Option<u32> {
        self.lookup_track_idx(self.current_abs_tick, self.current_song_idx)
    }

    /// The active **held pitch offset** (semitones) from any
    /// `LaneKind::Points` lane on the current Track's `TrackPitch`
    /// target, evaluated at the current tick.
    ///
    /// `Points` lanes on `TrackPitch` carry an *absolute, latched*
    /// transpose ([`AutomationValue::Pitch`]) — as opposed to the
    /// relative `Lfo` (vibrato) / `Slide` / `Glide` modulations that
    /// also live on `TrackPitch`. [`AutomationLane::value_at`] holds
    /// the latest point's value forward, so a transpose set mid-song
    /// re-pitches even a sustained note every tick — the behaviour a
    /// per-note instrument pitch-envelope can't give (the curve is on
    /// the channel/Track timeline, not the note). The DW importer uses
    /// this for the replayer's global transpose (which it recomputes
    /// into every channel's base period each frame). Returns `ZERO`
    /// when no such lane/point exists, so it is a measured no-op for
    /// tracker formats (XM/IT/MOD never emit `Pitch` points).
    #[inline]
    pub(super) fn track_pitch_points_delta(&self) -> PitchDelta {
        use xmrs::core::daw::automation::AutomationValue;
        let Some(track_idx) = self.current_track_idx() else {
            return PitchDelta::ZERO;
        };
        // `Points` on `TrackPitch` (the DW per-frame arpeggio) can
        // change *within* a row, so evaluate at the absolute per-frame
        // tick = row start + within-row tick. (`current_abs_tick` alone
        // is the row's start and would freeze the arpeggio to row
        // resolution.) Other `TrackPitch` lanes are row-triggered and
        // unaffected — they don't use this helper.
        let abs_tick = self
            .current_abs_tick
            .saturating_add(self.current_tick_in_row as u32);
        let mut delta = PitchDelta::ZERO;
        for lane in self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
        {
            if let Some(AutomationValue::Pitch(d)) = lane.value_at(abs_tick) {
                delta = delta + d;
            }
        }
        delta
    }

    /// `true` when an `AutomationLane` of `target` has an
    /// `LfoEvent::Set` whose tick exactly equals `tick`. The
    /// cell-side quirks that asked "does this row carry a main
    /// `Vibrato` / `Tremolo` / `Panbrello`?" delegate here.
    #[inline]
    fn lane_has_lfo_set_at(&self, target: AutomationTarget, tick: u32) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, LfoEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Lfo(LfoEvent::Set { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has an
    /// `LfoEvent::DepthOnly` whose tick exactly equals `tick`.
    /// Stand-in for the legacy "row carries a standalone
    /// `VibratoDepth`" check (FT2 vol-col-B quirk).
    #[inline]
    fn lane_has_lfo_depthonly_at(&self, target: AutomationTarget, tick: u32) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, LfoEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Lfo(LfoEvent::DepthOnly { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has a
    /// `SlideEvent::Set` whose tick exactly equals `tick`. Used
    /// for the FT2 `InstrReset` quirk (`has_volume_slide`-style)
    /// and similar row-carries-a-slide questions.
    #[inline]
    fn lane_has_slide_set_at(&self, target: AutomationTarget, tick: u32) -> bool {
        use xmrs::core::daw::automation::{LaneEvent, SlideEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Slide(SlideEvent::Set { .. })))
        })
    }

    /// `true` when an `AutomationLane` of `target` has a
    /// `GlideEvent::Set` whose tick exactly equals `tick`.
    /// Stand-in for the legacy "row carries TonePortamento" check.
    #[inline]
    fn lane_has_glide_set_at(&self, target: AutomationTarget, tick: u32) -> bool {
        use xmrs::core::daw::automation::{GlideEvent, LaneEvent};
        self.module.lanes_for(target).any(|l| {
            l.events_in(tick, tick.saturating_add(1))
                .iter()
                .any(|e| matches!(e, LaneEvent::Glide(GlideEvent::Set { .. })))
        })
    }

    /// Convenience: "row carries TonePortamento on the active
    /// Track's `TrackPitch` lane".
    #[inline]
    pub(super) fn row_has_tone_portamento_lane(&self) -> bool {
        self.current_track_idx().is_some_and(|t| {
            self.lane_has_glide_set_at(AutomationTarget::TrackPitch(t), self.current_abs_tick)
        })
    }

    /// Convenience: "row carries a main `Vibrato` on the active
    /// Track's `TrackPitch` lane".
    #[inline]
    pub(super) fn row_has_vibrato_lane(&self) -> bool {
        self.current_track_idx().is_some_and(|t| {
            self.lane_has_lfo_set_at(AutomationTarget::TrackPitch(t), self.current_abs_tick)
        })
    }

    /// Convenience: "row carries a standalone `VibratoDepth` on
    /// the active Track's `TrackPitch` lane" — XM vol-col B quirk.
    #[inline]
    pub(super) fn row_has_vibrato_depth_only_lane(&self) -> bool {
        self.current_track_idx().is_some_and(|t| {
            self.lane_has_lfo_depthonly_at(AutomationTarget::TrackPitch(t), self.current_abs_tick)
        })
    }

    /// Convenience: "row carries a `VolumeSlide` on the active
    /// Track's `TrackVolume` lane".
    #[inline]
    pub(super) fn row_has_volume_slide_lane(&self) -> bool {
        self.current_track_idx().is_some_and(|t| {
            self.lane_has_slide_set_at(AutomationTarget::TrackVolume(t), self.current_abs_tick)
        })
    }

    /// Advance the per-Track LFO state machines (Vibrato /
    /// Tremolo / Panbrello) on every tick where the corresponding
    /// lane is armed.
    ///
    /// Special cases honoured:
    /// * **Vibrato @ tick 0** advances only under the IT 2.14
    ///   `it_vibrato_ticks_at_row_zero` quirk (and only when
    ///   `it_old_effects` is off).
    /// * **FT2 vol-col-B** (`volcol_b_advances_vibrato`): a row
    ///   that carries a standalone `VibratoDepth` (no main
    ///   `Vibrato` Set) keeps the LFO advancing on `tick > 0`.
    pub(super) fn advance_lfos_from_lanes(&mut self, current_tick: usize) {
        let abs_tick = self.current_abs_tick;
        let song = self.current_song_idx;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };
        // ---- Vibrato (TrackPitch) ---------------------------------------
        let vib_state = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.lfo_state_at(abs_tick));
        if let Some(state) = vib_state {
            if state.armed {
                let row_has_main =
                    self.lane_has_lfo_set_at(AutomationTarget::TrackPitch(track_idx), abs_tick);
                let row_has_depth_only = self
                    .lane_has_lfo_depthonly_at(AutomationTarget::TrackPitch(track_idx), abs_tick);
                if current_tick == 0 {
                    if self.module.quirks.pitch_vibrato_ticks_continuously {
                        // DW: vibrato advances on every frame of the
                        // held note, including the row-start frame
                        // (1 frame = 1 tick). Without the tick-0 step
                        // the phase would lose one of every `speed`
                        // frames. Independent of `row_has_main`: once
                        // armed the LFO keeps running until stopped.
                        self.effect_vibrato.tick_q();
                    } else if row_has_main
                        && self.module.quirks.it_vibrato_ticks_at_row_zero
                        && !self.module.quirks.it_old_effects
                    {
                        self.effect_vibrato.tick_q();
                    }
                } else if row_has_main
                    || (row_has_depth_only && self.module.quirks.volcol_b_advances_vibrato)
                    || self.module.quirks.pitch_vibrato_ticks_continuously
                {
                    self.effect_vibrato.tick_q();
                }
            }
        }
        // ---- Tremolo (TrackVolume) --------------------------------------
        if current_tick > 0 {
            let trem_state = self
                .module
                .lanes_for(AutomationTarget::TrackVolume(track_idx))
                .find_map(|l| l.lfo_state_at(abs_tick));
            if let Some(state) = trem_state {
                if state.armed
                    && self.lane_has_lfo_set_at(AutomationTarget::TrackVolume(track_idx), abs_tick)
                {
                    self.effect_tremolo.tick_q();
                }
            }
            // ---- Panbrello (TrackPanning) ------------------------------
            let pan_state = self
                .module
                .lanes_for(AutomationTarget::TrackPanning(track_idx))
                .find_map(|l| l.lfo_state_at(abs_tick));
            if let Some(state) = pan_state {
                if state.armed
                    && self.lane_has_lfo_set_at(AutomationTarget::TrackPanning(track_idx), abs_tick)
                {
                    self.effect_panbrello.tick_q();
                }
            }
        }
    }

    pub(super) fn apply_slides_from_lanes(&mut self, current_tick: usize) {
        let abs_tick = self.current_abs_tick;
        let song = self.current_song_idx;
        let Some(track_idx) = self.lookup_track_idx(abs_tick, song) else {
            return;
        };
        let is_tick0 = current_tick == 0;

        // VolumeSlide on TrackVolume Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackVolume(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.volume = self.volume.with_tremolo(state.rate);
            }
        }
        // ChannelVolumeSlide on TrackChannelVolume Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackChannelVolume(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.channel_volume = self.channel_volume.shifted_by(state.rate);
            }
        }
        // PanningSlide on TrackPanning Slide lane.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPanning(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            if state.armed && state.fine == is_tick0 {
                self.panning = self.panning.shifted_by(state.rate);
            }
        }
        // TonePortamento on TrackPitch Glide lane. At tick 0,
        // recompute the slide goal from `self.note` (which
        // `tick0_load_pitch` already updated to the cell's
        // played pitch via `played_pitch_for` + finetune
        // composition — drum-kit remaps included). At tick > 0,
        // approach the goal by `state.rate`.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.glide_state_at(abs_tick))
        {
            if state.armed {
                if is_tick0 {
                    self.effect_tone_portamento_goal = self.period_helper.note_to_period(self.note);
                } else if self.period != self.effect_tone_portamento_goal {
                    self.period = self
                        .period
                        .slide_towards(self.effect_tone_portamento_goal, state.rate);
                }
            }
        }

        // Portamento on TrackPitch Slide lane. Format-specific
        // clamping (FT2 signed-overflow, ST3 `period_clamp`, IT
        // `allow_zero_period`) is preserved verbatim from the
        // pre-substitution cell-side arm.
        if let Some(state) = self
            .module
            .lanes_for(AutomationTarget::TrackPitch(track_idx))
            .find_map(|l| l.slide_state_at(abs_tick))
        {
            // A coarse slide (`fine == false`) normally runs on the
            // `speed-1` non-row ticks. `pitch_slide_ticks_at_row_zero`
            // (DW) extends it to tick 0 too, so the ramp advances on
            // every frame of the held note — see the quirk doc.
            let runs_this_tick = state.fine == is_tick0
                || (is_tick0
                    && !state.fine
                    && self.module.quirks.pitch_slide_ticks_at_row_zero);
            if state.armed && runs_this_tick {
                let speed_i16 = state.rate.raw();
                let new_period = self.period.saturating_add_signed(speed_i16);
                if self.module.quirks.ft2_pitch_slide_overflow && speed_i16 > 0 {
                    // FT2 `pitchSlideDown` signed-overflow quirk
                    // (`ft2_replayer.c:1914`): periods in
                    // `[32000, 32767]` snap to 31999; periods
                    // ≥ 32768 wrap negative under the int16
                    // cast, the clamp silently fails, and the
                    // slide freewheels. Some XM modules rely
                    // on this.
                    let wrapped = new_period.raw() as i16;
                    if (32000..=32767).contains(&(wrapped as i32)) {
                        self.period = Period::from_raw(31999);
                    } else {
                        self.period = if new_period == Period::ZERO {
                            Period::from_raw(1)
                        } else {
                            new_period
                        };
                    }
                } else if let Some((clamp_min, clamp_max)) = self.module.quirks.period_clamp {
                    // ST3 `amigalimits` clamp.
                    self.period = new_period.clamp(clamp_min, clamp_max);
                } else {
                    // Conservative clamp; IT `allow_zero_period`
                    // opens the low edge so a pitch slide can
                    // reach "infinitely high note" — ST3
                    // convention without amigalimits.
                    let min_period = if self.module.quirks.allow_zero_period {
                        Period::ZERO
                    } else {
                        Period::from_raw(1)
                    };
                    self.period = new_period.clamp(min_period, Period::from_raw(31999));
                }
            }
        }
    }
}
