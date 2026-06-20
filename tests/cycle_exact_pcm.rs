//! Cycle-exact playback smoke tests. Importers produce a populated
//! DAW timeline; the player walks that timeline. There's a single
//! cell source now (post-3b), so the tests verify that the player
//! actually produces non-silent audio on representative bundled
//! demos rather than comparing two modes.

#![cfg(all(feature = "std", any(feature = "import_xm", feature = "import")))]

use xmrs::prelude::*;
use xmrs::tracker::import::build::{Pattern, Row};
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn render_pcm(player: &mut XmrsPlayer, samples: usize) -> Vec<i16> {
    let mut buf = Vec::with_capacity(samples * 2);
    let it = player.into_iter();
    for s in it.take(samples * 2) {
        buf.push(s);
    }
    while buf.len() < samples * 2 {
        buf.push(0);
    }
    buf
}

#[test]
fn note_xm_produces_audio() {
    let data = include_bytes!("../../xmrs/examples/note.xm");
    let module = Module::load_xm(data).expect("load");
    let mut player = XmrsPlayer::new(&module, 48000, 0);
    let pcm = render_pcm(&mut player, 4096);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "note.xm should produce audio");
}

#[cfg(feature = "import_sid")]
#[cfg(feature = "import_sid")]
#[test]
fn sid_commando_synth_path_produces_audio() {
    // `to_modules()` builds the real `RobSid` instruments, which the player
    // renders through the cycle-accurate `sidera` chip (one per instrument).
    let sid = xmrs::tracker::import::sid::sid_module::SidModule::get_sid_commando();
    let modules = sid.to_modules();
    let module = &modules[0];

    let mut player = XmrsPlayer::new(module, 48000, 0);
    let pcm = render_pcm(&mut player, 8192);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(
        nonzero > 0,
        "SID synth (sidera) playback should produce audio"
    );

    // Deterministic: a second identical render must match bit-for-bit.
    let mut player2 = XmrsPlayer::new(module, 48000, 0);
    let pcm2 = render_pcm(&mut player2, 8192);
    assert_eq!(pcm, pcm2, "SID synth render must be deterministic");
}

fn make_euclidian_module(humanize_prob: xmrs::core::fixed::fixed::Q15, advance_max: u8) -> Module {
    use xmrs::core::cell::CellEvent;
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
    use xmrs::core::daw::track::Track;
    use xmrs::core::fixed::fixed::Q15;

    let data: Vec<i8> = (0..64)
        .map(|i| if i < 32 { 127i8 } else { -128i8 })
        .collect();
    let sample = Sample {
        name: "sq".into(),
        relative_pitch: 0,
        finetune: Finetune::default(),
        volume: ChannelVolume::FULL,
        default_note_volume: Volume::FULL,
        panning: Panning::CENTER,
        loop_flag: LoopType::Forward,
        loop_start: 0,
        loop_length: 64,
        sustain_loop_flag: LoopType::No,
        sustain_loop_start: 0,
        sustain_loop_length: 0,
        data: Some(SampleDataType::Mono8(data.into())),
    };
    let mut sample_instr = InstrDefault::default();
    sample_instr.sample.push(Some(sample));
    sample_instr.keyboard.sample_for_pitch = [Some(0); 120];

    let mut module = Module::default();
    module.instrument.push(Instrument {
        name: "sq".into(),
        instr_type: InstrumentType::Default(sample_instr),
        muted: false,
    });

    // Bjorklund(3, 8) — fixed-events Euclidean track. The track
    // owns the rhythm parameters directly; no wrapper instrument.
    module.tracks.push(Track::Euclidean {
        name: "ekn".into(),
        instrument: 0,
        muted: false,
        events: 3,
        steps: 8,
        rotation: 0,
        prototype: Cell {
            event: CellEvent::NoteOn {
                pitch: Pitch::C4,
                velocity: Volume::FULL,
            },
            effects: Vec::new(),
            expression: xmrs::core::cell::NoteExpression::NEUTRAL,
        },
        humanize_advance_max_ticks: advance_max,
        humanize_probability: humanize_prob,
    });

    let speed = module.default_tempo as u32;
    // One pattern, 8 rows wide, played once on channel 0.
    module.clips = SortedClips::from_unsorted(vec![Clip {
        track: 0,
        song: 0,
        target_channel: 0,
        position_tick: 0,
        speed_at_start: module.default_tempo as u8,
        track_row_offset: 0,
        source_start_row: 0,
        end_tick: 8 * speed,
    }]);
    let _ = Q15::ONE;
    let mut entries = Vec::new();
    for row in 0..8u32 {
        entries.push(TimelineEntry {
            song: 0,
            order_idx: 0,
            pattern_idx: 0,
            row_idx: row,
            loop_iter: 0,
            tick: row * speed,
            speed_at_row: speed as u8,
            bpm_at_row: module.default_bpm as u16,
        });
    }
    module.timeline_map = TimelineMap { entries };
    module
}

fn hash_pcm(pcm: &[i16]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for s in pcm {
        for b in s.to_le_bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    h
}

// --- Shared scaffolding for the RFC §3C/§4 audio-clip tests ---------
//
// Every audio-clip test plays the same recorded waveform over the same
// flat 16-row timeline; only the routing/source under test differs.
// These two helpers hold that boilerplate in one place.

/// A 64-sample mono square (±100) that loops forever — the recorded
/// waveform the audio-clip tests stream.
fn rec_square_sample() -> Sample {
    let data: Vec<i8> = (0..64)
        .map(|i| if i < 32 { 100i8 } else { -100i8 })
        .collect();
    Sample {
        name: "rec".into(),
        relative_pitch: 0,
        finetune: Finetune::default(),
        volume: ChannelVolume::FULL,
        default_note_volume: Volume::FULL,
        panning: Panning::CENTER,
        loop_flag: LoopType::Forward, // loop so it sounds for the whole clip
        loop_start: 0,
        loop_length: 64,
        sustain_loop_flag: LoopType::No,
        sustain_loop_start: 0,
        sustain_loop_length: 0,
        data: Some(SampleDataType::Mono8(data.into())),
    }
}

/// Fill `m.timeline_map` with `rows` single-pattern rows at the module's
/// default speed/bpm and return that speed (ticks per row) — the flat
/// timeline the audio-clip tests place their clips over.
fn fill_linear_timeline(m: &mut Module, rows: u32) -> u32 {
    use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
    let speed = m.default_tempo as u32;
    let entries = (0..rows)
        .map(|row| TimelineEntry {
            song: 0,
            order_idx: 0,
            pattern_idx: 0,
            row_idx: row,
            loop_iter: 0,
            tick: row * speed,
            speed_at_row: speed as u8,
            bpm_at_row: m.default_bpm as u16,
        })
        .collect();
    m.timeline_map = TimelineMap { entries };
    speed
}

/// `Track::Euclidean` binds to a Default sample-based instrument
/// via its `instrument` index. The player triggers the referenced
/// sample on each Bjorklund pulse. Smoke test: non-silent audio.
#[test]
fn euclidian_indirection_plays_wrapped_instrument() {
    let module = make_euclidian_module(xmrs::core::fixed::fixed::Q15::ZERO, 0);
    let mut player = XmrsPlayer::new(&module, 48000, 0);
    let pcm = render_pcm(&mut player, 2048);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "Euclidean-wrapped sample should produce audio");
}

/// Build a Module with two single-channel patterns, each carrying
/// one trigger of `pitch_a` / `pitch_b` at row 0. Order = `[0, 1]`.
/// If `restart_byte > 0` the helper sets `module.song_loop_to` to
/// the tick of `(restart_byte, row 0)` so the TimelineMap-driven
/// sequencer (DAW migration Phase 3c.2) wraps to that order on
/// end-of-song instead of falling back to order 0.
fn make_two_pattern_module(pitch_a: Pitch, pitch_b: Pitch, restart_byte: usize) -> Module {
    use xmrs::tracker::import::build::{build_timeline_layer, set_song_loop_to_restart_byte};

    let data: Vec<i8> = (0..64)
        .map(|i| if i < 32 { 127i8 } else { -128i8 })
        .collect();
    let sample = Sample {
        name: "sq".into(),
        relative_pitch: 0,
        finetune: Finetune::default(),
        volume: ChannelVolume::FULL,
        default_note_volume: Volume::FULL,
        panning: Panning::CENTER,
        loop_flag: LoopType::Forward,
        loop_start: 0,
        loop_length: 64,
        sustain_loop_flag: LoopType::No,
        sustain_loop_start: 0,
        sustain_loop_length: 0,
        data: Some(SampleDataType::Mono8(data.into())),
    };
    let mut instr = InstrDefault::default();
    instr.sample.push(Some(sample));
    instr.keyboard.sample_for_pitch = [Some(0); 120];

    let mut module = Module::default();
    module.instrument.push(Instrument {
        name: "sq".into(),
        instr_type: InstrumentType::Default(instr),
        muted: false,
    });

    let mut row_a: Row = vec![xmrs::tracker::import::unit::TrackImportUnit::default()];
    row_a[0].note = CellNote::Play(pitch_a);
    row_a[0].instrument = Some(0);
    let mut row_b: Row = vec![xmrs::tracker::import::unit::TrackImportUnit::default()];
    row_b[0].note = CellNote::Play(pitch_b);
    row_b[0].instrument = Some(0);

    let pat_a: Pattern = (0..4)
        .map(|r| {
            if r == 0 {
                row_a.clone()
            } else {
                vec![xmrs::tracker::import::unit::TrackImportUnit::default()]
            }
        })
        .collect();
    let pat_b: Pattern = (0..4)
        .map(|r| {
            if r == 0 {
                row_b.clone()
            } else {
                vec![xmrs::tracker::import::unit::TrackImportUnit::default()]
            }
        })
        .collect();

    let pattern_order = vec![vec![0, 1]];
    let patterns = vec![pat_a, pat_b];
    build_timeline_layer(&mut module, &pattern_order, &patterns);
    set_song_loop_to_restart_byte(&mut module, 0, restart_byte);
    module
}

