//! Stereo sample production for one channel.

use xmrs::core::fixed::units::Amp;

use crate::voice_pool::VoicePool;

use super::Channel;

impl<'a> Channel<'a> {
    /// Produce one stereo sample for the channel: live voice + every
    /// still-singing ghost. Returns `None` only when both are silent
    /// — a channel whose live note has cut but whose NNA=Continue
    /// ghosts are still ringing keeps producing audio.
    pub(crate) fn next_sample(&mut self, pool: &mut VoicePool<'a>) -> Option<(Amp, Amp)> {
        self.next_sample_with_inject(pool, (0, 0))
    }

    /// As [`Self::next_sample`], but first mixes a **pre-insert**
    /// stereo contribution (`inject`, Q1.15 `i32`) into the channel's
    /// accumulator — RFC §3C audio clips routed to this mixer channel,
    /// so a recorded-audio clip flows through the *same* per-channel
    /// insert chain (and, in `voices.mix`, the same sends) as the
    /// channel's notes. A non-zero `inject` keeps the channel audible
    /// even when no voice is ringing. `inject == (0, 0)` (every tracker
    /// import, and every channel with no audio clip) reproduces
    /// [`Self::next_sample`] exactly ⇒ bit-identical.
    pub(crate) fn next_sample_with_inject(
        &mut self,
        pool: &mut VoicePool<'a>,
        inject: (i32, i32),
    ) -> Option<(Amp, Amp)> {
        // Q-format integer accumulation: each contributing voice
        // is `(Amp, Amp)` Q1.15 (raw `i16`). Promote to `i32`
        // for the sum so 64+ stacked voices can't overflow the
        // accumulator before we narrow back to `Amp` at the
        // bottom. Saturating clamp on each accumulation step is
        // belt-and-braces (rare to need it in practice — a
        // typical 32-channel mix sums to ≤ ±16 of `Amp::FULL`).
        let mut left: i32 = inject.0;
        let mut right: i32 = inject.1;
        // A pre-insert audio injection makes the channel audible on its
        // own — it must run the insert chain and reach the mix even when
        // no voice is ringing.
        let mut any = inject.0 != 0 || inject.1 != 0;

        // Live voice.
        if let Some(i) = self.live_mut(pool) {
            if let Some((l, r)) = i.next() {
                // Pure Q1.15 path. Apply `actual_volume[ch]`
                // (Q1.15) to the raw audio (Q1.15) → Q1.15
                // saturating, then accumulate in i32.
                self.actual_volume[0].apply(l).accumulate_into(&mut left);
                self.actual_volume[1].apply(r).accumulate_into(&mut right);
                any = true;
            }
        }

        // Ghost voices. Each ghost owns its frozen gain / pan,
        // so we just accumulate.
        for id in &self.ghosts {
            if let Some(g) = pool.get_mut(*id) {
                if let Some((l, r)) = g.next() {
                    l.accumulate_into(&mut left);
                    r.accumulate_into(&mut right);
                    any = true;
                }
            }
        }

        if any {
            // S91 surround: invert the right channel's phase.
            // Negate at the i32 layer so we don't clip a
            // silent-into-silent fold. The accumulator can't
            // reach `i32::MIN` (each voice contributes
            // ≤ ±0x7FFF, hundreds of voices ≪ 2^31).
            if self.surround {
                right = -right;
            }
            // Narrow back to Q1.15 with saturation. The
            // per-channel accumulator may exceed one Q1.15
            // unit when several voices stack at full volume —
            // that's expected; the final mix-bus saturation
            // happens in `voices.mix`.
            let out = (Amp::from_q15_i32_sat(left), Amp::from_q15_i32_sat(right));

            // RFC §3B: run this mixer channel's stereo output through
            // its runtime insert chain (params driven by `DeviceParam`
            // automation in `update_device_params`). Every tracker
            // import leaves the chain empty (`is_active()` false), so
            // this is a measured no-op and the render is bit-identical
            // to the pre-3B mixer.
            let out = if self.insert_chain.is_active() {
                self.insert_chain.process(out)
            } else {
                out
            };
            Some(out)
        } else {
            None
        }
    }
}
