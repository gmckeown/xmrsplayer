# XMrsPlayer — a safe, `no_std` soundtracker music player

XMrsPlayer plays real soundtracker modules, on desktops and on tiny embedded targets alike. The crate is **100% safe Rust** (`#![forbid(unsafe_code)]`) and builds on `no_std`; it reuses the data structures defined by the companion [`xmrs`](https://codeberg.org/sbechet/xmrs) crate.

It does not aim for bit-exact compatibility with every tracker ever written — that target is unreachable given the historical and ever-shifting nature of the demo scene — but it aims to be close enough to amaze you. :-)

## Supported formats

| Format            | Status                                                                 |
| ----------------- | ---------------------------------------------------------------------- |
| Amiga Module (MOD)| Works                                                                  |
| FastTracker XM    | Works                                                                  |
| ScreamTracker S3M | Works                                                                  |
| Impulse Tracker IT| WIP — loader done, playback in progress                         |
| SID (C64)         | Experimental — reverse-engineered from Rob Hubbard's 6510 player       |

A word on **SID**: tracks and effects can already be extracted from several of Hubbard's hits and played back through the generic module engine. One thing is still needed to go further — working *per-track* to reconstruct patterns (some tracks change length, which defeats a naive pattern builder), The module engine is already generic enough; the rest is a matter of time (and motivated contributors — Rust speakers welcome!).

FM instruments are supported by the underlying `xmrs` data structures and are ready for future formats.

## Using it as a library

```sh
cargo add xmrsplayer
```

Minimal example (desktop, `std`):

```rust
use xmrs::prelude::*;
use xmrs::fixed::units::Amplification;
use xmrsplayer::prelude::*;

let bytes = std::fs::read("song.xm")?;
let module = Module::load(&bytes)?;

// Default voice pool capacity (256, matching schism's MAX_VOICES) —
// see `XmrsPlayer::new_with_voice_pool_capacity` if you need more
// (NNA-heavy modules) or fewer (embedded targets).
let mut player = XmrsPlayer::new(&module, 48_000, /* song */ 0);
player.set_amplification(Amplification::UNITY); // unity by default; raise for more presence
player.set_max_loop_count(1);                   // 0 = loop forever

// The player is an `Iterator<Item = i16>` producing Q1.15 interleaved
// L/R samples. Most audio backends (cpal, rodio, hound, ...) accept i16
// natively; if your backend wants f32, divide each sample by 32768.0 at
// the boundary.
for sample in player.by_ref().take(48_000 * 2) {
    // hand `sample` to your audio backend
}
```

A complete CLI player built on `cpal` is shipped as the `xmrsplayer` binary
(see *Installing the CLI player* below). The `examples/` directory contains
two focused demos: `arp_bug.rs` (a regression check for FT2 arpeggio) and
`inspect_mod.rs` (a diagnostic dumper that prints the parsed module's
instruments, pattern-order, DAW tracks/clips/timeline map, detected
Euclidean wrappers, and the result of `verify_layers_consistent`).

For modules with very heavy NNA polyphony (the `Another Life`-class
files that stack hundreds of `NNA = Continue` voices), you can raise
the pool capacity at construction:

```rust
let mut player = XmrsPlayer::new_with_voice_pool_capacity(
    &module, 48_000, /* song */ 0, /* capacity */ 1024
);
```

The default is sized to match how the original trackers behaved — go
above it only when you have evidence the song needs it.

The player also exposes a subscription API — hook into row/tick changes with `PlayerObserver`, into the produced audio stream with `MixObserver` / `ChannelsObserver` (VU-meters, WAV recorders, visualisers), or into MIDI events emitted by IT `Zxx` macros with `MidiObserver` (bridge to external synths or hardware). See [OBSERVERS.md](OBSERVERS.md).

### The DAW timeline layer (transparent to the player)

Since the DAW migration, every imported `Module` carries a second
representation of the song alongside the legacy `Pattern → Row →
TrackUnit` matrix:

- `module.tracks: Vec<Track>` — one track per (song, instrument) pair,
  carrying the actual `Vec<TrackUnit>` content.
- `module.clips: SortedClips` — placements of those tracks on the
  timeline (`song`, `target_channel`, `position_tick`, `end_tick`).
- `module.timeline_map: TimelineMap` — the linearised `pattern_order`
  used to compute absolute ticks.

The player calls `Module::row_at(song, pattern, row)`, which routes
through the DAW layer when populated (and through `Module.pattern[…]`
otherwise) — so existing code keeps working without any opt-in. For an
authored module you can build the DAW layer with
`xmrs::daw::build_timeline::build_timeline_layer`; importers do it
automatically.

The same indirection also enables **Euclidean rhythm wrappers**: when
a Track repeats a single (note, instrument, no-effect) cell on a
strict Bjorklund pattern, `auto_convert_strict_euclidean_tracks`
collapses the Track to a single prototype row and stashes the
`(events, steps, rotation)` triplet in an empty `Instrument` slot
typed as `InstrumentType::Euclidean(InstrEkn)`. The cells are
regenerated on-the-fly by `Module::row_at`, so playback is
bit-identical while the on-disk Track is tiny.

### Talking to the player in `f32`

The player's API is fixed-point: `Amplification` is Q4.12, the iterator yields Q1.15 `i16`. Most desktop integrations want to expose a slider in linear `f32` units, so `xmrs` provides one-line bridges from / to `f32` for every Q-format newtype, gated behind the `float-helpers` cargo feature.