/// Regression guard for the historical "restart byte" (Soundtracker
/// MOD, FastTracker XM). Pre-3b the sequencer wrapped to
/// `Module.restart_position` at end-of-song; post-3b the importer
/// realises it as a `NavigationEffect::PositionJump` on the last played
/// row. This test plays a synthetic two-pattern module past the
/// natural end-of-song and verifies that:
///
/// 1. Without a restart byte (default wrap-to-order-0), the song
///    cycles `[A, B, A, B, …]`.
/// 2. With `restart_byte = 1`, the song cycles `[A, B, B, B, …]`.
/// 3. The two renders therefore produce different PCM after the
///    first full song traversal.
#[test]
fn restart_byte_loops_to_alternate_order() {
    // 4-row patterns × 6 ticks × 960 samples/tick at default BPM
    // = 23040 samples per song traversal. Render two traversals'
    // worth so the divergence is captured.
    const FRAMES: usize = 48000;

    let module_default = make_two_pattern_module(Pitch::C4, Pitch::C5, 0);
    let module_restart = make_two_pattern_module(Pitch::C4, Pitch::C5, 1);

    let mut p_default = XmrsPlayer::new(&module_default, 48000, 0);
    let mut p_restart = XmrsPlayer::new(&module_restart, 48000, 0);
    let pcm_default = render_pcm(&mut p_default, FRAMES);
    let pcm_restart = render_pcm(&mut p_restart, FRAMES);

    assert_ne!(
        hash_pcm(&pcm_default),
        hash_pcm(&pcm_restart),
        "restart_byte = 1 must redirect the end-of-song wrap to order 1, \
         producing a PCM different from the default wrap-to-0 traversal",
    );
    // Both renders must be reproducible on a second run with the
    // same module — sanity check for determinism.
    let mut p_default2 = XmrsPlayer::new(&module_default, 48000, 0);
    let mut p_restart2 = XmrsPlayer::new(&module_restart, 48000, 0);
    assert_eq!(
        hash_pcm(&pcm_default),
        hash_pcm(&render_pcm(&mut p_default2, FRAMES))
    );
    assert_eq!(
        hash_pcm(&pcm_restart),
        hash_pcm(&render_pcm(&mut p_restart2, FRAMES))
    );
}

/// Euclidean cycle-exactness: a Track with a Bjorklund(3, 8)
/// pattern rendered before vs. after
/// `auto_convert_strict_euclidean_tracks` must produce
/// bit-identical PCM — the auto-conversion is lossless by
/// construction (the wrapper instrument regenerates the same
/// `(note, instrument)` cells on the same canonical pulse grid).
#[test]
fn euclidean_auto_conversion_is_cycle_exact() {
    // Build a 1-cycle square sample.
    let data: Vec<i8> = (0..64)
        .map(|i| if i < 32 { 127i8 } else { -128i8 })
        .collect();
    let sample = Sample {
        name: "sq".into(),
        relative_pitch: 0,
        finetune: Finetune::default(),
        volume: ChannelVolume::FULL,
        default_note_volume: Volume::FULL,
        panning: Panning::CENTER,
        loop_flag: LoopType::Forward,
        loop_start: 0,
        loop_length: 64,
        sustain_loop_flag: LoopType::No,
        sustain_loop_start: 0,
        sustain_loop_length: 0,
        data: Some(SampleDataType::Mono8(data.into())),
    };
    let mut sample_instr = InstrDefault::default();
    sample_instr.sample.push(Some(sample));
    sample_instr.keyboard.sample_for_pitch = [Some(0); 120];

    fn make_module(sample_instr: InstrDefault, convert: bool) -> Module {
        use xmrs::core::daw::euclidean::auto_convert_strict_euclidean_tracks;
        use xmrs::core::daw::sorted_clips::SortedClips;
        use xmrs::tracker::import::build::{
            build_timeline_map, dedupe_tracks_by_content, extract_tracks_and_clips,
        };

        let mut m = Module::default();
        m.instrument.push(Instrument {
            name: "sq".into(),
            instr_type: InstrumentType::Default(sample_instr),
            muted: false,
        });
        // Pattern of 8 rows, Play(C4) at rows 0, 3, 6 — canonical
        // Bjorklund(3, 8). Pattern length = 8 = K.
        let mut play_row: Row = (0..1)
            .map(|_| xmrs::tracker::import::unit::TrackImportUnit::default())
            .collect();
        play_row[0].note = CellNote::Play(Pitch::C4);
        play_row[0].instrument = Some(0);
        let pattern: Pattern = (0..8)
            .map(|r| {
                if r == 0 || r == 3 || r == 6 {
                    play_row.clone()
                } else {
                    vec![xmrs::tracker::import::unit::TrackImportUnit::default()]
                }
            })
            .collect();
        // Run the same pipeline as `build_timeline_layer` but skip
        // the optional Euclidean-conversion pass when `convert` is
        // false. This is the only way to compare pre/post
        // conversion on identical underlying timeline data — the
        // previous "undo" hack can't restore the row collapse the
        // conversion now performs.
        let pattern_order = vec![vec![0usize]];
        let patterns = vec![pattern];
        let (timeline_map, walker_wrap) = build_timeline_map(&m, &pattern_order, &patterns);
        m.timeline_map = timeline_map;
        if let Some(t) = walker_wrap {
            m.song_loop_to = Some(t);
        }
        let (tracks, clips) = extract_tracks_and_clips(&patterns, &m.timeline_map);
        m.tracks = tracks;
        m.clips = SortedClips::from_unsorted(clips);
        dedupe_tracks_by_content(&mut m);
        if convert {
            auto_convert_strict_euclidean_tracks(&mut m);
        }
        m
    }

    let module_raw = make_module(sample_instr.clone(), /*convert=*/ false);
    let module_ekn = make_module(sample_instr, /*convert=*/ true);

    // Sanity: the converted module has a Track::Euclidean.
    assert!(
        module_ekn.tracks.iter().any(|t| t.is_euclidean()),
        "expected a Track::Euclidean after auto-conversion"
    );
    assert!(
        !module_raw.tracks.iter().any(|t| t.is_euclidean()),
        "raw module should carry only Track::Notes"
    );

    // Render both at default `HumanizeMode::Disabled`.
    let mut p_raw = XmrsPlayer::new(&module_raw, 48000, 0);
    let mut p_ekn = XmrsPlayer::new(&module_ekn, 48000, 0);
    const FRAMES: usize = 32768;
    let pcm_raw = render_pcm(&mut p_raw, FRAMES);
    let pcm_ekn = render_pcm(&mut p_ekn, FRAMES);
    assert_eq!(
        hash_pcm(&pcm_raw),
        hash_pcm(&pcm_ekn),
        "Euclidean auto-conversion must be lossless — same PCM pre/post"
    );
}

