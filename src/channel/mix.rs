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
        // Q-format integer accumulation: each contributing voice
        // is `(Amp, Amp)` Q1.15 (raw `i16`). Promote to `i32`
        // for the sum so 64+ stacked voices can't overflow the
        // accumulator before we narrow back to `Amp` at the
        // bottom. Saturating clamp on each accumulation step is
        // belt-and-braces (rare to need it in practice — a
        // typical 32-channel mix sums to ≤ ±16 of `Amp::FULL`).
        let mut left: i32 = 0;
        let mut right: i32 = 0;
        let mut any = false;

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
            Some((Amp::from_q15_i32_sat(left), Amp::from_q15_i32_sat(right)))
        } else {
            None
        }
    }
}
