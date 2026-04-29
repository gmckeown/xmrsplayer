//! Audio-side subscription API.
//!
//! While [`crate::observer::PlayerObserver`] is notified on *song* events
//! (rows, ticks, pattern changes), the audio observers in this module are
//! notified on every **produced sample** — once per stereo frame of output.
//! They are the natural hook for VU-meters, oscilloscopes, FFT analyzers,
//! recording probes, any code that wants to consume (not modify) the audio
//! the player generates.
//!
//! Two complementary traits are exposed:
//!
//! * [`MixObserver`] — receives the final stereo mix, pre-gain, along with
//!   the `global_volume` and `amplification` values of the moment so the
//!   observer can compute post-gain itself if it wants.
//! * [`ChannelsObserver`] — receives a per-channel slice *before* summing,
//!   so an observer can inspect each channel independently (multi-track
//!   visualisers, per-channel meters, etc.).
//!
//! ## Cost model
//!
//! `MixObserver` is cheap: one vtable dispatch per sample per registered
//! observer. `ChannelsObserver` is more expensive — enabling it forces the
//! mixer to capture each channel's pre-sum value into a buffer, which
//! costs even when no observer is registered; for that reason the player
//! gates the capture on the channels-observer list being non-empty.
//!
//! ## Contract
//!
//! Both traits require `Send`, for the same reason [`PlayerObserver`]
//! does: the typical deployment wraps the player in `Arc<Mutex<_>>` and
//! shares it with an audio thread.
//!
//! Callbacks run **synchronously** on the thread that pulls the sample —
//! at audio rate (48 kHz) this means tight budget. Do not allocate, do
//! not block, do not do I/O. Push to a queue and drain elsewhere.
//!
//! During `player.pause(true)`, callbacks still fire but with `(0.0, 0.0)`
//! samples — recorders capturing silence, oscilloscopes drawing flatlines.
//! An observer that wants to distinguish pause from real silence can
//! register a parallel [`PlayerObserver`] and watch for... actually the
//! simpler path is: just test `left == 0.0 && right == 0.0` is ambiguous
//! (could be a genuine silent passage). If you need the distinction, keep
//! a `paused: bool` externally and update it when *you* call
//! `player.pause()`.
//!
//! [`PlayerObserver`]: crate::observer::PlayerObserver
//! [`MixObserver`]: crate::audio_observer::MixObserver
//! [`ChannelsObserver`]: crate::audio_observer::ChannelsObserver

/// Context passed to [`MixObserver::on_mix`].
///
/// `left` and `right` are the **pre-gain** summed mix — what comes out of
/// the channel mixer before `global_volume * amplification` is applied.
/// An observer wanting the post-gain version computes
/// `left * global_volume * amplification` itself; one that wants the pre-
/// gain signal (e.g. a loudness analyzer that shouldn't be influenced by
/// the user's volume slider) uses `left` and `right` directly.
pub struct MixContext {
    pub left: f32,
    pub right: f32,
    /// Current `global_volume` (song-driven, 0.0..=1.0). Normally moves
    /// only when a `GlobalEffect::Volume` / `VolumeSlide` fires.
    pub global_volume: f32,
    /// Current `amplification` (user-driven, unbounded). Changes only
    /// when `XmrsPlayer::set_amplification` is called.
    pub amplification: f32,
}

/// Context passed to [`ChannelsObserver::on_channels`].
///
/// `channels` is a borrowed slice, one `(left, right)` pair per song
/// channel, captured *during* the mix loop — so each entry already has
/// the channel's `actual_volume` / mute state applied, but **not** the
/// global gain. Sum of all pairs equals the pre-gain stereo mix reported
/// to [`MixObserver`].
///
/// The slice is valid only for the duration of the call; do not retain
/// references to it. If you need to keep the data, copy what you need
/// (each entry is a `(f32, f32)` — trivial to clone).
pub struct ChannelsContext<'a> {
    pub channels: &'a [(f32, f32)],
    pub global_volume: f32,
    pub amplification: f32,
}

/// Subscriber to the final (summed, stereo) audio mix, pre-gain.
///
/// See the module docs for the cost model and the threading contract.
pub trait MixObserver: Send {
    fn on_mix(&mut self, ctx: &MixContext);
}

/// Subscriber to the per-channel audio slice, pre-sum, pre-gain.
///
/// Registering at least one `ChannelsObserver` enables per-channel
/// capture in the mixer, which has a small but non-zero cost even when
/// only one observer consumes the data. If you only need the summed
/// mix, prefer [`MixObserver`].
pub trait ChannelsObserver: Send {
    fn on_channels(&mut self, ctx: &ChannelsContext<'_>);
}