/// With `HumanizeMode::Deterministic(seed)` AND
/// `humanize_probability > 0`, the player must produce a PCM
/// *different* from the same module played with the default
/// `HumanizeMode::Disabled`. Proves humanisation actually fires.
#[test]
fn humanise_deterministic_changes_pcm() {
    use xmrs::core::fixed::fixed::Q15;
    use xmrsplayer::humanize::HumanizeMode;
    // Probability ONE so the roll always hits; advance_max_ticks = 3
    // (within speed=6 default → safe to fire at tick 3-5).
    let module = make_euclidian_module(Q15::ONE, 3);

    // ~5 rows at default tempo: 6 ticks × 960 samples/tick at BPM
    // 125 = 5760 samples/row → 32768 samples covers ~5.7 rows,
    // enough to reach the second pulse at row 4.
    const FRAMES: usize = 32768;

    // Reference render: Disabled mode = cycle-exact natural triggers.
    let mut p_off = XmrsPlayer::new(&module, 48000, 0);
    let pcm_off = render_pcm(&mut p_off, FRAMES);

    // Humanise render: Deterministic seed = reproducible.
    let mut p_on = XmrsPlayer::new(&module, 48000, 0);
    p_on.set_humanize_mode(HumanizeMode::Deterministic(42));
    let pcm_on = render_pcm(&mut p_on, FRAMES);

    assert_ne!(
        hash_pcm(&pcm_off),
        hash_pcm(&pcm_on),
        "Humanise probability ONE + Deterministic mode should perturb the PCM"
    );

    // Same seed → reproducible.
    let mut p_on2 = XmrsPlayer::new(&module, 48000, 0);
    p_on2.set_humanize_mode(HumanizeMode::Deterministic(42));
    let pcm_on2 = render_pcm(&mut p_on2, FRAMES);
    assert_eq!(
        hash_pcm(&pcm_on),
        hash_pcm(&pcm_on2),
        "Deterministic(42) must produce the same PCM twice"
    );
}

/// With `HumanizeMode::Disabled` (the default), the PCM must be
/// identical regardless of the `Track::Euclidean`'s `humanize_*`
/// parameter values. The track-side params describe the *shape*;
/// the player must obey its mode-side gate.
#[test]
fn humanise_disabled_is_cycle_exact() {
    use xmrs::core::fixed::fixed::Q15;
    let mod_zero = make_euclidian_module(Q15::ZERO, 0);
    let mod_loaded = make_euclidian_module(Q15::HALF, 4);
    let mut p_zero = XmrsPlayer::new(&mod_zero, 48000, 0);
    let mut p_loaded = XmrsPlayer::new(&mod_loaded, 48000, 0);
    // Both default to HumanizeMode::Disabled.
    assert_eq!(
        p_zero.humanize_mode(),
        xmrsplayer::humanize::HumanizeMode::Disabled
    );
    assert_eq!(
        p_loaded.humanize_mode(),
        xmrsplayer::humanize::HumanizeMode::Disabled
    );
    let pcm_zero = render_pcm(&mut p_zero, 2048);
    let pcm_loaded = render_pcm(&mut p_loaded, 2048);
    assert_eq!(
        hash_pcm(&pcm_zero),
        hash_pcm(&pcm_loaded),
        "HumanizeMode::Disabled must ignore humanize_* params"
    );
}

// =====================================================================
// PCM-byte-perfect reference tests
// =====================================================================
//
// DAW migration Phase 3a establishes the "byte-perfect PCM"
// safety net before any continuous-effect extraction work begins.
// Each test renders a fixed number of samples from a bundled
// fixture, then compares the result byte-for-byte against a stored
// `.pcm` file in `tests/fixtures_pcm/`.
//
// **Regenerating fixtures.** When intentional renderer changes
// shift the canonical output, the fixtures need to be re-baked.
// Run the test suite with `XM_PCM_REGEN=1` to write the current
// renderer output as the new reference:
//
// ```sh
// XM_PCM_REGEN=1 cargo test --test cycle_exact_pcm -- pcm_byte_perfect
// ```
//
// Without the env var, missing-fixture and mismatched-fixture
// cases both fail loudly so an accidental regression is caught.

const PCM_FIXTURE_DIR: &str = "tests/fixtures_pcm";

/// Serialise a `Vec<i16>` of stereo-interleaved samples as raw
/// little-endian bytes — the on-disk format we compare against.
fn pcm_to_le_bytes(pcm: &[i16]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(pcm.len() * 2);
    for s in pcm {
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

/// Either compare the rendered PCM byte-for-byte against the stored
/// fixture (default) or write a fresh fixture and exit early
/// (`XM_PCM_REGEN=1`). Surfaces the cargo-relative fixture path in
/// every error message so the developer can locate the file
/// immediately.
fn assert_pcm_byte_perfect(fixture_name: &str, pcm: &[i16]) {
    use std::fs;
    use std::path::PathBuf;

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push(PCM_FIXTURE_DIR);
    path.push(format!("{}.pcm", fixture_name));

    let actual = pcm_to_le_bytes(pcm);

    if std::env::var("XM_PCM_REGEN").is_ok() {
        fs::create_dir_all(path.parent().unwrap()).expect("create_dir_all fixtures_pcm");
        fs::write(&path, &actual).expect("write fixture");
        // Don't assert — the test passes after a regen by design.
        return;
    }

    let expected = match fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) => panic!(
            "missing PCM fixture {} ({}). Run with `XM_PCM_REGEN=1` to bake \
             a fresh reference.",
            path.display(),
            e
        ),
    };

    if expected != actual {
        panic!(
            "PCM byte-perfect mismatch for {}: stored {} bytes, rendered {} bytes. \
             If the renderer change is intentional, regenerate with \
             `XM_PCM_REGEN=1 cargo test --test cycle_exact_pcm`. Path: {}",
            fixture_name,
            expected.len(),
            actual.len(),
            path.display(),
        );
    }
}

/// `note.xm` is the smoke-test reference fixture bundled with
/// `xmrs`. The first second of audio at 48 kHz makes a stable
/// signature for the byte-perfect comparison (the song's intro is
/// well under one second, so this captures both note triggers and
/// some sustained tail).
#[test]
fn pcm_byte_perfect_note_xm() {
    let data = include_bytes!("../../xmrs/examples/note.xm");
    let module = Module::load_xm(data).expect("load note.xm");
    let mut player = XmrsPlayer::new(&module, 48000, 0);
    // 1 second of stereo PCM @ 48 kHz.
    let pcm = render_pcm(&mut player, 48_000);
    assert_pcm_byte_perfect("note_xm", &pcm);
}

/// The Euclidean module (3-events / 8-step Bjorklund on a
/// square-wave sample) — pure-synthetic, deterministic. Render
/// length matches the canonical pulse spacing so both the converted
/// and the raw paths come through in the captured second.
#[test]
fn pcm_byte_perfect_euclidean() {
    let module = make_euclidian_module(xmrs::core::fixed::fixed::Q15::ZERO, 0);
    let mut player = XmrsPlayer::new(&module, 48000, 0);
    let pcm = render_pcm(&mut player, 48_000);
    assert_pcm_byte_perfect("euclidean_3_8", &pcm);
}

#[cfg(feature = "import_sid")]
#[test]
fn pcm_byte_perfect_sid_commando() {
    let sid = xmrs::tracker::import::sid::sid_module::SidModule::get_sid_commando();
    let modules = sid.to_modules();
    let module = &modules[0];
    let mut player = XmrsPlayer::new(module, 48000, 0);
    let pcm = render_pcm(&mut player, 48_000);
    assert_pcm_byte_perfect("sid_commando", &pcm);
}

