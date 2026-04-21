use crate::channel::Channel;
use crate::triggerkeep::*;
use alloc::{vec, vec::Vec};
use xmrs::prelude::*;

pub struct XmrsPlayer<'a> {
    pub module: &'a Module,
    pub current_song: usize,
    sample_rate: f32,

    tempo: usize,
    bpm: usize,
    /// Global volume: 0.0 to 1.0
    pub global_volume: f32,
    effect_volume_slide_speed: f32,
    effect_volume_slide_fine: bool,
    /// Global amplification (default 1.0)
    pub amplification: f32,
    current_table_index: usize,
    current_row: usize,
    current_tick: usize,
    /// sample rate / (BPM * 0.4)
    remaining_samples_in_tick: f32,
    /// +1 for a (left,right) sample
    pub generated_samples: u64,

    position_jump: bool,
    pattern_break: bool,
    jump_dest: usize,
    jump_row: usize,

    /// Extra ticks to be played before going to the next row - Used for EEy effect
    extra_ticks: usize,

    pub channel: Vec<Channel<'a>>,

    /// Last tempo value propagated to per-channel state
    /// (`EffectArpeggio`). Tracked so the per-step propagation loop is
    /// skipped when tempo has not actually changed — which is the
    /// common case, since Fxx landing on a row is rare.
    last_propagated_tempo: usize,

    row_loop_count: Vec<Vec<usize>>,
    loop_count: usize,
    max_loop_count: usize,

    /// None if next-one is a left sample, else right sample
    right_sample: Option<f32>,
    #[cfg(feature = "std")]
    debug: bool,

    pause: bool,
}

impl<'a> XmrsPlayer<'a> {
    /// Build a player for the given module.
    ///
    /// Format-specific FT2/XM behaviour — arpeggio LUT, E60 pattern-
    /// loop bug, arpeggio period clamp, portamento-down signed-
    /// overflow, K00-eats-note, vol-col `Bx` advancing the vibrato —
    /// is driven entirely by `module.format`. `ModuleFormat::Xm`
    /// activates the canonical FT2 reference behaviour, all other
    /// formats use their own native semantics. There is no longer a
    /// separate `ft2_quirks` flag: the format IS the switch.
    ///
    /// If you want FT2-style playback on a module carrying a non-Xm
    /// format tag (unusual — typically a hand-authored module against
    /// an FT2-style player), just set `module.format = ModuleFormat::Xm`
    /// before constructing the player.
    pub fn new(module: &'a Module, sample_rate: f32, song: usize) -> Self {
        let num_channels = module.get_num_channels();

        let current_song = if song < module.pattern_order.len() {
            song
        } else {
            0
        };

        let mut player = Self {
            module,
            current_song,
            sample_rate,
            tempo: module.default_tempo,
            bpm: module.default_bpm,
            global_volume: 1.0,
            amplification: 1.0,
            row_loop_count: vec![vec![0; MAX_NUM_ROWS]; module.get_song_length(current_song)],
            effect_volume_slide_speed: 0.0,
            effect_volume_slide_fine: true,
            current_table_index: 0,
            current_row: 0,
            current_tick: 0,
            remaining_samples_in_tick: 0.0,
            generated_samples: 0,
            position_jump: false,
            pattern_break: false,
            jump_dest: 0,
            jump_row: 0,
            extra_ticks: 0,
            channel: vec![],
            last_propagated_tempo: module.default_tempo,
            loop_count: 0,
            max_loop_count: 0,
            right_sample: None,
            #[cfg(feature = "std")]
            debug: false,
            pause: false,
        };

        player.channel =
            vec![Channel::new(module, sample_rate, module.default_tempo); num_channels];

        player
    }

