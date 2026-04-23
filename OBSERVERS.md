# Subscribing to player events

Starting with 0.10, `xmrsplayer` exposes a subscription API: anything that
wants to know "which row is playing right now, on which pattern, with which
notes and effects" can register an observer with the player and receive
callbacks as the song progresses. This document walks through the contract,
the threading rules, and a couple of recipes.

## The big picture

Under the hood the player is now three pieces:

1. **`Sequencer`** — knows where we are in the song (pattern order, row,
   tempo clock) and interprets navigation effects (`Fxx`, `Bxx`, `Dxx`,
   `E6y`, `EEy`…).
2. **`Voices`** — the audio engine (per-channel state + mixer). It is the
   *default row consumer*: every row the sequencer produces is forwarded to
   voices so the module actually plays.
3. **Your observers** — anything else that wants to be notified. Voices is
   called directly by the facade; observers are called right after, via
   trait-object dispatch.

From an observer's point of view, `Voices` is invisible. You just implement
a trait, register an instance, and receive events.

## The trait

```rust
pub trait PlayerObserver {
    fn on_row(&mut self, ctx: &RowContext<'_>);

    fn on_tick(&mut self, _ctx: &TickContext<'_>) {}
    fn subscribes_to_ticks(&self) -> bool { false }

    fn on_pattern_change(&mut self, _ctx: &PatternChangeContext<'_>) {}
    fn on_song_end(&mut self) {}
}
```

`on_row` is the only mandatory method. The others have no-op defaults, so a
"just tell me when the row changes" observer is three lines of code.

### The contexts

```rust
pub struct RowContext<'a> {
    pub module: &'a Module,
    pub song: usize,
    pub pattern: usize,      // index into module.pattern
    pub row: usize,
    pub cells: &'a [TrackUnit],   // one per channel, zero-copy borrow
    pub tempo: usize,
    pub bpm: usize,
}
```

`cells` is borrowed directly from the module — no allocation, no clone — so
you can pick out the note/instrument/volume/effects of any channel at zero
cost:

```rust
for (ch, cell) in ctx.cells.iter().enumerate() {
    if cell.note.is_valid() {
        let instr = cell.instrument;
        println!("ch{ch}: note={:?} instr={:?}", cell.note, instr);
    }
}
```

`TickContext` and `PatternChangeContext` follow the same pattern; see
`src/observer/mod.rs` for the full definitions.

### Opting into ticks

`on_tick` fires on every sustained (non-row-start) tick — roughly 50 times
per second at the default BPM. That's wasteful for a UI that only redraws
on row changes, so the default `subscribes_to_ticks()` returns `false` and
the player skips the callback entirely for observers that don't override
it.

Override it to return `true` *only* when you actually need tick-level
granularity (a vibrato visualiser, a per-tick LED strobe, a recording probe
measuring something that changes between rows).

```rust
impl PlayerObserver for MyVibratoViewer {
    fn on_row(&mut self, _ctx: &RowContext<'_>) { /* ... */ }
    fn on_tick(&mut self, ctx: &TickContext<'_>) { /* ... */ }
    fn subscribes_to_ticks(&self) -> bool { true }
}
```

## Registering an observer

```rust
use xmrsplayer::prelude::*;

let mut player = XmrsPlayer::new(&module, 48_000.0, 0);
player.add_observer(Box::new(MyObserver::new()));
```

Observers are stored in a `Vec<Box<dyn PlayerObserver + Send>>` and invoked in
registration order. You can register as many as you want. To remove all of
them at once, call `player.clear_observers()`; to count them,
`player.observer_count()`.

There is no "remove a specific observer" method by design — if you need to
toggle a subscriber on and off, put the toggle inside the observer itself
(a `self.enabled: bool` flag read at the top of `on_row`).

### The `Send` bound

`PlayerObserver` has `Send` as a supertrait: `trait PlayerObserver: Send`.
This is because the typical deployment pattern wraps the player in
`Arc<Mutex<XmrsPlayer>>` and hands it to an audio callback thread (cpal,
rodio, …). For the `Mutex<XmrsPlayer>` to be `Sync` (and the `Arc` sendable),
every owned field — including the observer list — must be sendable.