/// SID corpus fidelity golden, referenced against the REAL chip.
///
/// Unlike a self-referential PCM golden (which only proves "nothing changed
/// from our own last output"), this pins every tune to the `sidplay` register
/// dump of the real 6502+SID — the fixtures in `tests/fixtures_sid_oracle/`
/// (one `frame,d400..d418` CSV per tune, generated offline from the oracle,
/// stored gzip-packed as `<tune>#<song>.csv.gz`; to refresh, capture with
/// `sidplay --regdump` and re-pack with `gzip -9`).
///
/// A naive per-frame bit-compare can't be the pass/fail rule: tunes differ from
/// the oracle by sub-frame timing drift and by genuine per-tune fidelity gaps
/// (delta / sanxion are WIP), so the raw divergence is large but STABLE. So the
/// golden is the per-tune COUNT of diverging voice-frames, frozen as a baseline:
///   badGate  gate bit (ctrl&1) differs                — note on/off misfire
///   badAd/Sr attack-decay / sustain-release differ    — envelope (the Monty cut)
///   badFhi   |freq-hi| diverges by > FREQ_HI_TOL       — pitch (the skydive)
/// Any movement (a tune drifting further from OR closer to the chip) flips its
/// counts and fails, NAMING the tune+metric — e.g. re-breaking Monty's skydive
/// spikes its `badFhi` from 0. Regenerate the baseline with `XM_PCM_REGEN=1`
/// (and only after confirming the move is an improvement against the oracle).
/// The counts double as a fidelity dashboard that should ratchet toward zero.
///
/// `#[ignore]` because it renders 16 s × 15 tunes of cycle-accurate SID (~236 M
/// chip cycles, ~1 min) — too slow for routine `cargo test`. It is the SID
/// fidelity GATE: run it explicitly before/after any SID change, and in CI:
///   `cargo test --test cycle_exact_pcm -- --ignored sid_oracle_fidelity`
/// (the register stream is rate-independent, so the chip emulation, not the
/// audio rate, is the floor; a register-only player path would let it auto-run).
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "expensive SID oracle gate — run with --ignored before/after SID work"]
fn sid_oracle_fidelity_golden() {
    use xmrs::tracker::import::sid::sid_module::SidModule;

    const SECS: usize = 16;
    const FREQ_HI_TOL: i32 = 8;

    type SidTune = (&'static str, fn() -> SidModule);
    let tunes: &[SidTune] = &[
        ("commando", SidModule::get_sid_commando),
        ("crazy_comets", SidModule::get_sid_crazy_comets),
        ("last_v8", SidModule::get_sid_last_v8),
        ("monty", SidModule::get_sid_monty_on_the_run),
        ("thing_on_a_spring", SidModule::get_sid_thing_on_a_spring),
        ("zoid", SidModule::get_sid_zoid),
        ("ace_2", SidModule::get_sid_ace_2),
        ("delta", SidModule::get_sid_delta),
        ("human_race", SidModule::get_sid_human_race),
        (
            "international_karate",
            SidModule::get_sid_international_karate,
        ),
        ("lightforce", SidModule::get_sid_lightforce),
        ("sanxion_song_1", SidModule::get_sid_sanxion_song_1),
        ("sanxion_song_2", SidModule::get_sid_sanxion_song_2),
        ("spellbound", SidModule::get_sid_spellbound),
        ("thrust", SidModule::get_sid_thrust),
    ];

    eprintln!(
        "\n{:22} {:^11}  {:>4} {:>4} {:>4} {:>4}   (counts are TIME-ALIGNED: capture-init",
        "tune", "shift v0/1/2", "gate", "ad", "sr", "fhi"
    );
    eprintln!(
        "{:22} {:^11}  {:>4} {:>4} {:>4} {:>4}    offset + frame-skip removed per voice)",
        "", "", "", "", "", ""
    );
    let mut rows: Vec<(String, [u32; 4])> = Vec::new();
    for (name, ctor) in tunes {
        // Every sub-song of the tune (`module i` ↔ the oracle's `--song i+1`),
        // each goldened independently — see fixtures `<tune>#<i>.csv`.
        let modules = ctor().to_modules();
        for (i, module) in modules.iter().enumerate() {
            let key = format!("{name}#{i}");
            xmrs::generators::sid::driver::capture_begin();
            let mut player = XmrsPlayer::new(module, 8000, 0);
            player.set_max_loop_count(1);
            for _ in player.take(SECS * 8000 * 2) {}
            let syn = xmrs::generators::sid::driver::capture_take();
            let ora = load_oracle_csv(&key);

            let (counts, shifts, saturated) = aligned_voice_counts(&syn, &ora, FREQ_HI_TOL);
            // `saturated` = a local window pinned to its search edge: the drift
            // outran the tracker, so the counts may still carry residual artefact.
            eprintln!(
                "{:24} {:>3}/{:>2}/{:>2}  {:>4} {:>4} {:>4} {:>4}{}",
                key,
                shifts[0],
                shifts[1],
                shifts[2],
                counts[0],
                counts[1],
                counts[2],
                counts[3],
                if saturated {
                    "  drift>window (read with care)"
                } else {
                    ""
                }
            );
            rows.push((key, counts));
        }
    }

    assert_sid_fidelity_baseline(&rows);
}

/// Compare our per-frame virtual-chip registers to the oracle AFTER removing the
/// three artefacts the raw index-to-index count conflated with real error:
///   * **frame-skip** — our player emits SID frames at a slightly-off rate
///     (fractional tempo ⇒ ~727 frames / 16 s where the 50 Hz oracle has 800),
///     so the two streams drift linearly apart. Removed by mapping oracle frame
///     `i` → synth frame `round(i · synth_len / oracle_len)`.
///   * **capture-init offset** — sidplay dumps a post-init idle frame 0 and the
///     first `play()` at frame 1, our capture dumps the first `play()` at frame
///     0 (a 0..6-frame constant lead, tune-dependent).
///   * **per-voice phase** — voices ring out / re-trigger a frame or two apart.
///
/// The last two are absorbed by searching, PER VOICE, the small integer `shift`
/// that best aligns the GATE + pitch (the structural timing signal) and counting
/// every metric at that shift. What survives is genuine value divergence: a gate
/// that fires on the wrong note, an envelope written differently, a pitch off by
/// more than `freq_hi_tol`. Returns `(summed [gate, ad, sr, fhi] counts, the
/// detected per-voice shift)` — the shift magnitude IS the artefact size.
/// Max per-voice realignment searched by [`aligned_voice_counts`] — covers the
/// capture-init offset (≤6) + per-voice phase (≤2); a shift pinned to this edge
/// means the drift exceeds the artefact range.
#[cfg(feature = "import_sid")]
const ALIGN_WINDOW: i32 = 8;

/// One GLOBAL best shift per voice (a per-window/DTW tracker was tried and is
/// strictly worse: on weak-signal windows it finds spurious local minima and
/// WALKS off — it wrecked the already-exact tunes like monty without fixing the
/// drifting ones). Robust by construction: a single shift can't match notes
/// out-of-order, so genuine value errors always survive. Returns `(summed
/// counts, per-voice shift, any-voice-saturated)`. A saturated shift (|shift| ==
/// ALIGN_WINDOW) OR a huge residual on an ALTERNATING register (gate that toggles
/// every frame via v30 wave-alt, or octave-arp freq-hi) is the *accepted*
/// 1-frame-phase artefact, NOT a value error — see `fixtures_sid_oracle/ACCEPTED.md`.
// The inner loops index the oracle by frame number `v`/`i` while ALSO using it
// for register-offset and synth-frame arithmetic (`4 + v*7`, `round(i·scale)`);
// the index is a coordinate driving math, not a plain cursor, so a range loop
// reads clearest here.
#[allow(clippy::needless_range_loop)]
#[cfg(feature = "import_sid")]
fn aligned_voice_counts(
    syn: &[[u8; 25]],
    ora: &[[u8; 25]],
    freq_hi_tol: i32,
) -> ([u32; 4], [i32; 3], bool) {
    let w = ALIGN_WINDOW;
    let (o, s) = (ora.len(), syn.len());
    let mut totals = [0u32; 4];
    let mut shifts = [0i32; 3];
    if o == 0 || s == 0 {
        return (totals, shifts, false);
    }
    let scale = s as f64 / o as f64;
    let map = |i: usize, shift: i32| -> Option<usize> {
        let j = (i as f64 * scale).round() as i32 + shift;
        (j >= 0 && (j as usize) < s).then_some(j as usize)
    };

    for v in 0..3 {
        let (c, ad, sr, fhi) = (4 + v * 7, 5 + v * 7, 6 + v * 7, 1 + v * 7);
        let mut best_shift = 0i32;
        let mut best_score = f64::INFINITY;
        for shift in -w..=w {
            let (mut bad, mut valid) = (0i64, 0i64);
            for i in 0..o {
                if let Some(j) = map(i, shift) {
                    valid += 1;
                    bad += ((syn[j][c] & 1) != (ora[i][c] & 1)) as i64;
                    bad += ((syn[j][fhi] as i32 - ora[i][fhi] as i32).abs() > freq_hi_tol) as i64;
                }
            }
            if valid > 0 {
                let score = bad as f64 / valid as f64;
                if score < best_score {
                    best_score = score;
                    best_shift = shift;
                }
            }
        }
        shifts[v] = best_shift;
        for i in 0..o {
            if let Some(j) = map(i, best_shift) {
                totals[0] += ((syn[j][c] & 1) != (ora[i][c] & 1)) as u32;
                totals[1] += (syn[j][ad] != ora[i][ad]) as u32;
                totals[2] += (syn[j][sr] != ora[i][sr]) as u32;
                totals[3] += ((syn[j][fhi] as i32 - ora[i][fhi] as i32).abs() > freq_hi_tol) as u32;
            }
        }
    }
    let saturated = shifts.iter().any(|&x| x.abs() == w);
    (totals, shifts, saturated)
}

/// Compare per-tune `[badGate, badAd, badSr, badFhi]` divergence-from-oracle
/// counts against the frozen baseline (`tests/fixtures_sid_oracle/baseline.txt`,
/// one `tune bg ba bs bf` line). Regenerates under `XM_PCM_REGEN=1`; otherwise
/// reports EVERY tune whose counts moved, with the metric and the direction
/// (`-` = closer to the chip = a fidelity win you should re-baseline; `+` = a
/// regression to investigate).
#[cfg(feature = "import_sid")]
fn assert_sid_fidelity_baseline(rows: &[(String, [u32; 4])]) {
    use std::collections::BTreeMap;
    use std::fmt::Write as _;
    use std::fs;
    use std::path::PathBuf;

    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures_sid_oracle");
    path.push("baseline.txt");

    if std::env::var("XM_PCM_REGEN").is_ok() {
        let mut out = String::new();
        for (n, c) in rows {
            writeln!(out, "{n} {} {} {} {}", c[0], c[1], c[2], c[3]).unwrap();
        }
        fs::write(&path, out).expect("write fidelity baseline");
        return;
    }

    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing fidelity baseline {} ({e}). Run `XM_PCM_REGEN=1 cargo test \
             --test cycle_exact_pcm` to bake it.",
            path.display()
        )
    });
    let base: BTreeMap<&str, [u32; 4]> = text
        .lines()
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let k = it.next()?;
            let v: Vec<u32> = it.filter_map(|x| x.parse().ok()).collect();
            (v.len() == 4).then(|| (k, [v[0], v[1], v[2], v[3]]))
        })
        .collect();

    let labels = ["gate", "ad", "sr", "fhi"];
    let mut diffs = String::new();
    for (name, cur) in rows {
        match base.get(name.as_str()) {
            Some(b) if b == cur => {}
            Some(b) => {
                let mut parts = String::new();
                for i in 0..4 {
                    if b[i] != cur[i] {
                        let sign = if cur[i] > b[i] { '+' } else { '-' };
                        write!(parts, " {}{}:{}->{}", sign, labels[i], b[i], cur[i]).unwrap();
                    }
                }
                writeln!(diffs, "  {name}:{parts}").unwrap();
            }
            None => writeln!(diffs, "  {name}: NEW {cur:?}").unwrap(),
        }
    }

    if !diffs.is_empty() {
        panic!(
            "SID oracle-fidelity moved (vs sidplay). `-metric` = closer to the chip \
             (re-baseline it), `+metric` = regression to investigate. If intended, \
             regenerate with `XM_PCM_REGEN=1 cargo test --test cycle_exact_pcm`.\n{diffs}Path: {}",
            path.display()
        );
    }
}

