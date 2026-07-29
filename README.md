# XMrsPlayer — a safe, `no_std` soundtracker music player

XMrsPlayer plays real soundtracker modules, on desktops and on tiny embedded targets alike. Beyond the tracker formats (MOD, XM, S3M, IT) it also plays David Whittaker (DW) Amiga tunes, and renders OPL2/AdLib FM and Commodore 64 SID instruments through clean-room synthesizers. The crate is **100% safe Rust** (`#![forbid(unsafe_code)]`) and builds on `no_std`; it reuses the data structures defined by the companion [`xmrs`](https://codeberg.org/sbechet/xmrs) crate.

It does not aim for bit-exact compatibility with every tracker ever written — that target is unreachable given the historical and ever-shifting nature of the demo scene — but it aims to be close enough to amaze you. :-)

## Supported formats

| Format            | Status                                                                 |
| ----------------- | ---------------------------------------------------------------------- |
| Amiga Module (MOD)| Works — replay semantics follow ProTracker (pt2-clone)                 |
| FastTracker XM    | Works — replay semantics follow FastTracker 2 (ft2-clone)              |
| ScreamTracker S3M | Works — replay semantics follow Schism Tracker                         |
| Impulse Tracker IT| Works — replay semantics follow Schism Tracker, validated over a 322-file corpus |
| David Whittaker DW| Works — reverse-engineered from the original `.dw` 68000 player binaries |
| SID (C64)         | Plays — Rob Hubbard's tunes through a cycle-accurate 6581/8580 synthesizer |

A word on **DW**: David Whittaker's Amiga tunes ship as self-contained 68000 player binaries with no magic header. The `xmrs` importer detects them structurally, reads the replayer's init-time constants, and re-interprets the original per-voice playback (per-voice loops, period-space vibrato, arpeggio pitch lanes, sub-song selection) into the generic `Module` model, so they play back through the same engine as the tracker formats. Load via `Module::load` (tried last in auto-detect) or `Module::load_dw`.

A word on **SID**: Commodore 64 tunes are played back the way the FM instruments are — through a dedicated synthesizer rather than the sample engine. The `xmrs` importer reverse-engineers Rob Hubbard's 6510 replayer into `InstrRobSid` instruments (a `SidVoice` patch plus his signature per-tick effect tables: waveform cycling, pulse-width sweep, frequency arpeggio, vibrato), and the player drives them through a cycle-accurate MOS 6581/8580 synthesis core (`xmrs::generators::sid`, built on the `sidera` chip model). The mapping is one chip per instrument, with the chip's three hardware voices serving as that instrument's polyphony pool — exactly like the OPL driver's slots. Note pitch, portamento, freq slides and vibrato track each note like a sample voice. The imported `Module` is fully generic, so volume, panning, automation and DAW routing are all representable and editable like any other track — but at render the chip is currently summed mono into the master (only per-channel mute is honoured), so those per-track controls aren't yet applied to SID voices.

A word on **FM**: OPL2 / AdLib instruments embedded in S3M modules play back through a clean-room OPL2 synthesizer (`xmrs::generators::opl`), modeled on the documented YMF262 (OPL3) hardware behaviour with integer-only, const-generated tables — no vendored emulator code. The same `xmrs` data structures are ready for OPL in future formats (MPTM, native FM patches).

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
Euclidean tracks, and the result of `verify_layers_consistent`).

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

### The DAW timeline layer

Every imported `Module` stores songs natively in a DAW-style layout —
the legacy `Pattern → Row → TrackUnit` matrix is gone:

- `module.tracks: Vec<Track>` — an enum with three variants.
  `Track::Notes` carries the classic per-row `Vec<Cell>` (one cell
  per row); `Track::Euclidean` is a Bjorklund generator that
  synthesises one cell per row at playback time (see
  *Euclidean rhythm tracks* below); `Track::Audio` is a
  recorded-audio clip (see *The DAW signal graph* below). The note
  variants bind to a fixed `instrument` index. Cells carry a typed
  `CellEvent` (`NoteOn { pitch, velocity }`, `InstrReset`,
  `NoteOff { retrig }`, …), a vector of **discrete**
  `TrackEffect`s (Arpeggio, NoteDelay/Cut/Off/Retrig/FadeOut,
  Tremor, Volume/Panning/ChannelVolume set, instrument switches,
  MidiMacro), and a per-note `NoteExpression` (pitch-bend / volume /
  pan — neutral for every tracker import).
- `module.clips: SortedClips` — placements of those tracks on the
  timeline (`song`, `target_channel`, `position_tick`, `end_tick`).
