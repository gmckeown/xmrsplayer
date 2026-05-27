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
#[test]
fn sid_commando_produces_audio() {
    let sid = xmrs::tracker::import::sid::sid_module::SidModule::get_sid_commando();
    let modules = sid.to_modules(false);
    let module = &modules[0];
    let mut player = XmrsPlayer::new(module, 48000, 0);
    let pcm = render_pcm(&mut player, 4096);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "SID playback should produce audio");
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
        data: Some(SampleDataType::Mono8(data)),
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
        data: Some(SampleDataType::Mono8(data)),
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
        data: Some(SampleDataType::Mono8(data)),
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
    let modules = sid.to_modules(false);
    let module = &modules[0];
    let mut player = XmrsPlayer::new(module, 48000, 0);
    let pcm = render_pcm(&mut player, 48_000);
    assert_pcm_byte_perfect("sid_commando", &pcm);
}

// =====================================================================
// Lane ↔ cell validators were removed alongside the migrated
// `TrackEffect` variants in DAW migration Phase 3c.3 — there is
// no cell-side source of truth left to dual-validate against.
// =====================================================================