/// Load a `sidplay --regdump` oracle CSV from `tests/fixtures_sid_oracle/`.
///
/// The fixtures are stored gzip-compressed (`<name>.csv.gz`) — they are highly
/// redundant register dumps (~9.5 % packed) and storing them raw bloated the
/// tree by 2.6 MB. Decompressed here at load time.
#[cfg(feature = "import_sid")]
fn load_oracle_csv(name: &str) -> Vec<[u8; 25]> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    use std::path::PathBuf;
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures_sid_oracle");
    path.push(format!("{name}.csv.gz"));
    let bytes = std::fs::read(&path)
        .unwrap_or_else(|e| panic!("missing oracle fixture {} ({e})", path.display()));
    let mut text = String::new();
    GzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap_or_else(|e| panic!("corrupt gzip oracle fixture {} ({e})", path.display()));
    text.lines()
        .skip(1)
        .map(|l| {
            let mut a = [0u8; 25];
            for (i, x) in l.split(',').skip(1).enumerate().take(25) {
                a[i] = u8::from_str_radix(x.trim(), 16).unwrap_or(0);
            }
            a
        })
        .collect()
}

/// RFC §1B — the player **sums** every modulator lane on a parameter
/// (replacing the pre-1B `find_map` first-match). Tracker imports emit
/// at most one lane per `(target, kind)` so the fold is bit-identical
/// there (proven by the golden corpus); this test exercises the
/// *stacking* path a DAW author unlocks, where it must actually sum.
///
/// Build a single sustained C-4 with a continuously-armed vibrato
/// (a `Set` on every row so the LFO keeps advancing), then render with
/// 0, 1 and 2 *identical* vibrato lanes on the same `TrackPitch`:
///
/// * 0 → 1 lane: vibrato appears (sanity — modulation is live);
/// * 1 → 2 lanes: the 2nd lane's output is summed into the first,
///   doubling the pitch swing, so the PCM must differ. If the extras
///   were ignored (old first-match) the two renders would be identical.
#[test]
fn stacked_vibrato_lanes_sum() {
    use xmrs::core::cell::{Cell, CellEvent};
    use xmrs::core::daw::automation::{AutomationLane, AutomationTarget, LaneKind, LfoEvent};
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
    use xmrs::core::daw::track::Track;
    use xmrs::core::fixed::fixed::{Q15, Q8_8};

    const ROWS: u32 = 32;

    fn build(num_vibrato_lanes: usize) -> Module {
        let data: Vec<i8> = (0..64)
            .map(|i| if i < 32 { 127i8 } else { -128i8 })
            .collect();
        let sample = Sample {
            name: "sq".into(),
            relative_pitch: 0,
            finetune: Finetune::default(),
            volume: ChannelVolume::FULL,
            default_note_volume: Volume::FULL,
            panning: Panning::CENTER,
            loop_flag: LoopType::Forward,
            loop_start: 0,
            loop_length: 64,
            sustain_loop_flag: LoopType::No,
            sustain_loop_start: 0,
            sustain_loop_length: 0,
            data: Some(SampleDataType::Mono8(data.into())),
        };
        let mut instr = InstrDefault::default();
        instr.sample.push(Some(sample));
        instr.keyboard.sample_for_pitch = [Some(0); 120];

        let mut module = Module::default();
        module.instrument.push(Instrument {
            name: "sq".into(),
            instr_type: InstrumentType::Default(instr),
            muted: false,
        });

        let speed = module.default_tempo as u32;

        // One sustained C-4 at row 0; empty rows after.
        let mut rows = vec![Cell::default(); ROWS as usize];
        rows[0].event = CellEvent::NoteOn {
            pitch: Pitch::C4,
            velocity: Volume::FULL,
        };
        module.tracks.push(Track::Notes {
            name: "n".into(),
            instrument: 0,
            rows,
            muted: false,
        });

        module.clips = SortedClips::from_unsorted(vec![Clip {
            track: 0,
            song: 0,
            target_channel: 0,
            position_tick: 0,
            speed_at_start: module.default_tempo as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: ROWS * speed,
        }]);
        let mut entries = Vec::new();
        for row in 0..ROWS {
            entries.push(TimelineEntry {
                song: 0,
                order_idx: 0,
                pattern_idx: 0,
                row_idx: row,
                loop_iter: 0,
                tick: row * speed,
                speed_at_row: speed as u8,
                bpm_at_row: module.default_bpm as u16,
            });
        }
        module.timeline_map = TimelineMap { entries };

        // `num_vibrato_lanes` identical Vibrato lanes on TrackPitch(0),
        // each re-armed every row so the LFO keeps advancing for the
        // whole render.
        for _ in 0..num_vibrato_lanes {
            let events = (0..ROWS)
                .map(|row| LfoEvent::Set {
                    tick: row * speed,
                    speed: Q8_8::from_raw(0x40),
                    depth: Q15::from_raw(8000),
                    waveform: Waveform::BipolarSine,
                    retrig: true,
                })
                .collect();
            module.automation.push(AutomationLane::new_with_kind(
                AutomationTarget::TrackPitch(0),
                LaneKind::Lfo { events },
            ));
        }
        module
    }

    const FRAMES: usize = 24_000;
    let (m0, m1, m2) = (build(0), build(1), build(2));
    let mut p0 = XmrsPlayer::new(&m0, 48000, 0);
    let mut p1 = XmrsPlayer::new(&m1, 48000, 0);
    let mut p2 = XmrsPlayer::new(&m2, 48000, 0);
    let h0 = hash_pcm(&render_pcm(&mut p0, FRAMES));
    let h1 = hash_pcm(&render_pcm(&mut p1, FRAMES));
    let h2 = hash_pcm(&render_pcm(&mut p2, FRAMES));

    assert_ne!(
        h0, h1,
        "a single vibrato lane must modulate the pitch (sanity)"
    );
    assert_ne!(
        h1, h2,
        "a 2nd stacked vibrato lane must SUM into the pitch modulation \
         (RFC §1B fold) — N=2 must differ from N=1; if it doesn't, the \
         extra lane is being ignored (old first-match)"
    );
}