- `module.automation: Vec<AutomationLane>` — every **continuous**
  modulation (Vibrato/Tremolo/Panbrello/Portamento/TonePortamento/
  VolumeSlide/ChannelVolumeSlide/PanningSlide per-Track + Bpm/Speed/
  GlobalVolume + their slides song-level) extracted upstream from
  `TrackImportUnit` at import time. The player consumes the lanes
  exclusively for those classes — there is no cell-side fallback.
- `module.timeline_map: TimelineMap` — the linearised `pattern_order`
  used to compute absolute ticks. Absorbs every navigation effect
  (`Bxx` / `Dxx` / `E6y` / `EEy` + restart byte) at import time so
  the sequencer just walks `entries` linearly.

The player calls `Module::row_at(song, pattern, row)`, which routes
through the DAW layer (the only path — `Module.pattern[…]` no longer
exists). For an authored module you can build the DAW layer with
`xmrs::daw::build_timeline::build_timeline_layer`; importers do it
automatically.

The same indirection also enables **Euclidean rhythm tracks**: when
a `Track::Notes` repeats a single (note, no-effect) cell on a strict
Bjorklund pattern, `auto_convert_strict_euclidean_tracks` rewrites
it in place as a `Track::Euclidean` variant carrying the
`(events, steps, rotation)` parameters and the single prototype
`Cell` directly on the Track. The cells are regenerated on-the-fly
by `Module::row_at`, so playback is bit-identical while the on-disk
Track is tiny.

### The DAW signal graph

The player also renders the `xmrs` **DAW generalization** — a native
signal graph (`Device` insert chains per channel, aux/return `Bus`es
fed by `Send`s, a master chain), recorded-audio `Track::Audio` clips
(linear-resampled, routed through the same channel strip as notes and
drawn from the shared `Module::assets` pool), `DeviceParam`
automation, per-voice `ModScope::Voice` modulation, and per-note
`NoteExpression`. The contract stays strict for the modulation and
routing model: a loaded MOD / XM / S3M / DW module leaves the whole
graph empty / neutral, so its render is **bit-identical** to a build
without it. The one deliberate exception is IT mix-plugins — the loader
projects a module's DMO / VST `mix_plugins` onto native device inserts,
so a plugin-bearing `.it` renders its effects (a plugin-free `.it` stays
bit-identical). Everything else exists for editors and authored modules
built on the `xmrs` data model.

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
| `import_dw`   | David Whittaker `.dw`     |
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
xmrsplayer -f tune.sid -s 2           # a C64 tune, sub-song 2
```

Every format is recognised from the file's own bytes — `-f` is the only
way in. A `.sid` is no exception: its sub-songs are sub-songs of one
module, picked with `-s`, exactly like a `.dw`.

Interactive keys while playing: `Space` to pause, `←`/`→` to move, `Enter`/`i` for info, `Esc` or `q` to quit.

## Design notes

- **Safety first.** Both this crate and its dependency `xmrs` are `#![forbid(unsafe_code)]`.
- **Dependencies are lean**, and gated behind features — this matters not only for embedded.
- **Concert pitch by default.** Notes resolve to MIDI-aligned frequencies (A-4 = 440 Hz exactly), so tracker output mixes cleanly with any 440-Hz-aligned source. The legacy FT2 / ProTracker reference values (≈ −2 cents in linear mode, ≈ +17 cents in Amiga mode) are still available in `xmrs::fixed::tables` for bit-numerical comparison against reference players.
- **Playback decisions are quirk-driven.** Per-format runtime behaviour — FT2's arpeggio LUT and `E60` pattern-loop bug, IT's persistent tremor state and pan-reset policy, the per-format `Sample.volume` semantics, and so on — is exposed as individual flags on `module.quirks` (a `PlaybackQuirks`). The sibling `module.origin` tag (an `Option<ModuleFormat>`) is metadata only (used for display, export, and editor UI). The `xmrs::tracker::profiles::ft2()`, `it214()`, `pt()`, etc. free functions preset coherent quirk bundles per format, and `PlaybackQuirks::default()` is the quirk-free editor default; an editor or modern authored module can compose the quirks freely without lying about the format tag. See `xmrs::core::compatibility::PlaybackQuirks` for the canonical list.
- **Embedded-friendly.** The code should be progressively degradable to older processors through successive optimizations, and it would be a lot of fun to see what LLVM can do with it on float-less architectures like the [Z80](https://github.com/jacobly0/llvm-project), the [MOS 6502 family](https://github.com/llvm-mos/llvm-mos), or on the 68k target.

## Contributing

The project was written with extension and sharing in mind — feel free to use it for your own projects. If you do, an issue on [the codeberg repository](https://codeberg.org/sbechet/xmrsplayer) mentioning it would be lovely (but not mandatory).

## License

MIT — see [`LICENSE`](./LICENSE).