    #[cfg(feature = "std")]
    pub fn debug(&mut self, debug: bool) {
        self.debug = debug;
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

    pub fn set_max_loop_count(&mut self, max_loop_count: usize) {
        self.max_loop_count = max_loop_count;
    }

    pub fn get_loop_count(&self) -> usize {
        self.loop_count
    }

    pub fn get_sample_rate(&self) -> f32 {
        self.sample_rate
    }

    pub fn get_tempo(&self) -> usize {
        self.tempo
    }

    pub fn get_bpm(&self) -> usize {
        self.bpm
    }

    /// Jump to row at index table_position in pattern_order at speed
    /// if speed == 0, resets to default speed
    pub fn goto(&mut self, table_position: usize, row: usize, speed: usize) -> bool {
        if table_position < self.module.get_song_length(self.current_song) {
            let num_row = self.module.pattern_order[self.current_song][table_position];
            if row < self.module.get_num_rows(num_row) {
                // Create a position jump
                self.jump_dest = table_position;
                self.jump_row = row;
                self.position_jump = true;

                // Cleanup self
                self.tempo = if speed == 0 {
                    self.module.default_tempo
                } else {
                    speed
                };
                self.bpm = self.module.default_bpm;
                self.global_volume = 1.0;

                // Cleanup channels
                let num_channels = self.module.get_num_channels();
                for i in 0..num_channels {
                    self.channel[i].trigger_pitch(TRIGGER_KEEP_PERIOD); // clean what we can
                }

                // next() must call tick() then row()
                self.remaining_samples_in_tick = 0.0;
                self.current_tick = 0;

                true
            } else {
                false
            }
        } else {
            false
        }
    }

    /// Returns current pattern number in pattern_order
    pub fn get_current_pattern(&self) -> usize {
        self.module.pattern_order[self.current_song][self.current_table_index]
    }

    /// Returns current index in pattern_order
    pub fn get_current_table_index(&self) -> usize {
        self.current_table_index
    }

    /// Returns current row
    pub fn get_current_row(&self) -> usize {
        self.current_row
    }

    /// Force pause, returning (0.0, 0.0) samples
    pub fn pause(&mut self, pause: bool) {
        self.pause = pause;
    }

    fn post_pattern_change(&mut self) {
        /* Loop if necessary */
        if self.current_table_index >= self.module.pattern_order[self.current_song].len() {
            self.current_table_index = self.module.restart_position;
        }

        #[cfg(feature = "std")]
        if self.debug {
            println!(
                "pattern_order[0x{:03x}] = 0x{:03x}",
                self.current_table_index,
                self.module.pattern_order[self.current_song][self.current_table_index]
            );
        }
    }

    fn tick0_global_effects(&mut self, ch_index: usize) {
        let ch = &mut self.channel[ch_index];
        let pattern_slot = &ch.current;

        for gfx in pattern_slot.global_effects.clone() {
            match gfx {
                GlobalEffect::Bpm(bpm) => {
                    self.bpm = bpm;
                }
                GlobalEffect::BpmSlide(_value) => {
                    #[cfg(feature = "demo")]
                    println!("BpmSlide");
                    todo!();
                }
                GlobalEffect::MidiMacro(_m) => {
                    #[cfg(feature = "demo")]
                    println!("MidiMacro");
                    todo!();
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
                    if value != 0 {
                        if value == ch.pattern_loop_count {
                            // Loop is over
                            ch.pattern_loop_count = 0;
                            if self.module.quirks.pattern_loop_resumes {
                                // ST3 `s_patloop` (digcmd.c:1003):
                                // when the loop finishes, the next
                                // row becomes the new loop start. If
                                // a second SBx appears later in the
                                // same pattern without a fresh SB0,
                                // it will naturally loop from just
                                // after the previous loop's end
                                // instead of re-using the original
                                // origin. FT2 doesn't do this; S3M
                                // modules do rely on it.
                                ch.pattern_loop_origin = self.current_row + 1;
                            }
                        } else {
                            // Jump to the beginning of the loop
                            ch.pattern_loop_count += 1;
                            self.position_jump = true;
                            self.jump_row = ch.pattern_loop_origin;
                            self.jump_dest = self.current_table_index;
                        }
                    } else {
                        // Set loop start point
                        ch.pattern_loop_origin = self.current_row;
                        if self.module.quirks.e60_leaks_to_next_pattern {
                            // Replicate FT2 E60 bug: in FT2, E60 not
                            // only sets the per-channel loop origin,
                            // it ALSO leaks that row number into the
                            // song's pBreakPos. When the pattern then
                            // ends naturally, the next pattern starts
                            // at pBreakPos instead of row 0. Some XM
                            // modules rely on this; canonical XM
                            // playback needs to reproduce it.
                            self.jump_row = ch.pattern_loop_origin;
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
                GlobalEffect::Volume(volume) => {
                    self.global_volume = volume.clamp(0.0, 1.0);
                }
                GlobalEffect::VolumeSlide { speed: s, fine: f } => {
                    if f {
                        // Fine slide: apply once at tick 0.
                        self.global_volume = (self.global_volume + s).clamp(0.0, 1.0);
                    }
                    // Save speed for non-fine application during subsequent ticks.
                    self.effect_volume_slide_speed = s;
                    self.effect_volume_slide_fine = f;
                }
            }
        }
    }

    fn tick0(&mut self) {
        if self.position_jump {
            self.current_table_index = self.jump_dest;
            self.current_row = self.jump_row;
            self.position_jump = false;
            self.pattern_break = false;
            self.jump_row = 0;
            self.post_pattern_change();
        } else if self.pattern_break {
            self.current_table_index += 1;
            self.current_row = self.jump_row;
            self.pattern_break = false;
            self.jump_row = 0;
            self.post_pattern_change();
        }

        let pat_idx_temp = self.module.pattern_order[self.current_song][self.current_table_index];
        let pat_idx = if pat_idx_temp < self.module.pattern.len() {
            pat_idx_temp
        } else {
            // empty pattern, returning to zero
            self.current_table_index = 0;
            self.module.pattern_order[self.current_song][self.current_table_index]
        };

        let num_channels = self.module.get_num_channels();
        let mut in_a_loop = false;

        let current_row = self.current_row;
        #[cfg(feature = "std")]
        if self.debug {
            print!("{:03X} ", current_row);
        }
        for ch_index in 0..num_channels {
            let ps = &self.module.pattern[pat_idx][current_row][ch_index];
            #[cfg(feature = "std")]
            if self.debug {
                print!("{:?} ", ps.note);
            }
            self.channel[ch_index].tick0(ps);
            self.tick0_global_effects(ch_index);
            if !in_a_loop && self.channel[ch_index].pattern_loop_count > 0 {
                in_a_loop = true;
            }
        }
        #[cfg(feature = "std")]
        if self.debug {
            println!();
        }

        if !in_a_loop {
            /* No E6y loop is in effect (or we are in the first pass) */
            self.loop_count = self.row_loop_count[self.current_table_index][self.current_row];
            self.row_loop_count[self.current_table_index][self.current_row] += 1;
        }

        // `current_row` is a `usize`, so the wrap-to-0 branch below cannot
        // fire in practice (pattern lengths are bounded well below usize::MAX).
        // A previous comment speculated about shrinking this field to `u8`
        // and wanted the wrap to push us into the next pattern; if that ever
        // happens, reintroduce the `== 0` guard in the condition below.
        self.current_row += 1;
        let pattern_len = self.module.pattern[pat_idx].len();

        if !self.position_jump && !self.pattern_break && self.current_row >= pattern_len {
            self.current_table_index += 1;
            self.current_row = self.jump_row; /* This will be 0 most of
                                               * the time, except when E60
                                               * is used */
            self.jump_row = 0;
            self.post_pattern_change();
        }
    }

    fn tick(&mut self) {
        // Detect whether any channel carries a global volume slide this row.
        // The global slide must be applied at most once per tick, not once per channel.
        let any_global_slide = self
            .channel
            .iter()
            .any(|ch| ch.current.has_global_volume_slide());

        for ch in &mut self.channel {
            ch.tick(self.current_tick);
        }

        // tick() is only called for current_tick != 0, so no extra guard needed here.
        // Fine slides were already applied in tick0_global_effects.
        if any_global_slide && !self.effect_volume_slide_fine {
            self.global_volume =
                (self.global_volume + self.effect_volume_slide_speed).clamp(0.0, 1.0);
        }
    }

    fn step(&mut self) {
        if self.remaining_samples_in_tick <= 0.0 {
            if self.current_tick == 0 {
                self.tick0();
            } else {
                self.tick();
            }

            self.current_tick += 1;
            // `F00` (speed = 0) halts row advance: tick() keeps firing so
            // sustained effects continue, but tick0() never runs again
            // until the tempo is set to a non-zero value.
            if self.tempo != 0 && self.current_tick >= self.tempo + self.extra_ticks {
                self.current_tick = 0;
                self.extra_ticks = 0;
            }

            // Propagate the current song speed to each channel so the
            // arpeggio FT2 LUT indexes from the live tempo, not the
            // default copy stashed at construction. Fxx (speed
            // change) can land anywhere in a pattern, but most
            // tick-boundaries see no change — gate the N-channel
            // loop on an actual tempo delta to keep the common case
            // at a single compare.
            if self.tempo != self.last_propagated_tempo {
                for ch in &mut self.channel {
                    ch.set_tempo(self.tempo);
                }
                self.last_propagated_tempo = self.tempo;
            }
            /* FT2 manual says number of ticks / second = BPM * 0.4 */
            self.remaining_samples_in_tick += self.sample_rate / (self.bpm as f32 * 0.4);
        }
        self.remaining_samples_in_tick -= 1.0;
    }

    /// Returns samples from each channel before applying global volume and amplification.
    /// If the function returns None, no more samples are available.
    ///
    /// In conjunction with the samples_apply_volume() function, this function can be used to replace the iterator or the sample() function if you want to control each channel in fine detail, for example, to create beautiful graphic effects.
    pub fn samples_from_channels(&mut self) -> Option<Vec<(f32, f32)>> {
        if self.pause {
            return Some(vec![(0.0, 0.0); self.channel.len()]);
        }

        self.step();

        if self.max_loop_count > 0 && self.loop_count >= self.max_loop_count {
            return None;
        }

        let samples: Vec<(f32, f32)> = self
            .channel
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
            .collect();

        self.generated_samples += 1;
        Some(samples)
    }

    pub fn samples_to_sample(&mut self, samples: &[(f32, f32)]) -> (f32, f32) {
        let sample = samples
            .iter()
            .fold((0.0, 0.0), |(acc_left, acc_right), (left, right)| {
                (acc_left + left, acc_right + right)
            });
        (sample.0, sample.1)
    }

    /// This function applies volume and amplification to the various channel samples. It is applied to the result of the `samples_from_channels()` function.
    pub fn samples_apply_volume(&mut self, samples: &[(f32, f32)]) -> (f32, f32) {
        let fgvol = self.global_volume * self.amplification;
        let sample = self.samples_to_sample(samples);
        (sample.0 * fgvol, sample.1 * fgvol)
    }

    /// Returns the sum of the samples from all channels, optionally
    /// scaled by `global_volume * amplification`. This is the hot
    /// path used by the `Iterator` impl: unlike
    /// [`Self::samples_from_channels`] + [`Self::samples_apply_volume`],
    /// it walks channels once with an `(f32, f32)` accumulator and
    /// never allocates a per-sample `Vec`. At 48 kHz stereo that saves
    /// ~96 k allocations/second.
    ///
    /// Semantically identical to the older
    /// `samples_from_channels().map(samples_apply_volume_or_sum)`
    /// composition — the three explicit stages are still available
    /// for callers that need per-channel inspection.
    pub fn sample(&mut self, apply_volume: bool) -> Option<(f32, f32)> {
        if self.pause {
            return Some((0.0, 0.0));
        }

        self.step();

        if self.max_loop_count > 0 && self.loop_count >= self.max_loop_count {
            return None;
        }

        let (mut left, mut right) = (0.0f32, 0.0f32);
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

        self.generated_samples += 1;

        if apply_volume {
            let g = self.global_volume * self.amplification;
            Some((left * g, right * g))
        } else {
            Some((left, right))
        }
    }

    /// Returns samples one after the other, starting with the left channel.
    fn sample_one(&mut self) -> Option<f32> {
        match self.right_sample {
            Some(right) => {
                self.right_sample = None;
                Some(right)
            }
            None => {
                let next_samples = self.sample(true);
                match next_samples {
                    Some((left, right)) => {
                        self.right_sample = Some(right);

                        Some(left)
                    }
                    None => None,
                }
            }
        }
    }
}

impl<'a> Iterator for XmrsPlayer<'a> {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        if self.max_loop_count > 0 && self.loop_count >= self.max_loop_count {
            None
        } else {
            self.sample_one()
        }
    }
}