/// RFC §2B — a `ModScope::Voice` lane routes the channel's modulation
/// to **every** voice of the track, NNA ghosts included; the default
/// `ModScope::Track` leaves ghosts frozen (the historical behaviour,
/// proven bit-identical by the golden corpus). This test proves the
/// scope is *live*, not dead config.
///
/// An instrument with `NNA = Continue` plays C-4, then a second C-4 a
/// couple of rows later displaces the first into a still-ringing
/// ghost. A continuously-armed vibrato lane sits on `TrackPitch`.
/// Rendered once with the lane `Track`-scoped and once `Voice`-scoped:
/// under `Voice` the ghost also vibrates, so the PCM must differ.
#[test]
fn voice_scope_reaches_ghosts() {
    use xmrs::core::cell::{Cell, CellEvent};
    use xmrs::core::daw::automation::{
        AutomationLane, AutomationTarget, LaneKind, LfoEvent, ModScope,
    };
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
    use xmrs::core::daw::track::Track;
    use xmrs::core::fixed::fixed::{Q15, Q8_8};
    use xmrs::core::instrument_behavior::{DuplicateCheckType, NewNoteAction};

    const ROWS: u32 = 24;
    const SECOND_NOTE_ROW: u32 = 2;

    fn build(scope: ModScope) -> Module {
        let data: Vec<i8> = (0..64)
            .map(|i| if i < 32 { 127i8 } else { -128i8 })
            .collect();
        let sample = Sample {
            name: "sq".into(),
            relative_pitch: 0,
            finetune: Finetune::default(),
            volume: ChannelVolume::FULL,
            default_note_volume: Volume::FULL,
            panning: Panning::CENTER,
            loop_flag: LoopType::Forward,
            loop_start: 0,
            loop_length: 64,
            sustain_loop_flag: LoopType::No,
            sustain_loop_start: 0,
            sustain_loop_length: 0,
            data: Some(SampleDataType::Mono8(data.into())),
        };
        let mut instr = InstrDefault::default();
        instr.sample.push(Some(sample));
        instr.keyboard.sample_for_pitch = [Some(0); 120];
        // NNA = Continue: the second trigger detaches the first into a
        // ghost that keeps ringing on the looping sample.
        instr.behavior.duplicate_check = DuplicateCheckType::Off(NewNoteAction::Continue);

        let mut module = Module::default();
        module.instrument.push(Instrument {
            name: "sq".into(),
            instr_type: InstrumentType::Default(instr),
            muted: false,
        });

        let speed = module.default_tempo as u32;

        let mut rows = vec![Cell::default(); ROWS as usize];
        rows[0].event = CellEvent::NoteOn {
            pitch: Pitch::C4,
            velocity: Volume::FULL,
        };
        rows[SECOND_NOTE_ROW as usize].event = CellEvent::NoteOn {
            pitch: Pitch::C4,
            velocity: Volume::FULL,
        };
        module.tracks.push(Track::Notes {
            name: "n".into(),
            instrument: 0,
            rows,
            muted: false,
        });

        module.clips = SortedClips::from_unsorted(vec![Clip {
            track: 0,
            song: 0,
            target_channel: 0,
            position_tick: 0,
            speed_at_start: module.default_tempo as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: ROWS * speed,
        }]);
        let mut entries = Vec::new();
        for row in 0..ROWS {
            entries.push(TimelineEntry {
                song: 0,
                order_idx: 0,
                pattern_idx: 0,
                row_idx: row,
                loop_iter: 0,
                tick: row * speed,
                speed_at_row: speed as u8,
                bpm_at_row: module.default_bpm as u16,
            });
        }
        module.timeline_map = TimelineMap { entries };

        let events = (0..ROWS)
            .map(|row| LfoEvent::Set {
                tick: row * speed,
                speed: Q8_8::from_raw(0x40),
                depth: Q15::from_raw(8000),
                waveform: Waveform::BipolarSine,
                retrig: true,
            })
            .collect();
        let mut lane = AutomationLane::new_with_kind(
            AutomationTarget::TrackPitch(0),
            LaneKind::Lfo { events },
        );
        lane.scope = scope;
        module.automation.push(lane);
        module
    }

    const FRAMES: usize = 48_000;
    let (m_track, m_voice) = (build(ModScope::Track), build(ModScope::Voice));
    let mut p_track = XmrsPlayer::new(&m_track, 48000, 0);
    let mut p_voice = XmrsPlayer::new(&m_voice, 48000, 0);
    let h_track = hash_pcm(&render_pcm(&mut p_track, FRAMES));
    let h_voice = hash_pcm(&render_pcm(&mut p_voice, FRAMES));

    assert_ne!(
        h_track, h_voice,
        "ModScope::Voice must route the vibrato to the NNA-Continue ghost too, \
         so its render must differ from ModScope::Track (where the ghost stays \
         frozen). Identical hashes mean the Voice scope is dead config."
    );
}

/// RFC §2C — per-note expression (MPE) modulates a single voice
/// independently of the rest of the track. An instrument with
/// `NNA = Continue` plays C-4 carrying a per-note pitch bend, then a
/// second, **neutral** C-4 displaces it into a still-ringing ghost.
/// The bent voice keeps its own bend through the fork, so the render
/// with a non-zero ghost bend must differ from the all-neutral one —
/// even though no automation lane is involved and the *live* note's
/// expression is neutral in both. That is the per-voice independence
/// §2B alone can't express.
#[test]
fn note_expression_modulates_voice_independently() {
    use xmrs::core::cell::{Cell, CellEvent, NoteExpression};
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
    use xmrs::core::daw::track::Track;
    use xmrs::core::fixed::units::PitchDelta;
    use xmrs::core::instrument_behavior::{DuplicateCheckType, NewNoteAction};

    const ROWS: u32 = 24;

    fn build(first_note_bend: PitchDelta) -> Module {
        let data: Vec<i8> = (0..64)
            .map(|i| if i < 32 { 127i8 } else { -128i8 })
            .collect();
        let sample = Sample {
            name: "sq".into(),
            relative_pitch: 0,
            finetune: Finetune::default(),
            volume: ChannelVolume::FULL,
            default_note_volume: Volume::FULL,
            panning: Panning::CENTER,
            loop_flag: LoopType::Forward,
            loop_start: 0,
            loop_length: 64,
            sustain_loop_flag: LoopType::No,
            sustain_loop_start: 0,
            sustain_loop_length: 0,
            data: Some(SampleDataType::Mono8(data.into())),
        };
        let mut instr = InstrDefault::default();
        instr.sample.push(Some(sample));
        instr.keyboard.sample_for_pitch = [Some(0); 120];
        instr.behavior.duplicate_check = DuplicateCheckType::Off(NewNoteAction::Continue);

        let mut module = Module::default();
        module.instrument.push(Instrument {
            name: "sq".into(),
            instr_type: InstrumentType::Default(instr),
            muted: false,
        });

        let speed = module.default_tempo as u32;

        let mut rows = vec![Cell::default(); ROWS as usize];
        // First note carries the per-note bend — it becomes the ghost.
        rows[0].event = CellEvent::NoteOn {
            pitch: Pitch::C4,
            velocity: Volume::FULL,
        };
        rows[0].expression = NoteExpression {
            pitch_bend: first_note_bend,
            ..NoteExpression::NEUTRAL
        };
        // Second note is neutral and displaces the first into a ghost.
        rows[2].event = CellEvent::NoteOn {
            pitch: Pitch::C4,
            velocity: Volume::FULL,
        };
        module.tracks.push(Track::Notes {
            name: "n".into(),
            instrument: 0,
            rows,
            muted: false,
        });

        module.clips = SortedClips::from_unsorted(vec![Clip {
            track: 0,
            song: 0,
            target_channel: 0,
            position_tick: 0,
            speed_at_start: module.default_tempo as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: ROWS * speed,
        }]);
        let mut entries = Vec::new();
        for row in 0..ROWS {
            entries.push(TimelineEntry {
                song: 0,
                order_idx: 0,
                pattern_idx: 0,
                row_idx: row,
                loop_iter: 0,
                tick: row * speed,
                speed_at_row: speed as u8,
                bpm_at_row: module.default_bpm as u16,
            });
        }
        module.timeline_map = TimelineMap { entries };
        module
    }

    const FRAMES: usize = 48_000;
    let (m_neutral, m_bent) = (
        build(PitchDelta::ZERO),
        build(PitchDelta::from_semitones(7)),
    );
    let mut p_neutral = XmrsPlayer::new(&m_neutral, 48000, 0);
    let mut p_bent = XmrsPlayer::new(&m_bent, 48000, 0);
    let h_neutral = hash_pcm(&render_pcm(&mut p_neutral, FRAMES));
    let h_bent = hash_pcm(&render_pcm(&mut p_bent, FRAMES));

    assert_ne!(
        h_neutral, h_bent,
        "a per-note pitch bend must modulate its own voice (and follow it into \
         the NNA ghost), so a bent first note must render differently from a \
         neutral one — independently of the second, neutral voice"
    );
}