The default `xmrsplayer` build (the `std` feature is on) already forwards `xmrs/float-helpers`, so the conversions are available out of the box on desktop:

```rust
use xmrs::fixed::units::Amplification;

// f32 → Amplification: clamps to Q4.12's representable range
// `[-8.0, +8.0)`. Use this at the UI boundary, exactly once per
// numeric edit; store the resulting `Amplification`, don't keep
// the f32 around as the source of truth (every f32 ↔ Q round
// trip drifts by up to one LSB).
let user_slider: f32 = 1.5;
player.set_amplification(Amplification::from_f32(user_slider));

// Amplification → f32: read back for display.
let current: f32 = player.amplification().to_f32();
```

`Volume`, `Panning`, `Amp`, `EnvValue`, `Frequency`, `Period`, etc. all expose the same `from_f32` / `to_f32` pair under the same feature.

If you're targeting `no_std` (`default-features = false`), or you want the smallest possible build with no soft-float pull-in, leave the feature off and stay in fixed-point: `Amplification::UNITY`, `Amplification::from_int(2)` (= 2× gain), or `Amplification::from_raw_q4_12(0x1800)` (= 1.5× gain) cover the common cases without any `f32`.

## Cargo features

`xmrsplayer` has opt-in features so you only pay for what you use.

**Default:** `["std", "import"]` — the common desktop/server case.

### Runtime

| Feature      | Effect                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------- |
| `std`        | Enable the standard library and forward `std` + `float-helpers` to `xmrs`. The latter exposes `f32 ↔ Q` conversions on every fixed-point type, which desktop editors / GUIs use for level-meters and numeric inputs. Embedded users skip both with `default-features = false`. |
| `demo`       | Everything needed to build the example CLI player: pulls in `std`, `import`, `clap`, `console`, `cpal`, `hound`. |

The whole gain / pitch / LFO / filter / sample chain is integer
Q-format, end-to-end. No `libm`, no `micromath`, no `num-traits`. The
crate compiles `no_std` out of the box without picking a math
backend — there's nothing to pick.

### Format importers (propagated to `xmrs`)

| Feature       | Format                    |
| ------------- | ------------------------- |
| `import`      | Umbrella — all of the below |
| `import_mod`  | Amiga MOD                 |
| `import_xm`   | FastTracker XM            |
| `import_s3m`  | ScreamTracker S3M         |
| `import_it`   | Impulse Tracker IT        |
| `import_sid`  | C64 SID                   |

Build examples:

```sh
# Tiny no_std build, XM only
cargo build --no-default-features --features "import_xm" --release

# Embedded build with multiple importers
cargo build --no-default-features --features "import_mod import_xm" --release

# Desktop build (default)
cargo build --release
```

## Installing the CLI player

The `demo` feature ships a standalone CLI player (`xmrsplayer`) built on `cpal`.

From crates.io:

```sh
cargo install xmrsplayer --features demo
```

From a local checkout:

```sh
cargo install --path . --features demo
```

Run it directly from the git repo:

```sh
cargo run --features demo --release -- --help
```

Typical invocation:

```sh
xmrsplayer -f song.xm                 # play
xmrsplayer -f song.xm -o song.wav     # render to WAV
xmrsplayer -f song.xm -a 0.25 -l 1    # one loop, amplification 0.25
```

Interactive keys while playing: `Space` to pause, `←`/`→` to move, `Enter`/`i` for info, `Esc` or `q` to quit.

## Design notes

- **Safety first.** Both this crate and its dependency `xmrs` are `#![forbid(unsafe_code)]`.
- **Dependencies are lean**, and gated behind features — this matters not only for embedded.
- **Concert pitch by default.** Notes resolve to MIDI-aligned frequencies (A-4 = 440 Hz exactly), so tracker output mixes cleanly with any 440-Hz-aligned source. The legacy FT2 / ProTracker reference values (≈ −2 cents in linear mode, ≈ +17 cents in Amiga mode) are still available in `xmrs::fixed::tables` for bit-numerical comparison against reference players.
- **Playback decisions are quirk-driven.** Per-format runtime behaviour — FT2's arpeggio LUT and `E60` pattern-loop bug, IT's persistent tremor state and pan-reset policy, the per-format `Sample.volume` semantics, and so on — is exposed as individual flags on `PlaybackQuirks` inside `Module::profile`. `module.profile.format` itself is metadata only (used for display, export, and editor UI). The `CompatibilityProfile::ft2()`, `it214()`, `pt()`, etc. constructors preset coherent quirk bundles per format; an editor or modern authored module can compose the quirks freely without lying about the format tag. See `xmrs::module::PlaybackQuirks` for the canonical list.
- **Embedded-friendly.** The code should be progressively degradable to older processors through successive optimizations, and it would be a lot of fun to see what LLVM can do with it on float-less architectures like the [Z80](https://github.com/jacobly0/llvm-project), the [MOS 6502 family](https://github.com/llvm-mos/llvm-mos), or on the 68k target.

## Contributing

The project was written with extension and sharing in mind — feel free to use it for your own projects. If you do, an issue on [the codeberg repository](https://codeberg.org/sbechet/xmrsplayer) mentioning it would be lovely (but not mandatory).

## License

MIT — see [`LICENSE`](./LICENSE).