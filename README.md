# XMrsPlayer — a safe, `no_std` soundtracker music player

XMrsPlayer plays real soundtracker modules, on desktops and on tiny embedded targets alike. The crate is **100% safe Rust** (`#![forbid(unsafe_code)]`) and builds on `no_std`; it reuses the data structures defined by the companion [`xmrs`](https://codeberg.org/sbechet/xmrs) crate.

It does not aim for bit-exact compatibility with every tracker ever written — that target is unreachable given the historical and ever-shifting nature of the demo scene — but it aims to be close enough to amaze you. :-)

## Supported formats

| Format            | Status                                                                 |
| ----------------- | ---------------------------------------------------------------------- |
| Amiga Module (MOD)| Works                                                                  |
| FastTracker XM    | Works                                                                  |
| ScreamTracker S3M | Works                                                                  |
| Impulse Tracker IT| WIP — loader mostly done, playback in progress                         |
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
use xmrsplayer::prelude::*;

let bytes = std::fs::read("song.xm")?;
let module = Module::load(&bytes)?;

let mut player = XmrsPlayer::new(&module, 48_000.0, /* song */ 0);
player.set_amplification(0.25);       // lower to avoid clipping on busy modules
player.set_max_loop_count(1);         // 0 = loop forever

// The player is an Iterator<Item = f32> producing interleaved L/R samples.
for sample in player.by_ref().take(48_000 * 2) {
    // hand `sample` to your audio backend (cpal, rodio, hound, ...)
}
```

See the `examples/` directory for complete players built on `cpal` and `rodio`.

The player also exposes a subscription API — hook into row/tick changes with `PlayerObserver`, into the produced audio stream with `MixObserver` / `ChannelsObserver` (VU-meters, WAV recorders, visualisers), or into MIDI events emitted by IT `Zxx` macros with `MidiObserver` (bridge to external synths or hardware). See [OBSERVERS.md](OBSERVERS.md).

## Cargo features

`xmrsplayer` has opt-in features so you only pay for what you use.

**Default:** `["std", "import"]` — the common desktop/server case.

### Runtime

| Feature      | Effect                                                                                            |
| ------------ | ------------------------------------------------------------------------------------------------- |
| `std`        | Enable the standard library and use `f32`'s inherent methods for float math. Forwarded to `xmrs`. |
| `libm`       | `no_std` float backend via `num-traits` + `libm`.                                                 |
| `micromath`  | `no_std` float backend via `micromath` (smaller, slightly less precise).                          |
| `demo`       | Everything needed to build the example CLI players: pulls in `std`, `import`, `clap`, `console`, `cpal`, `hound`, `rodio`. |

When `std` is disabled you **must** pick exactly one of `libm` or `micromath`; otherwise the crate emits a `compile_error!`.

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
# Tiny no_std build, XM only, libm backend
cargo build --no-default-features --features "libm import_xm" --release

# no_std with micromath instead
cargo build --no-default-features --features "micromath import_xm" --release

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
- **FT2/XM quirks are format-driven.** Setting `module.format = ModuleFormat::Xm` activates the canonical FT2 reference behaviour (arpeggio LUT, `E60` pattern-loop bug, arpeggio period clamp, portamento-down signed-overflow, `K00`-eats-note, vol-col `Bx` advancing vibrato). There is no separate `ft2_quirks` flag — the format *is* the switch.
- **Embedded-friendly.** The code should be progressively degradable to older processors through successive optimizations, and it would be a lot of fun to see what LLVM can do with it on float-less architectures like the [Z80](https://github.com/jacobly0/llvm-project), the [MOS 6502 family](https://github.com/llvm-mos/llvm-mos), or on the 68k target.

## Contributing

The project was written with extension and sharing in mind — feel free to use it for your own projects. If you do, an issue on [the codeberg repository](https://codeberg.org/sbechet/xmrsplayer) mentioning it would be lovely (but not mandatory).

## License

MIT — see [`LICENSE`](./LICENSE).