/// RFC §3B — the native per-channel insert chain actually processes
/// audio. The same Euclidean module is rendered twice: once with no
/// inserts (the universal tracker state, proven bit-identical by the
/// golden corpus) and once with a ~0.5 `Gain` insert on channel 0.
/// The inserted render must be audibly **attenuated** — roughly half
/// the energy — proving the chain is live, not dead data.
#[test]
fn channel_insert_gain_attenuates() {
    use xmrs::core::daw::device::{Device, DeviceChain};
    use xmrs::core::fixed::fixed::Q15;

    let module_plain = make_euclidian_module(Q15::ZERO, 0);
    let mut module_gain = make_euclidian_module(Q15::ZERO, 0);
    // ~0.5 linear gain insert on mixer channel 0.
    let half = Volume::from_q15(Q15::from_raw(16_384));
    module_gain.channel_inserts = vec![DeviceChain {
        devices: vec![Device::Gain(half)],
        bypass: false,
    }];

    const FRAMES: usize = 24_000;
    let mut p_plain = XmrsPlayer::new(&module_plain, 48000, 0);
    let mut p_gain = XmrsPlayer::new(&module_gain, 48000, 0);
    let pcm_plain = render_pcm(&mut p_plain, FRAMES);
    let pcm_gain = render_pcm(&mut p_gain, FRAMES);

    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();
    let e_plain = energy(&pcm_plain);
    let e_gain = energy(&pcm_gain);

    assert!(e_plain > 0, "the plain render must be audible");
    assert!(
        e_gain < e_plain,
        "a 0.5 Gain insert must attenuate channel 0: inserted energy {} must be \
         below plain {}",
        e_gain,
        e_plain
    );
    // Roughly half — generous bounds absorb saturation/quantisation.
    assert!(
        e_gain > e_plain / 4 && e_gain < e_plain * 3 / 4,
        "Gain(~0.5) energy {} should be about half of plain {}",
        e_gain,
        e_plain
    );
}

/// RFC §3A — a `DeviceParam` automation lane drives an insert
/// device's parameter end-to-end. The channel carries a `Gain` insert
/// initialised to **silent** (which would mute it), and a
/// `DeviceParam` Points lane that sets the gain param to full. With
/// the lane the channel is audible (automation restored the gain);
/// without it the static silent gain mutes the channel — proving the
/// lane actually reaches the device, not just that gains work.
#[test]
fn device_param_automation_drives_gain() {
    use xmrs::core::daw::automation::{
        AutomationLane, AutomationPoint, AutomationTarget, AutomationValue, LaneKind,
    };
    use xmrs::core::daw::device::{Device, DeviceChain};
    use xmrs::core::fixed::fixed::Q15;

    fn build(with_lane: bool) -> Module {
        let mut m = make_euclidian_module(Q15::ZERO, 0);
        // A Gain insert that starts silent — it mutes channel 0 unless
        // automation drives its gain param up.
        m.channel_inserts = vec![DeviceChain {
            devices: vec![Device::Gain(Volume::SILENT)],
            bypass: false,
        }];
        if with_lane {
            let mut lane = AutomationLane::new(AutomationTarget::DeviceParam {
                track: 0,
                device: 0,
                param: 0,
            });
            lane.kind = LaneKind::Points(vec![AutomationPoint {
                tick: 0,
                value: AutomationValue::Normalized(Q15::from_raw(32_767)),
            }]);
            m.automation.push(lane);
        }
        m
    }

    const FRAMES: usize = 24_000;
    let (m_lane, m_nolane) = (build(true), build(false));
    let mut p_lane = XmrsPlayer::new(&m_lane, 48000, 0);
    let mut p_nolane = XmrsPlayer::new(&m_nolane, 48000, 0);
    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();
    let e_lane = energy(&render_pcm(&mut p_lane, FRAMES));
    let e_nolane = energy(&render_pcm(&mut p_nolane, FRAMES));

    assert_eq!(
        e_nolane, 0,
        "a static Gain(SILENT) insert must mute channel 0 (control)"
    );
    assert!(
        e_lane > 0,
        "a DeviceParam lane must drive the gain param to full, restoring \
         audio the static silent gain would otherwise mute (energy {})",
        e_lane
    );
}

/// RFC §3B — a **stateful** insert device (the one-pole `LowPass`)
/// processes a channel through the full player path, its per-channel
/// state persisting across samples. The filtered render must differ
/// from the dry one and stay stable (no runaway above the input peak).
#[test]
fn lowpass_insert_filters_channel() {
    use xmrs::core::daw::device::{Device, DeviceChain};
    use xmrs::core::fixed::fixed::Q15;

    let dry = make_euclidian_module(Q15::ZERO, 0);
    let mut wet = make_euclidian_module(Q15::ZERO, 0);
    wet.channel_inserts = vec![DeviceChain {
        devices: vec![Device::LowPass {
            cutoff: Q15::from_raw(2_000),
            state: (0, 0),
        }],
        bypass: false,
    }];

    const FRAMES: usize = 24_000;
    let mut p_dry = XmrsPlayer::new(&dry, 48000, 0);
    let mut p_wet = XmrsPlayer::new(&wet, 48000, 0);
    let pcm_dry = render_pcm(&mut p_dry, FRAMES);
    let pcm_wet = render_pcm(&mut p_wet, FRAMES);

    assert_ne!(
        hash_pcm(&pcm_dry),
        hash_pcm(&pcm_wet),
        "a LowPass insert must change the channel output"
    );
    let peak = |p: &[i16]| p.iter().map(|s| (*s as i32).abs()).max().unwrap_or(0);
    assert!(
        peak(&pcm_wet) <= peak(&pcm_dry) + 1,
        "one-pole LPF must be stable: wet peak {} must not exceed dry {}",
        peak(&pcm_wet),
        peak(&pcm_dry)
    );
}

/// RFC §3B — the **master bus** insert chain processes the final
/// summed mix. A `Gain(~0.5)` master chain must attenuate the whole
/// output to roughly half; an empty master chain is bit-identical
/// (proven by the golden corpus).
#[test]
fn master_bus_attenuates_mix() {
    use xmrs::core::daw::device::{Device, DeviceChain};
    use xmrs::core::fixed::fixed::Q15;

    let dry = make_euclidian_module(Q15::ZERO, 0);
    let mut mastered = make_euclidian_module(Q15::ZERO, 0);
    let half = Volume::from_q15(Q15::from_raw(16_384));
    mastered.master_chain = DeviceChain {
        devices: vec![Device::Gain(half)],
        bypass: false,
    };

    const FRAMES: usize = 24_000;
    let mut p_dry = XmrsPlayer::new(&dry, 48000, 0);
    let mut p_master = XmrsPlayer::new(&mastered, 48000, 0);
    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();
    let e_dry = energy(&render_pcm(&mut p_dry, FRAMES));
    let e_master = energy(&render_pcm(&mut p_master, FRAMES));

    assert!(e_dry > 0);
    assert!(
        e_master < e_dry && e_master > e_dry / 4,
        "master Gain(~0.5) should roughly halve the mix: {} vs dry {}",
        e_master,
        e_dry
    );
}

/// RFC §3B — a channel **send** taps the channel's post-insert signal
/// into a return bus, whose processed output is summed back into the
/// master. With the send the output is louder (the parallel bus return
/// adds signal) than the same graph with no send — proving the
/// send→bus→master routing is live.
#[test]
fn send_routes_channel_to_return_bus() {
    use xmrs::core::daw::device::{Bus, Device, DeviceChain, Send};
    use xmrs::core::fixed::fixed::Q15;

    fn build(with_send: bool) -> Module {
        let mut m = make_euclidian_module(Q15::ZERO, 0);
        // Bring channel 0 down to ~0.4 so the dry+send sum stays below
        // full scale (otherwise a clipped square hides the difference).
        let low = Volume::from_q15(Q15::from_raw(13_000));
        m.channel_inserts = vec![DeviceChain {
            devices: vec![Device::Gain(low)],
            bypass: false,
        }];
        // One return bus with an active (unity) chain.
        m.buses = vec![Bus {
            name: "aux".into(),
            chain: DeviceChain {
                devices: vec![Device::Gain(Volume::FULL)],
                bypass: false,
            },
        }];
        if with_send {
            let half = Volume::from_q15(Q15::from_raw(16_384));
            m.channel_sends = vec![vec![Send {
                bus: 0,
                level: half,
            }]];
        }
        m
    }

    const FRAMES: usize = 24_000;
    let (m_off, m_on) = (build(false), build(true));
    let mut p_off = XmrsPlayer::new(&m_off, 48000, 0);
    let mut p_on = XmrsPlayer::new(&m_on, 48000, 0);
    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();
    let e_off = energy(&render_pcm(&mut p_off, FRAMES));
    let e_on = energy(&render_pcm(&mut p_on, FRAMES));

    assert!(e_off > 0);
    assert!(
        e_on > e_off,
        "a 0.5 send into a unity return bus must add signal to the mix: \
         with-send {} must exceed without-send {}",
        e_on,
        e_off
    );
}