Most observers satisfy this automatically (they hold plain data — counters,
pre-allocated `Vec`s, channel senders). If yours holds a `!Send` type —
a raw pointer, an `Rc<_>`, a `RefCell<_>` — either restructure to move the
non-sendable state behind an `Arc<Mutex<_>>`, or don't share the player
across threads at all (single-threaded usage removes the bound practically,
though the type still enforces it).

`Sync` is **not** required: observer methods are always called through
`&mut self` from a single thread at a time (the one holding the mutex).

## Threading contract

Every observer callback runs **synchronously, on the thread that pulled the
last sample from the player**. In a typical audio setup that's the audio
callback thread (cpal / rodio). That has three consequences:

1. **Do not block.** No mutex acquisitions you can't prove will succeed
   instantly, no file I/O, no network, no long CPU loops. The audio callback
   has a deadline (usually a few ms); if you miss it the stream xruns.

2. **Do not allocate on hot paths.** `on_tick` fires ~50 Hz; `on_row` a few
   Hz. The latter is fine for the occasional `format!()`; the former should
   stay allocation-free.

3. **Send data elsewhere to do real work.** The canonical pattern is a
   single-producer single-consumer queue:

   ```rust
   use std::sync::mpsc::{self, Sender};

   struct RowForwarder { tx: Sender<RowSnapshot> }

   impl PlayerObserver for RowForwarder {
       fn on_row(&mut self, ctx: &RowContext<'_>) {
           // Build a small owned snapshot (no borrowed references).
           let snap = RowSnapshot {
               pattern: ctx.pattern,
               row: ctx.row,
               notes: ctx.cells.iter().map(|c| c.note).collect(),
           };
           let _ = self.tx.send(snap);  // non-blocking send
       }
   }
   ```

   The UI thread drains the receiver and updates the display. The audio
   thread does only the minimum: build the snapshot and enqueue it.

   For real-time-sensitive setups, prefer a lock-free SPSC queue
   (`rtrb`, `crossbeam` `ArrayQueue`) over `std::sync::mpsc`.

## No back-references to the player

An observer receives `&RowContext`, not a handle to the player. This is
deliberate: if an observer needed to call `player.goto(...)` from inside
`on_row`, the player is already borrowed mutably up the stack (you're
inside its `sample()` call), and you'd deadlock the moment you tried to
lock it.

If you want to react to an event by jumping, push the command to a queue
and let the thread that owns the player drain it between samples. For
example, your main thread can do:

```rust
if let Some(cmd) = command_rx.try_recv().ok() {
    match cmd {
        Command::JumpTo(pos) => { player.lock().unwrap().goto(pos, 0, 0); }
        // ...
    }
}
```

## The `DebugObserver`

Gone are the days of `player.debug(true)`. Register the built-in observer
instead:

```rust
use xmrsplayer::prelude::*;

player.add_observer(Box::new(DebugObserver::new()));
```

It prints pattern-order changes and one row per notification. For a
firehose that also prints every tick, flip the flag:

```rust
player.add_observer(Box::new(DebugObserver::new().with_trace_ticks(true)));
```

Only available when the `std` feature is on, since it uses `println!`.

## A complete tiny example

```rust
use xmrs::prelude::*;
use xmrsplayer::prelude::*;

struct NoteHighlighter;

impl PlayerObserver for NoteHighlighter {
    fn on_row(&mut self, ctx: &RowContext<'_>) {
        for (ch, cell) in ctx.cells.iter().enumerate() {
            if cell.note.is_valid() {
                // Send `(ch, cell.note, cell.instrument)` to your UI somehow.
                // For the sake of the example we just print.
                println!("row {}: ch{ch} -> {:?}", ctx.row, cell.note);
            }
        }
    }
}

fn main() {
    let bytes = std::fs::read("song.xm").unwrap();
    let module = Module::load(&bytes).unwrap();
    let mut player = XmrsPlayer::new(&module, 48_000.0, 0);

    player.add_observer(Box::new(NoteHighlighter));

    for sample in player.by_ref().take(48_000 * 10) {
        // feed `sample` to your audio backend
        let _ = sample;
    }
}
```

