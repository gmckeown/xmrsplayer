//! RFC §2A — the per-tick **voice modulation bus**.
//!
//! [`VoiceMod`] is the explicit snapshot of a channel's per-tick
//! modulation (everything the lanes / effects contribute), decoupled
//! from *which voice reads it*. The channel fills it once per tick
//! ([`crate::channel::Channel::voice_mod`]); a voice consumes it
//! ([`crate::state_instr_default::StateInstrDefault::apply_voice_mod`]),
//! combining it with the voice's own envelope state to produce the
//! final per-sample step and stereo gains.
//!
//! Today there is exactly **one** consumer per channel — the live
//! voice, routed exactly as the pre-2A player did, so the render is
//! bit-identical. The point of making the bus explicit is the seam it
//! opens: RFC §2B re-points `apply_voice_mod` *per voice* (live + NNA
//! ghosts, MPE), each reading its own `VoiceMod`, without touching the
//! arithmetic this phase froze.

use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::{ChannelVolume, Panning, Period, PitchDelta, Volume};

/// One channel's modulation contribution for a single tick. All
/// fields are *channel-side* values (from the lanes / row effects);
/// the voice-internal envelope (volume / panning / pitch) is **not**
/// here — it is read straight from the voice at apply time.
#[derive(Clone, Copy, Debug)]
pub(crate) struct VoiceMod {
    /// Base channel period (un-modulated note period).
    pub period: Period,
    /// Channel volume after tremor / random-volume-variation.
    pub volume: Volume,
    /// Channel pan position.
    pub panning: Panning,
    /// Channel-volume scale (IT M-column / DW per-instrument).
    pub channel_volume: ChannelVolume,
    /// Arpeggio semitone offset — `ZERO` when no arpeggio this tick.
    pub arp_pitch: PitchDelta,
    /// Summed vibrato-LFO output (raw period/semitone delta, §1B).
    pub vibrato_raw: i16,
    /// Held transpose from a `TrackPitch` `Points` lane.
    pub points_pitch: PitchDelta,
    /// XM glissando: round base pitch to the nearest semitone.
    pub semitone: bool,
    /// Summed tremolo-LFO output (Q1.15 volume delta, §1B).
    pub tremolo: Q15,
    /// Summed panbrello-LFO output (Q1.15 pan delta, §1B).
    pub panbrello: Q15,
    /// S3M/FT2 tremor: force the voice volume to silent this tick.
    pub tremor: bool,
}
