/// The purpose of this module is to alleviate imports of `xmrsplayer` parts.
///
/// ```
/// #![allow(unused_imports)]
/// use xmrsplayer::prelude::*;
/// ```
///
/// Exports both the top-level `XmrsPlayer` and the subscription API —
/// `PlayerObserver` (song-side events) and `MixObserver` /
/// `ChannelsObserver` (audio-side events). The built-in `DebugObserver`
/// is re-exported too when `std` is available.
pub use crate::audio_observer::{ChannelsContext, ChannelsObserver, MixContext, MixObserver};
pub use crate::midi_observer::{MidiEvent, MidiObserver};
pub use crate::observer::{PatternChangeContext, PlayerObserver, RowContext, TickContext};
pub use crate::xmrsplayer::XmrsPlayer;

#[cfg(feature = "std")]
pub use crate::debug_midi_observer::DebugMidiObserver;
#[cfg(feature = "std")]
pub use crate::debug_observer::DebugObserver;