## Migrating from 0.9.x

The public API is almost identical. The only breaking changes:

| 0.9.x                            | 0.10+                                                        |
| -------------------------------- | ------------------------------------------------------------ |
| `player.amplification = 0.25;`   | `player.set_amplification(0.25);`                            |
| `player.global_volume = 0.5;`    | `player.set_global_volume(0.5);`                             |
| `player.generated_samples`       | `player.generated_samples()`                                 |
| `player.debug(true);`            | `player.add_observer(Box::new(DebugObserver::new()));`       |
| `player.channel[n].muted = ...;` | `player.set_mute_channel(n, ...);` (already existed in 0.9)  |

`player.module` stays public — it's a `&Module` reference and there is no
invariant to guard.

## Testing story

The `Sequencer` is now independently driveable: you can feed it a
hand-built `Module` and check that `advance_tick()` emits the expected
`TickOutcome`s, without instantiating `Voices` and without producing any
audio. This makes the historically tricky navigation logic (FT2 `E60` leak,
ST3 pattern-loop resume, `Bxx` + `Dxx` interaction, `EEy` combined with
`Fxx`) unit-testable with pure data. A test module is a good place to
start growing the regression suite.

## Audio observers

So far we've covered `PlayerObserver`, which fires on *song* events (rows,
ticks, pattern changes). Starting in 0.10 there is a complementary
subscription path that fires on **produced samples** — once per stereo
frame of audio coming out of the mixer. The use cases are different:
VU-meters, oscilloscopes, FFT analyzers, recording probes, loudness
meters — anything that wants to consume (not modify) the audio.

Two traits cover the two levels of detail:

```rust
pub trait MixObserver: Send {
    fn on_mix(&mut self, ctx: &MixContext);
}

pub trait ChannelsObserver: Send {
    fn on_channels(&mut self, ctx: &ChannelsContext<'_>);
}
```

`MixObserver` receives the **final stereo mix**, pre-gain. Use it for
anything that operates on the summed signal.

`ChannelsObserver` receives the **per-channel pre-sum slice** — one
`(left, right)` pair per song channel, with the channel's panning / mute
already applied but before the sum and the global gain. Use it for
multi-track visualisers or per-channel meters.

### Contexts

```rust
pub struct MixContext {
    pub left: f32,
    pub right: f32,
    pub global_volume: f32,   // 0.0..=1.0, song-driven
    pub amplification: f32,   // user-driven, unbounded
}

pub struct ChannelsContext<'a> {
    pub channels: &'a [(f32, f32)],   // one per song channel
    pub global_volume: f32,
    pub amplification: f32,
}
```

Both contexts carry `global_volume` and `amplification` alongside the
samples so the observer is **self-contained**: no need to query the
player (which is borrowed mutably during the callback anyway) and no
need to cache the gain through side-channel notifications.

### Pre-gain by design

Both contexts expose samples *before* the `global_volume * amplification`
multiplication. An observer that wants the post-gain value (what the
listener actually hears) multiplies itself:

```rust
fn on_mix(&mut self, ctx: &MixContext) {
    let gain = ctx.global_volume * ctx.amplification;
    let post_l = ctx.left * gain;
    let post_r = ctx.right * gain;
    // ...
}
```