/// RFC §3C — an audio clip on a `Track::Audio` actually plays through
/// the timeline, resampled from its source rate to the output rate.
/// The module has no instruments or note cells: all the sound comes
/// from the recorded PCM clip. A second render at half the source rate
/// must differ (resampling changes pitch/speed), proving the resampler
/// is live.
#[test]
fn audio_clip_plays_resampled() {
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::track::{AudioSource, Track};

    fn build(source_rate: u32) -> Module {
        let mut m = Module::default();
        m.tracks.push(Track::Audio {
            name: "rec".into(),
            muted: false,
            source: AudioSource::Inline(rec_square_sample()),
            source_rate,
        });
        let speed = fill_linear_timeline(&mut m, 16);
        m.clips = SortedClips::from_unsorted(vec![Clip {
            track: 0,
            song: 0,
            target_channel: 0,
            position_tick: 0,
            speed_at_start: speed as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: 16 * speed,
        }]);
        m
    }

    const FRAMES: usize = 24_000;
    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();

    // Source rate == output rate ⇒ step 1.0 (no resampling).
    let m_unity = build(48_000);
    let mut p_unity = XmrsPlayer::new(&m_unity, 48000, 0);
    let pcm_unity = render_pcm(&mut p_unity, FRAMES);
    assert!(
        energy(&pcm_unity) > 0,
        "an audio clip must produce sound with no instruments/notes at all"
    );

    // Half the source rate ⇒ plays an octave down / half speed ⇒ a
    // different render. Proves the linear resampler is engaged.
    let m_half = build(24_000);
    let mut p_half = XmrsPlayer::new(&m_half, 48000, 0);
    let pcm_half = render_pcm(&mut p_half, FRAMES);
    assert!(energy(&pcm_half) > 0);
    assert_ne!(
        hash_pcm(&pcm_unity),
        hash_pcm(&pcm_half),
        "resampling at half the source rate must change the rendered audio"
    );
}

/// RFC §3C (v2 routing) — an audio clip is injected **pre-insert** into
/// its target channel, so it flows through that channel's insert chain
/// just like the channel's notes. Putting a `Gain(~0.5)` insert on the
/// audio clip's channel must roughly halve its energy; v1 (clips summed
/// straight to master, bypassing inserts) would have left it unchanged.
#[test]
fn audio_clip_flows_through_channel_inserts() {
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::device::{Device, DeviceChain};
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::track::{AudioSource, Track};
    use xmrs::core::fixed::fixed::Q15;

    fn build(with_insert: bool) -> Module {
        let mut m = Module::default();
        m.tracks.push(Track::Audio {
            name: "rec".into(),
            muted: false,
            source: AudioSource::Inline(rec_square_sample()),
            source_rate: 48_000,
        });
        let speed = fill_linear_timeline(&mut m, 16);
        m.clips = SortedClips::from_unsorted(vec![Clip {
            track: 0,
            song: 0,
            target_channel: 0,
            position_tick: 0,
            speed_at_start: speed as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: 16 * speed,
        }]);

        if with_insert {
            let half = Volume::from_q15(Q15::from_raw(16_384));
            m.channel_inserts = vec![DeviceChain {
                devices: vec![Device::Gain(half)],
                bypass: false,
            }];
        }
        m
    }

    const FRAMES: usize = 24_000;
    let energy = |p: &[i16]| p.iter().map(|s| (*s as i64).abs()).sum::<i64>();
    let (m_dry, m_ins) = (build(false), build(true));
    let mut p_dry = XmrsPlayer::new(&m_dry, 48000, 0);
    let mut p_ins = XmrsPlayer::new(&m_ins, 48000, 0);
    let e_dry = energy(&render_pcm(&mut p_dry, FRAMES));
    let e_ins = energy(&render_pcm(&mut p_ins, FRAMES));

    assert!(e_dry > 0, "audio clip must produce sound");
    assert!(
        e_ins < e_dry && e_ins > e_dry / 4,
        "a Gain(~0.5) insert on the clip's channel must roughly halve it \
         (proving the clip now runs the channel strip, not just master): \
         {e_ins} vs dry {e_dry}"
    );
}

/// RFC §3C — a seek (`goto_tick`) must re-sync the audio-clip cursor.
/// The cursor free-runs with continuous playback, so a jump back to the
/// region start has to restart the clip from its head; otherwise the
/// stale cursor keeps reading from a desynced source position. We
/// render K frames inside the clip's first tick, `goto_tick(0)`, then
/// render K more: the two windows must be byte-identical (the clip
/// replays from the top). Without the `reset_for_goto` cursor reset the
/// second window would diverge.
#[test]
fn audio_clip_cursor_resyncs_on_seek() {
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::track::{AudioSource, Track};

    let mut m = Module::default();
    m.tracks.push(Track::Audio {
        name: "rec".into(),
        muted: false,
        source: AudioSource::Inline(rec_square_sample()),
        source_rate: 32_000, // ≠ output ⇒ exercises the resampler too
    });
    let speed = fill_linear_timeline(&mut m, 16);
    m.clips = SortedClips::from_unsorted(vec![Clip {
        track: 0,
        song: 0,
        target_channel: 0,
        position_tick: 0,
        speed_at_start: speed as u8,
        track_row_offset: 0,
        source_start_row: 0,
        end_tick: 16 * speed,
    }]);

    // K small enough to stay within the first tick (≈960 frames at the
    // default 125 BPM / 48 kHz), so the only moving part is the clip.
    const K: usize = 300;
    let mut p = XmrsPlayer::new(&m, 48000, 0);
    let before = render_pcm(&mut p, K);
    assert!(
        before.iter().any(|&s| s != 0),
        "clip must be audible before the seek"
    );
    assert!(p.goto_tick(0), "goto_tick(0) must land on the row-0 entry");
    let after = render_pcm(&mut p, K);

    assert_eq!(
        hash_pcm(&before),
        hash_pcm(&after),
        "after goto_tick(0) the audio clip must replay byte-identically \
         from its head — a stale (un-reset) cursor would desync it"
    );
}

/// RFC §4 — the asset pool is a **transparent** generalization: a
/// `Track::Audio` whose source is `Pooled(i)` (reading `Module.assets[i]`)
/// must render **bit-identically** to the same track with the waveform
/// owned `Inline`. This is the §0.3 equivalence default for the pool —
/// referencing changes storage, never the audio. The second clip shares
/// the *same* pooled asset, proving "stored once, played many" too.
#[test]
fn pooled_asset_renders_identically_to_inline() {
    use xmrs::core::daw::clip::Clip;
    use xmrs::core::daw::sorted_clips::SortedClips;
    use xmrs::core::daw::track::{AudioSource, Track};

    // Two audio tracks on two channels. `pooled` stores ONE asset and
    // points both tracks at it; `inline` gives each track its own copy.
    fn build(pooled: bool) -> Module {
        let mut m = Module::default();

        if pooled {
            let a = m.push_asset(rec_square_sample());
            for name in ["a", "b"] {
                m.tracks.push(Track::Audio {
                    name: name.into(),
                    muted: false,
                    source: AudioSource::Pooled(a),
                    source_rate: 32_000,
                });
            }
        } else {
            for name in ["a", "b"] {
                m.tracks.push(Track::Audio {
                    name: name.into(),
                    muted: false,
                    source: AudioSource::Inline(rec_square_sample()),
                    source_rate: 32_000,
                });
            }
        }

        let speed = fill_linear_timeline(&mut m, 16);
        let clip = |track: u32, ch: u8| Clip {
            track,
            song: 0,
            target_channel: ch,
            position_tick: 0,
            speed_at_start: speed as u8,
            track_row_offset: 0,
            source_start_row: 0,
            end_tick: 16 * speed,
        };
        m.clips = SortedClips::from_unsorted(vec![clip(0, 0), clip(1, 1)]);
        m
    }

    const FRAMES: usize = 24_000;
    let m_pool = build(true);
    let m_inline = build(false);
    assert_eq!(m_pool.assets.len(), 1, "the shared asset is stored once");
    assert!(m_inline.assets.is_empty(), "inline keeps an empty pool");

    let mut p_pool = XmrsPlayer::new(&m_pool, 48000, 0);
    let mut p_inline = XmrsPlayer::new(&m_inline, 48000, 0);
    let pcm_pool = render_pcm(&mut p_pool, FRAMES);
    let pcm_inline = render_pcm(&mut p_inline, FRAMES);

    assert!(pcm_pool.iter().any(|&s| s != 0), "pooled audio must sound");
    assert_eq!(
        hash_pcm(&pcm_pool),
        hash_pcm(&pcm_inline),
        "a Pooled asset reference must render bit-identically to the \
         equivalent Inline waveform (§4 transparency)"
    );
}

// =====================================================================
// Lane ↔ cell validators were removed alongside the migrated
// `TrackEffect` variants in DAW migration Phase 3c.3 — there is
// no cell-side source of truth left to dual-validate against.
// =====================================================================