An observer wanting the raw mix (e.g. a loudness analyzer that must not
be influenced by the user's volume slider) uses `ctx.left` / `ctx.right`
as-is. This keeps the design symmetric, lets each observer pick its own
semantics, and makes the pipeline transparent — nobody has to wonder
what `post-gain` means exactly when new stages are inserted.

### Registration

```rust
use xmrsplayer::prelude::*;

player.add_audio_mix_observer(Box::new(MyVuMeter::default()));
player.add_audio_channels_observer(Box::new(MyMultiTrackScope::default()));

// Symmetric clear / count methods:
player.clear_audio_mix_observers();
player.clear_audio_channels_observers();
let n = player.audio_mix_observer_count();
let m = player.audio_channels_observer_count();
```

### Cost model

A `MixObserver` is cheap: one vtable dispatch per sample per registered
observer. At 48 kHz with N observers, that's `48000 × N` calls per
second — well under 1% CPU for small N.

A `ChannelsObserver` costs more in two ways. First, each sample now
fills a per-channel capture buffer in the mixer (one write per channel
per sample). Second, the observer's `on_channels` receives a slice of
that buffer to iterate. The player gates the capture on the channels-
observer list being non-empty, so the cost is paid only when someone
consumes it — but *any* channels observer enables the capture for *all*
samples. If you only need the summed mix, always prefer `MixObserver`.

### Pause behaviour

`player.pause(true)` still fires audio observers, but with samples
forced to `(0.0, 0.0)`. This is the documented contract:

- Recorders capture silence for the duration of the pause (so the
  output file's length matches the perceived musical time).
- Oscilloscopes / VU-meters draw a flatline.
- FFT analyzers see a silent window and typically emit a zero spectrum.

An observer that wants to distinguish "paused silence" from "musical
silence" must track it externally — keep a `paused: bool` on your side
and flip it when you call `player.pause()`.

### Ordering

When both observer kinds are registered, the player dispatches in this
order within a single sample:

1. `voices.mix(...)` — produces the stereo mix and (if any channels
   observer is registered) fills the per-channel capture buffer.
2. Every `ChannelsObserver::on_channels` (registration order).
3. Every `MixObserver::on_mix` (registration order).

This way a channels observer and a mix observer can cooperate: the
channels one sees the fine-grained state, then the mix one sees the
summed output, for the same sample.

### Example — a minimalist VU-meter

```rust
use xmrsplayer::prelude::*;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct VuMeter {
    peak_left: f32,
    peak_right: f32,
}

impl MixObserver for VuMeter {
    fn on_mix(&mut self, ctx: &MixContext) {
        let gain = ctx.global_volume * ctx.amplification;
        self.peak_left = self.peak_left.max((ctx.left * gain).abs());
        self.peak_right = self.peak_right.max((ctx.right * gain).abs());
    }
}

// The observer goes into a `Box<dyn MixObserver + Send>`, so `VuMeter`
// itself must be `Send`. `f32` is `Send`, so the derived auto-`Send`
// covers us here. If you want to *read* the meter from another thread,
// wrap it in `Arc<Mutex<VuMeter>>` yourself and install a thin
// forwarder observer that pushes into it — the player only needs to
// own a `Send` object, not a `Sync` one.
```

### Example — recording to a WAV file

```rust
use hound::{WavWriter, WavSpec, SampleFormat};

struct Recorder {
    writer: WavWriter<std::io::BufWriter<std::fs::File>>,
}

impl MixObserver for Recorder {
    fn on_mix(&mut self, ctx: &MixContext) {
        let gain = ctx.global_volume * ctx.amplification;
        let l = (ctx.left * gain).clamp(-1.0, 1.0);
        let r = (ctx.right * gain).clamp(-1.0, 1.0);
        // `write_sample` is O(1), no allocation — safe on audio thread.
        self.writer.write_sample((l * 32767.0) as i16).ok();
        self.writer.write_sample((r * 32767.0) as i16).ok();
    }
}
```

Note: `hound`'s `write_sample` buffers internally, so it doesn't block
on each call. For very long recordings, the buffer will eventually need
to flush to disk — if that's a concern, record into an in-memory queue
from the observer and do the actual disk I/O from another thread.

### Alternative consumption path

The crate also exposes
[`XmrsPlayer::samples_from_channels`](crate::xmrsplayer::XmrsPlayer::samples_from_channels)
for code that wants to drive the player while getting per-channel data
back as a `Vec<(f32, f32)>`. That path **does not** fire audio observers —
it is considered an alternative to `sample()` / the `Iterator` impl,
and firing observers on both would cause double dispatch if the two
paths were interleaved. Pick one path or the other for a given player
instance.
