//! Cycle-exact playback smoke tests. Importers produce a populated
//! DAW timeline; the player walks that timeline. There's a single
//! cell source now (post-3b), so the tests verify that the player
//! actually produces non-silent audio on representative bundled
//! demos rather than comparing two modes.

#![cfg(all(feature = "std", any(feature = "import_xm", feature = "import")))]
// These comparisons walk two register dumps at once — ours and the oracle's —
// so the loop index is a FRAME (or a voice), and the body reaches into several
// arrays at different offsets: `f[i]` against `f[i - 1]`, `ora[i]` against
// `syn[j]` where `j` carries a per-voice shift. `needless_range_loop` sees only
// the first array and proposes an iterator that cannot express the alignment,
// which is the whole point of these tests.
#![allow(clippy::needless_range_loop)]

use xmrs::prelude::*;
use xmrs::tracker::import::build::{Pattern, Row};
use xmrsplayer::xmrsplayer::XmrsPlayer;

/// Where a `sidplay --statedump` work-RAM dump is expected.
///
/// These dumps are megabytes of CSV produced by hand for one investigation;
/// they are not in the repository and never will be. So the diagnostics that
/// read them look under `tests/fixtures_sid_oracle/` — anchored on
/// `CARGO_MANIFEST_DIR`, like every other fixture in this file — and let `$var`
/// point elsewhere.
///
/// The rule this replaces: three absolute paths into a scratch directory,
/// complete with a session id. They resolved on exactly one machine, and
/// everywhere else the tests reported "skipping" — a diagnostic that never runs
/// is worse than one that fails, because it looks like it passed.
#[cfg(feature = "import_sid")]
fn ram_dump_path(var: &str, file: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| {
        let mut p = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        p.push("tests/fixtures_sid_oracle");
        p.push(file);
        p.to_string_lossy().into_owned()
    })
}

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

/// The LAST Rob Hubbard player generation, end to end: the bundled Lion_Heart
/// image is decoded straight into a module by `g2::to_module` — sixteen SID
/// instruments with their voice programs and swept filters, three free-running
/// voice lanes — and rendered through the cycle-accurate chip.
///
/// This is the first tune of that generation to play at all, so the bar here is
/// "it makes sound, deterministically", not register fidelity: the per-note
/// markers are not bound yet (see `ROBSID2_INSTRUMENT_MODEL.md`), so an oracle
/// comparison would be measuring a known-incomplete import.
#[cfg(feature = "import_sid")]
#[test]
fn g2_lion_heart_produces_audio() {
    let data = include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid");
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("Lion_Heart decodes");

    let mut player = XmrsPlayer::new(&module, 48000, 0);
    let pcm = render_pcm(&mut player, 48000);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(
        nonzero > 4800,
        "Lion_Heart should sound for most of a second, got {nonzero} non-zero samples"
    );

    let mut player2 = XmrsPlayer::new(&module, 48000, 0);
    assert_eq!(
        pcm,
        render_pcm(&mut player2, 48000),
        "the render must be deterministic"
    );
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

/// Opening a `.sid` the ordinary way must sound EXACTLY like the bundled
/// constructor it replaced.
///
/// This is the claim that let the player's dedicated `--sid` switch be deleted:
/// `Module::load` recognises the image, finds its transcribed record by
/// fingerprint and applies it, so nothing that used to require naming a tune is
/// lost by pointing at a file. Rendered PCM is the only honest way to assert
/// it — a structural comparison would pass while the tempo silently differed.
///
/// EVERY sub-song is compared, not just the first: sub-song N of the merged
/// module against module N of the old per-module list. Song 0 would pass by
/// construction (it keeps its own indices); the rebased ones are where a wrong
/// track or instrument offset would actually be heard.
#[cfg(feature = "import_sid")]
#[test]
fn a_sid_opened_as_a_file_sounds_like_the_bundled_tune() {
    use xmrs::tracker::import::sid::songs;

    for (name, image) in [
        ("commando", songs::COMMANDO),
        // Multi-sub-song, and a fractional cadence patched per sub-song: if the
        // merge dropped either, this is where it shows.
        ("delta", songs::DELTA),
        // The last generation — no record at all, everything from the image.
        ("lion_heart", songs::LION_HEART),
    ] {
        let by_file = Module::load(image).unwrap_or_else(|e| panic!("{name}: {e:?}"));

        let expected: Vec<Module> = match xmrs::tracker::import::sid::g2::to_module(image) {
            Some(m) => vec![m],
            None => xmrs::tracker::import::sid::sid_module::SidModule::for_image(image)
                .pop()
                .unwrap_or_else(|| panic!("{name}: no record"))
                .to_modules(),
        };

        for (song, want) in expected.iter().enumerate() {
            let mut a = XmrsPlayer::new(&by_file, 48000, song);
            let mut b = XmrsPlayer::new(want, 48000, 0);
            assert_eq!(
                render_pcm(&mut a, 48_000),
                render_pcm(&mut b, 48_000),
                "{name} sub-song {song}: opening the file does not sound like \
                 the bundled tune"
            );
        }
    }
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

/// Arbitrate the hand-written table against `SidLayout::detect`, per tune.
///
/// Detection disagrees with three of the fifteen transcribed records (Thing on a
/// Spring's instrument count and sound-effect address, Thrust's instrument
/// count, The Human Race's sub-song count). Ghidra already answers those — the
/// instructions that reference each table are in the image — but this renders
/// BOTH readings and scores each against the oracle, so the answer rests on a
/// measurement and not on a reading.
///
/// A caveat worth keeping in view: the oracle is `sidera`'s own `sidplay`, not a
/// historical one. That matters much less here than it would for a timbre
/// question — `--regdump` records the writes the **6502** put on the bus while
/// running the tune's own replayer, so it tests the CPU trace, not sidera's
/// analog model of the chip. Which bytes the replayer reads out of the note
/// stream is exactly a CPU-trace question.
///
/// Only the sub-songs the fixtures cover are scored; a detected layout that
/// finds MORE sub-songs (The Human Race) has its extra ones reported, not
/// judged, since no oracle capture exists for them.
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "expensive SID oracle gate — run with --ignored alongside sid_oracle_fidelity"]
fn sid_detected_layout_vs_hand_written() {
    use xmrs::tracker::import::sid::sid_module::SidModule;

    const SECS: usize = 16;
    const FREQ_HI_TOL: i32 = 8;

    type SidTune = (&'static str, fn() -> SidModule);
    let tunes: &[SidTune] = &[
        ("thing_on_a_spring", SidModule::get_sid_thing_on_a_spring),
        ("thrust", SidModule::get_sid_thrust),
        ("human_race", SidModule::get_sid_human_race),
        // Controls: three tunes where the two readings are identical. Their
        // counts MUST match exactly — if they move, the harness itself is
        // suspect and the three verdicts above mean nothing.
        ("commando", SidModule::get_sid_commando),
        ("last_v8", SidModule::get_sid_last_v8),
        ("spellbound", SidModule::get_sid_spellbound),
    ];

    let score = |module: &xmrs::prelude::Module, key: &str| -> Option<[u32; 4]> {
        let ora = load_oracle_csv_opt(key)?;
        xmrs::generators::sid::driver::capture_begin();
        let mut player = XmrsPlayer::new(module, 8000, 0);
        player.set_max_loop_count(1);
        for _ in player.take(SECS * 8000 * 2) {}
        let syn = xmrs::generators::sid::driver::capture_take();
        Some(aligned_voice_counts(&syn, &ora, FREQ_HI_TOL).0)
    };

    eprintln!(
        "\n{:24} {:>21}   {:>21}   verdict",
        "tune#song", "hand  gate ad sr fhi", "detect  gate ad sr fhi"
    );
    let mut verdicts: Vec<(String, &'static str)> = Vec::new();
    for (name, ctor) in tunes {
        let hand = ctor();
        let Some(auto) = hand.with_detected_layout() else {
            eprintln!("{name:24} not detected");
            continue;
        };
        // Guard against a silent no-op: a verdict of "identical" is only worth
        // anything once the two readings are known to differ.
        let (hi, ai) = (hand.instruments.len(), auto.instruments.len());
        let (hp, ap) = (hand.voices.phrases.len(), auto.voices.phrases.len());
        eprintln!(
            "{name:24} instruments {hi} -> {ai}, patterns {hp} -> {ap}, \
             sub-songs {} -> {}",
            hand.voices.songs.len(),
            auto.voices.songs.len()
        );

        let (hm, am) = (hand.to_modules(), auto.to_modules());
        if hm.len() != am.len() {
            eprintln!(
                "{name:24} sub-songs: hand {}, detected {} (extra ones have no oracle capture)",
                hm.len(),
                am.len()
            );
        }
        for i in 0..hm.len().max(am.len()) {
            let key = format!("{name}#{i}");
            let (h, a) = (
                hm.get(i).and_then(|m| score(m, &key)),
                am.get(i).and_then(|m| score(m, &key)),
            );
            let (Some(h), Some(a)) = (h, a) else {
                continue;
            };
            let (sh, sa): (u32, u32) = (h.iter().sum(), a.iter().sum());
            let verdict = match sa.cmp(&sh) {
                core::cmp::Ordering::Less => "DETECT closer to the chip",
                core::cmp::Ordering::Greater => "HAND closer to the chip",
                core::cmp::Ordering::Equal => "identical",
            };
            eprintln!(
                "{:24} {:>6} {:>3} {:>3} {:>4}   {:>8} {:>3} {:>3} {:>4}   {verdict}",
                key, h[0], h[1], h[2], h[3], a[0], a[1], a[2], a[3]
            );
            verdicts.push((key, verdict));
        }
    }

    // ---- the payload width, on its own ---------------------------------
    // The layout diff above turned out to be inert. The stream encoding is the
    // one disagreement that changes what the decoder READS, so score it
    // separately — and only it, with the tables and every behavioural setting
    // left as transcribed.
    eprintln!(
        "\n{:24} {:>21}   {:>21}   verdict   (payload width only)",
        "tune#song", "version  gate ad sr fhi", "binary  gate ad sr fhi"
    );
    for (name, ctor) in tunes {
        let hand = ctor();
        let Some(auto) = hand.with_detected_stream() else {
            continue;
        };
        // Does the width change what the decoder produces at all? If the row
        // grids come out identical the question is settled without the oracle:
        // this tune has no entry that takes the portamento branch, so the two
        // readings cannot be told apart by any measurement.
        let sg = hand.sid.short_gate_instrs();
        let differs = (0..hand.voices.songs.len())
            .any(|s| hand.voices.voice_passes(s, &sg) != auto.voices.voice_passes(s, &sg));
        eprintln!(
            "{name:24} {:<8} -> {:<14} decoded rows {}",
            format!("v{}", hand.sid.version_for_report()),
            format!("{:?}", auto.voices.stream.unwrap()),
            if differs { "DIFFER" } else { "identical" }
        );

        let (hm, am) = (hand.to_modules(), auto.to_modules());
        for i in 0..hm.len() {
            let key = format!("{name}#{i}");
            let (Some(h), Some(a)) = (score(&hm[i], &key), score(&am[i], &key)) else {
                continue;
            };
            let (sh, sa): (u32, u32) = (h.iter().sum(), a.iter().sum());
            eprintln!(
                "{:24} {:>6} {:>3} {:>3} {:>4}   {:>8} {:>3} {:>3} {:>4}   {}",
                key,
                h[0],
                h[1],
                h[2],
                h[3],
                a[0],
                a[1],
                a[2],
                a[3],
                match sa.cmp(&sh) {
                    core::cmp::Ordering::Less => "BINARY closer to the chip",
                    core::cmp::Ordering::Greater => "VERSION closer to the chip",
                    core::cmp::Ordering::Equal => "identical",
                }
            );
        }
    }

    // Every pair scores the same — including the three tunes where the two
    // readings genuinely differ. That is the finding, so pin it: the
    // disagreements are INERT, they touch only material no sub-song reaches
    // (Thing's 30 unselected instrument records, Thrust's 28th, The Human Race's
    // 21 unreferenced patterns and 2 uncaptured sub-songs). The oracle therefore
    // cannot arbitrate them, and Ghidra stays the only evidence.
    //
    // If this ever fails, something made the difference observable — which is
    // worth stopping for, in either direction.
    for (key, verdict) in &verdicts {
        assert_eq!(
            *verdict, "identical",
            "{key}: the hand-written and detected layouts used to be \
             indistinguishable against the oracle; they no longer are"
        );
    }
}

/// The LAST player generation against the real chip.
///
/// Same instrument as the v10–v30 gate — our per-frame virtual-chip registers
/// versus a `sidplay --regdump` capture of the same tune, time-aligned per voice
/// — but pointed at `g2::to_module`. It exists to make the remaining G2 work
/// MEASURABLE: the import is knowingly incomplete (the per-note markers are not
/// bound, the tempo's twelve-step frame steal is not modelled), so the counters
/// here are a starting line, not a target. Freeze them, then watch them fall.
///
/// Re-capture with:
///   sidplay --seconds 16 --regdump lion_heart.csv Lion_Heart.sid
///   gzip -9 -c lion_heart.csv > tests/fixtures_sid_oracle/lion_heart_g2#0.csv.gz
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "expensive SID oracle gate — run with --ignored alongside sid_oracle_fidelity"]
fn g2_oracle_fidelity_golden() {
    // 90 s, not the 16 s of the v10–v30 gate: those tunes are short and dense,
    // Lion_Heart is long. At 16 s the window covered 3 % of its arpeggios — an
    // effect could be right or absent and the counters would not move.
    const SECS: usize = 90;
    const FREQ_HI_TOL: i32 = 8;

    // TWO tunes, deliberately: Lion_Heart uses neither the per-note cutoff nor
    // the pulse-width hold, so on its own it leaves `ArmMode::At` and the PW
    // hold unproven against the chip. Sun_Never_Shines is the opposite — 562
    // cutoffs, 458 PW holds, and 64 % of its notes marked against Lion_Heart's
    // 12 % — so between them every per-note marker the importer emits is heard
    // by an oracle.
    type G2Tune = (&'static str, &'static [u8], [u32; 3], [u32; 6]);
    let tunes: &[G2Tune] = &[
        (
            "lion_heart_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid"),
            [520, 244, 165],
            // `gate` reached ZERO here once the program's control writes went
            // through the end-of-note mask (`$17bd`) like the replayer's: the
            // settled tail after the `$85` terminator was re-opening the gate of
            // a note whose last row had gone by, every frame.
            [0, 102, 226, 29, 2317, 8],
        ),
        (
            "sun_never_shines_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid"),
            SNS_ON,
            SNS_REG,
        ),
        // A THIRD tune, added the day the other two stopped being enough. Same
        // replayer build (load `$1003`, instruments `$1b4b`) but a different
        // dialect of it: 1690 hold-filter markers against Lion_Heart's handful,
        // and a `0x89` filter-voice marker neither reference tune carries.
        //
        // It landed here on its first render, with no tuning: zero gate
        // divergences, `fhi` 68 and `freq` 631 against Lion_Heart's 222 and
        // 2757. That is the generalisation test for the whole session's work,
        // and it is the reason to keep it frozen.
        //
        // The trigger counts sit one below the chip's on each voice — the last
        // entry of the render, not a dropped note (`set_max_loop_count(1)` ends
        // the render where the song loops).
        (
            "pacific_coast_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/pacific_coast.sid"),
            [729, 335, 337],
            [0, 32, 32, 0, 476, 0],
        ),
        // And the last two G2 tunes the corpus holds, so the generation is
        // covered end to end rather than by its three best-behaved members.
        // Go_Go_Dash is the only one that does NOT load at `$1003` — its image
        // sits at `$1000` — which is worth having under an oracle on its own,
        // since every table address the importer recovers is relative to it.
        (
            "radio_ace_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/radio_ace.sid"),
            RADIO_ACE_ON,
            RADIO_ACE_REG,
        ),
        (
            "go_go_dash_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/go_go_dash.sid"),
            GO_GO_DASH_ON,
            GO_GO_DASH_REG,
        ),
        // The sixth and last G2 tune the corpus holds. With it the generation is
        // covered ENTIRELY — no member of it is left unmeasured, so a change
        // that helps one tune and quietly breaks another has nowhere to hide.
        // It is also the only one whose instrument bank does not start at
        // `$1b4b` (`$1b73`, orders at `$1b6b`), which is the last structural
        // assumption the other five leave untested.
        (
            "lakers_vs_celtics_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/lakers_vs_celtics.sid"),
            LAKERS_ON,
            LAKERS_REG,
        ),
        (
            // The EARLIER three-marker player, and the first of its family to be
            // read at all.
            //
            // First contact with the chip, and the note side landed: the tune
            // triggers [275, 450, 405] against the chip's [276, 451, 405] — one
            // note out on two voices — with `gate`, `ad` and `sr` all at ZERO.
            // The order lists, the row cadence, the entry walk, the `$83` length
            // and the instrument envelopes are therefore all right.
            //
            // The waveform then followed: flags bit 2 is the two-phase attack,
            // control byte at `+9` and frame count at `+11` exactly as in the
            // later player, and wiring it took the waveform counter 1121 → 0.
            //
            // What is left is the PITCH, and only the pitch: `fhi` 1576 and
            // `freq` 7136 — of which 705 frames are AUDIBLE, the rest falling on
            // voices the chip has gated off. Voice 2 alone accounts for 548 of
            // the inaudible ones. `g2_fhi_breakdown` prints the split, and it is
            // worth reading before this counter is taken at face value — the
            // same measurement puts Lion Heart's 29 wrong frames at 6 audible
            // and Radio ACE's 72 at 2.
            //
            // `g2_fhi_breakdown` also says it is two things, not one. Voices 0 and 1 diverge in 522 and 450 runs of ONE frame,
            // which is the alternating-effect phase artefact `ACCEPTED.md`
            // describes rather than wrong values. Voice 2 is different and real:
            // the chip ramps a frequency by a constant 0x0A0F up and back down
            // (`ecf7 f706 0115 0b24 1533 1f42 2951 …`) where we write nothing at
            // all — though with `ctrl` at 00 on both sides, so it is inaudible.
            //
            // NOT the voice program: that format turns out to be identical to
            // the later player's (2-byte `ctrl, freq_hi` steps, 3-byte
            // `ctrl, lo, hi` bends that SUBTRACT, `$85` terminator — Ghidra
            // `$137f`/`$13a6`), and it decodes correctly; instrument 1's bends
            // are 448, 320, 128, 256, 384, 512, nothing like 0x0A0F. A constant
            // step reversing at each end is a VIBRATO, which puts this back on
            // the `+5` default — whose model, not whose existence, is what has
            // yet to be got right.
            //
            // Four further explanations have been tried and measured away —
            // removing the transpose makes it worse (so the transpose is right),
            // transposing programmed instruments changes nothing, no note comes
            // near overflowing the frequency table (max index 76 of 96), and the
            // two frequency tables are the same scale to 0.5 %. The instrument's
            // `+5` default vibrato is real in Ghidra but applying it costs more
            // than it saves — see the note in `g2.rs`.
            "off_the_cuff_3m#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/off_the_cuff.sid"),
            [275, 450, 405],
            [0, 0, 0, 1576, 7136, 0],
        ),
    ];
    for (key, data, base_on, base_reg) in tunes {
        run_g2_oracle(key, data, *base_on, *base_reg, SECS, FREQ_HI_TOL);
    }
}

/// Frozen for Sun_Never_Shines the first time it was put under the oracle — and
/// it landed there: ZERO gate divergences over 13 500 voice-frames, and one
/// trigger off the chip on two voices. That is the measurement that proves the
/// per-note cutoff (`ArmMode::At`, 562 of them here) and the pulse-width hold
/// (458) are right, which Lion_Heart could never show since it uses neither.
/// The `ad`/`sr` pair sits at 1676 apiece — identical, so likely one systematic
/// envelope effect rather than scattered noise. Not yet probed.
///
/// Re-frozen when the two-phase attack stopped sounding on the trigger frame
/// (`fhi` 2794→1386, `freq` 4987→3579). That change took Lion_Heart's `fhi`
/// 1467→728 and `freq` 4678→3939, and on the historical corpus drove three
/// counters to EXACTLY zero (delta#11 gate 158→0, delta#12 fhi 68→0,
/// lightforce#0 fhi 173→0) — see `coupled.rs::note_on`.
///
/// Re-frozen again when the per-note arpeggio reached the chip at all
/// (`TrackEffect::NoteArming::arpeggio`): `fhi` 1386→82, `freq` 3579→2086 here,
/// 728→596 / 3939→3657 on Lion_Heart. Before it, every arpeggio the importer
/// emitted was dropped — the cell effect rotates on row ticks and dies at the
/// row boundary, while the replayer steps a per-voice slot every frame for the
/// whole note. The historical corpus did not move at all.
const SNS_ON: [u32; 3] = [457, 72, 430];
/// Re-frozen once the voice program stopped inventing pitch it was never
/// allowed to write. Two exits of the replayer's wave engine were modelled as
/// one: a `Fixed` step writes `$d401` ALONE (`$16cb`, then `JMP $17cf` — the
/// frame's `$d400` write is jumped over), and the `$85` terminator hands the
/// pitch back to the played note instead of pinning the last step's forever
/// (`$16bc`, `JMP $17b7`). Together: Lion_Heart `fhi` 597→368 and `freq`
/// 3517→3244, here `fhi` 82→74 and `freq` 2097→1872 — and, measured directly
/// against the replayer's work RAM, the unmodulated frames that no verified
/// modulator could explain went from 215/3466 to 5 on voice 0 and from 63/519
/// to 0 on voice 1.
///
/// Re-frozen again on the chain that fell out of the first fix. With the
/// program no longer masking anything, the REST entries turned out to be
/// missing fetches: a `0x60` entry is fetched (`$129d`) and jumps to the
/// register write, skipping the effect chain, so every per-frame counter
/// freezes — and we were making ZERO of those (11 / 64 / 49 on Lion_Heart's
/// three voices). Encoding a rest as a note-off cell WITH the instrument column
/// (the import's existing note-fetch shape) took fetch coverage to 484/484,
/// 344/344, 235/235, every one on the replayer's exact frame. That in turn
/// earned back the two changes previously withheld as "more faithful but worse"
/// — the replayer's own arpeggio phase counter (RAM agreement 82 % → 94 %) and
/// its step-then-emit ping-pong order — which had only ever been judged against
/// the two defects since fixed. The three together: Lion_Heart `fhi` 368→237
/// and `freq` 3244→3239, here `fhi` 74→14 and `freq` 1872→1818. The v10–v30
/// corpus does not move (all of it is gated on `interp_flat` / the G2 import).
///
/// Re-frozen a third time on the vibrato's own three defects, all found by
/// putting the ping-pong position (`$1003,X`) beside the replayer's, which had
/// never been done: the ADVANCE decision already agreed on 96 % of frames while
/// the POSITION agreed on 5 %, i.e. the law was right and the phase was not.
///  * the constructor seeded Thrust's converged phase [3,3,1] on every tune —
///    Ghidra-derived, but for `$0cce`, a different generation; the last player's
///    `$1003,X` ships 0 rising;
///  * the start-delay guard used `>=` where its own comment said `>` (a delay of
///    D must skip frames 1..=D);
///  * a SILENT voice keeps ping-ponging on the chip — the voice loop has no
///    "is it sounding" test — where ours froze on 535 of Lion_Heart voice 2's
///    frames.
///
/// Position agreement 2319 → 5580 frames of 13 500, Lion_Heart `freq`
/// 3239→2825; `freq` here goes the other way (1818→1966) while `fhi` holds at
/// 14. Judged on Lion_Heart because that is the tune with the work-RAM oracle.
///
/// Re-frozen once more for `lh_sounding_note`. The replayer adds the arpeggio's
/// offset to the struck note FIRST (`$1449` → `$1a52,X`) and the vibrato then
/// reads that note at `$14c3` to pick both its centre (`$14fa`) and its step
/// (`$14de`) — so on an arpeggiating note the swing rides the arpeggio, note by
/// note. This engine had the swing built on the struck note and then erased by
/// the arpeggio stage. The number that shows it is Lion_Heart voice 1 — the one
/// voice that arpeggiates — whose frequency was wrong on 24 % of the frames
/// where its ping-pong counter AGREED with the replayer's, and is now wrong on
/// 8 %. Lion_Heart `fhi` 235→222 and `freq` 2825→2757; here `freq` moves by one
/// frame.
const SNS_REG: [u32; 6] = [0, 106, 106, 14, 1129, 0];

/// Both frozen from their FIRST render, with nothing tuned for them — which is
/// the whole point of adding them. Zero gate divergences apiece, and `fhi` 72
/// and 15 against Lion_Heart's 222; Go_Go_Dash lands second-best of the five
/// despite being the only tune whose image loads at `$1000` instead of `$1003`,
/// so the table addresses the importer recovers are right independently of the
/// load address. Trigger counts sit one under the chip's on the voices that are
/// still sounding when the render stops — the same end-of-render effect as the
/// other three, not a dropped note.
const RADIO_ACE_ON: [u32; 3] = [243, 118, 713];
const RADIO_ACE_REG: [u32; 6] = [0, 74, 74, 72, 1334, 0];
const GO_GO_DASH_ON: [u32; 3] = [449, 599, 266];
const GO_GO_DASH_REG: [u32; 6] = [0, 154, 154, 15, 772, 0];
/// This tune was the outlier that paid for itself: frozen at `fhi` 893 against
/// the other five's 14-222, it was the one that exposed the SIGNED track
/// transpose (see `voice_rows`). Its divergences were two 190-frame runs of
/// `$ffff` — a note pinned at 95, the last table entry, which every G2 tune
/// stores as `$ffff`. Fixing the sign took it to 54, and took Lion_Heart —
/// ground over for a whole session — from 222 to 30.
const LAKERS_ON: [u32; 3] = [289, 309, 114];
const LAKERS_REG: [u32; 6] = [0, 130, 130, 55, 484, 5];

#[cfg(feature = "import_sid")]
fn run_g2_oracle(
    key: &str,
    data: &[u8],
    base_on: [u32; 3],
    base_reg: [u32; 6],
    secs: usize,
    freq_hi_tol: i32,
) {
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");

    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(secs * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ora = load_oracle_csv(key);

    let (counts, shifts, saturated) = aligned_voice_counts(&syn, &ora, freq_hi_tol);
    let freq = full_freq_divergence(&syn, &ora, shifts);
    // The WHOLE control byte, not just its gate bit. The shared counter above
    // compares `& 1`, so for an entire session a two-phase instrument opening
    // every note on noise where we held a pulse — 3898 frames across the six
    // tunes, and plainly audible — moved none of these numbers at all. A gate
    // that cannot see the waveform is a gate with a hole in it.
    let ctrl: u32 = (0..3)
        .map(|v| {
            let c = 4 + v * 7;
            (0..ora.len())
                .filter(|&i| {
                    let j =
                        (i as f64 * syn.len() as f64 / ora.len() as f64).round() as i32 + shifts[v];
                    j >= 0 && (j as usize) < syn.len() && syn[j as usize][c] != ora[i][c]
                })
                .count() as u32
        })
        .sum();
    eprintln!(
        "\nG2 {key}  frames syn={} ora={}  shift {:?}  gate={} ad={} sr={} fhi={} freq={}{}",
        syn.len(),
        ora.len(),
        shifts,
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        freq,
        if saturated { "  drift>window" } else { "" }
    );
    // NB the frame counts say nothing about the tempo — only about how long
    // each side was rendered (an earlier version of this test claimed
    // otherwise). Ours can also stop short: `set_max_loop_count(1)` ends the
    // render when the song loops, and closing the gate at the end of every note
    // changed where that is detected.
    assert!(
        syn.len() > ora.len() / 2,
        "the render collapsed: {} frames against the oracle's {}",
        syn.len(),
        ora.len()
    );

    // The starting line, measured 2026-07-25 over 90 s with the per-note markers
    // still unbound. DOWN is progress, UP is a regression; re-freeze only in the
    // same commit as the change that earned it.
    //
    // ⚠️ Read these with care until `saturated` goes away. The per-voice
    // alignment fits ONE shift over the whole capture; something in the import
    // still drifts progressively, so by the end of the window the two sides are
    // out of phase and every counter saturates. Until that is found, the numbers
    // measure "how early the drift starts" as much as "how wrong the registers
    // are" — which is why the Ghidra-exact tempo simulation moved `ad`/`sr` down
    // and `gate` slightly up instead of collapsing everything.
    // How many notes each side actually triggers. This is the number to watch:
    // it survives the phase drift that saturates the register counters, and it
    // is what exposed the missing end-of-note gate-off (voice 0 was firing 169
    // notes against the chip's 521; closing the gate on each note's last row
    // brought it to 564).
    let syn_on = note_on_counts(&syn);
    let ora_on = note_on_counts(&ora);
    eprintln!("  note-ons syn={syn_on:?} ora={ora_on:?}");

    assert_eq!(
        syn_on, base_on,
        "{key}: trigger counts moved (chip plays {ora_on:?})"
    );

    // The per-frame register counters. ⚠️ They saturate: the per-voice alignment
    // fits ONE shift over the whole 90 s, so once the two sides slip out of
    // phase every counter pins high and small real improvements can read as
    // small regressions. Frozen to catch gross change only — trust the trigger
    // counts above for progress.
    eprintln!("  waveform (whole $d404) divergences: {ctrl}");
    let got = [counts[0], counts[1], counts[2], counts[3], freq, ctrl];
    assert_eq!(
        got, base_reg,
        "{key}: divergence vs the real chip moved: {got:?} against the frozen {base_reg:?}"
    );
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
/// Voice-frames whose FULL 16-bit frequency register differs by more than a
/// quarter-tone, using the per-voice shifts the coarse comparison already found.
///
/// The four counters of `aligned_voice_counts` are `gate`, `ad`, `sr` and the
/// frequency HIGH byte with a tolerance of 8 — deliberately coarse, to survive
/// the capture jitter the v10–v30 corpus lives with. But that makes them
/// **blind to pitch detail**: a four-semitone arpeggio on a low note moves the
/// high byte by less than 8 and vanishes under the tolerance. Anything that
/// modulates pitch — arpeggio, portamento, vibrato — is therefore invisible to
/// them, which is exactly the G2 work still to come. This looks at both bytes,
/// with a RELATIVE tolerance so it tracks musical distance rather than register
/// distance (a quarter-tone is ~1.5 %).
/// Gate rises (0→1) per voice — how many notes each side actually TRIGGERS.
///
/// This turned out to be far more diagnostic than the per-frame register
/// counters: it is insensitive to the phase drift that saturates them, and it
/// caught the defect they could not see (the import was triggering a third of
/// the notes the chip does, because the end-of-note gate-off was missing).
fn note_on_counts(f: &[[u8; 25]]) -> [u32; 3] {
    let mut out = [0u32; 3];
    for v in 0..3 {
        let c = 4 + v * 7;
        for i in 1..f.len() {
            if f[i][c] & 1 == 1 && f[i - 1][c] & 1 == 0 {
                out[v] += 1;
            }
        }
    }
    out
}

fn full_freq_divergence(syn: &[[u8; 25]], ora: &[[u8; 25]], shifts: [i32; 3]) -> u32 {
    let (o, s) = (ora.len(), syn.len());
    if o == 0 || s == 0 {
        return 0;
    }
    let scale = s as f64 / o as f64;
    let mut bad = 0u32;
    for v in 0..3 {
        let (lo, hi) = (v * 7, 1 + v * 7);
        for i in 0..o {
            let j = (i as f64 * scale).round() as i32 + shifts[v];
            if j < 0 || j as usize >= s {
                continue;
            }
            let j = j as usize;
            let a = u16::from_le_bytes([syn[j][lo], syn[j][hi]]) as f64;
            let b = u16::from_le_bytes([ora[i][lo], ora[i][hi]]) as f64;
            // Both silent (or both parked at zero) is not a divergence.
            if a < 1.0 && b < 1.0 {
                continue;
            }
            let d = (a - b).abs() / b.max(1.0);
            if d > 0.015 {
                bad += 1;
            }
        }
    }
    bad
}

fn load_oracle_csv(name: &str) -> Vec<[u8; 25]> {
    load_oracle_csv_opt(name).unwrap_or_else(|| panic!("missing oracle fixture {name}.csv.gz"))
}

/// As [`load_oracle_csv`], but `None` when no capture exists — a sub-song the
/// oracle was never run on can be reported rather than crashing the run.
#[allow(dead_code)]
fn load_oracle_csv_opt(name: &str) -> Option<Vec<[u8; 25]>> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    use std::path::PathBuf;
    let mut path = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    path.push("tests/fixtures_sid_oracle");
    path.push(format!("{name}.csv.gz"));
    let bytes = std::fs::read(&path).ok()?;
    let mut text = String::new();
    GzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap_or_else(|e| panic!("corrupt gzip oracle fixture {} ({e})", path.display()));
    Some(
        text.lines()
            .skip(1)
            .map(|l| {
                let mut a = [0u8; 25];
                for (i, x) in l.split(',').skip(1).enumerate().take(25) {
                    a[i] = u8::from_str_radix(x.trim(), 16).unwrap_or(0);
                }
                a
            })
            .collect(),
    )
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

// =====================================================================
// TEMPORARY DIAGNOSTIC — delete once the freq residual is explained.
// =====================================================================

/// Where do the ~4700 divergent frequency frames actually live?
///
/// Four axes, because each one rules out a different family of cause:
///   * per voice      — one broken lane vs a global effect
///   * over time      — progressive (drift) vs uniform (modelling)
///   * since note-on  — at the attack (wavetable / arpeggio phase) vs sustained
///   * in SEMITONES   — a musical interval (wrong note, octave, arp step) vs
///     cents (vibrato depth, portamento rate)
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_freq_residual_probe() {
    const SECS: usize = 90;
    let tunes: [(&str, &[u8]); 2] = [
        (
            "lion_heart_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid"),
        ),
        (
            "sun_never_shines_g2#0",
            include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid"),
        ),
    ];
    for (key, data) in tunes {
        let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
        xmrs::generators::sid::driver::capture_begin();
        let mut player = XmrsPlayer::new(&module, 8000, 0);
        player.set_max_loop_count(1);
        for _ in player.take(SECS * 8000 * 2) {}
        let syn = xmrs::generators::sid::driver::capture_take();
        let ora = load_oracle_csv(key);
        let (_, shifts, _) = aligned_voice_counts(&syn, &ora, 8);
        let (o, s) = (ora.len(), syn.len());
        let scale = s as f64 / o as f64;

        // Asked and answered: the fitted register shift of -2 is REAL, not a fit
        // absorbing a modulator phase error. Scanning it gives a sharp minimum
        // on both tunes — Lion_Heart 6531 / **3517** / 6216 / 6972 and
        // Sun_Never_Shines 6908 / **2097** / 7115 / 6781 for shifts -3/-2/-1/0.
        // So registers at -2 and internal state at -1 are both correct, and the
        // one-frame gap between them is structural (we sample the state before
        // `modulate` steps it; the RAM dump is taken after).
        eprintln!("\n=== {key}  syn={s} ora={o} shift={shifts:?} ===");
        for v in 0..3 {
            let (lo, hi, ctrl) = (v * 7, 1 + v * 7, 4 + v * 7);
            let mut compared = 0u32;
            let mut bad = 0u32;
            let mut decile = [0u32; 10];
            let mut since = [0u32; 7]; // 0, 1, 2-3, 4-7, 8-15, 16-31, 32+
            let mut semis: std::collections::BTreeMap<i32, u32> = std::collections::BTreeMap::new();
            let mut higher = 0u32;
            let mut zero_syn = 0u32;
            let mut zero_ora = 0u32;
            let mut zero_gatediff = 0u32;
            let mut gate_diff = 0u32;
            let mut age = 0usize;
            for i in 0..o {
                // Age measured on the ORACLE, the side we trust.
                if i > 0 && ora[i][ctrl] & 1 == 1 && ora[i - 1][ctrl] & 1 == 0 {
                    age = 0;
                } else {
                    age += 1;
                }
                let j = (i as f64 * scale).round() as i32 + shifts[v];
                if j < 0 || j as usize >= s {
                    continue;
                }
                let j = j as usize;
                let a = u16::from_le_bytes([syn[j][lo], syn[j][hi]]) as f64;
                let b = u16::from_le_bytes([ora[i][lo], ora[i][hi]]) as f64;
                if a < 1.0 && b < 1.0 {
                    continue;
                }
                compared += 1;
                let d = (a - b).abs() / b.max(1.0);
                if d <= 0.015 {
                    continue;
                }
                bad += 1;
                if a > b {
                    higher += 1;
                }
                // A register parked at zero is SILENCE, not a pitch. Counting it
                // as a ratio pins the histogram at the clamp and hides the real
                // intervals underneath — which is what the first pass did.
                let gs = syn[j][ctrl] & 1;
                let go = ora[i][ctrl] & 1;
                if a < 16.0 || b < 16.0 {
                    if a < 16.0 {
                        zero_syn += 1;
                    } else {
                        zero_ora += 1;
                    }
                    if gs != go {
                        zero_gatediff += 1;
                    }
                    continue;
                }
                if gs != go {
                    gate_diff += 1;
                }
                decile[(i * 10 / o).min(9)] += 1;
                since[match age {
                    0 => 0,
                    1 => 1,
                    2..=3 => 2,
                    4..=7 => 3,
                    8..=15 => 4,
                    16..=31 => 5,
                    _ => 6,
                }] += 1;
                // Musical distance: 12·log2(a/b), rounded, clamped for display.
                let st = if a >= 1.0 && b >= 1.0 {
                    (12.0 * (a / b).log2()).round() as i32
                } else {
                    99
                };
                *semis.entry(st.clamp(-25, 25)).or_default() += 1;
            }
            eprintln!(
                "voice {v}: compared={compared} bad={bad} ({:.0}%)  syn-higher={:.0}%",
                100.0 * bad as f64 / compared.max(1) as f64,
                100.0 * higher as f64 / bad.max(1) as f64
            );
            eprintln!(
                "  parked-at-0: ours={zero_syn} chip={zero_ora} (gate differs on {zero_gatediff})\
                 |  pitched-but-gate-differs={gate_diff}"
            );
            eprintln!("  by decile      {decile:?}");
            eprintln!(
                "  since note-on  0={} 1={} 2-3={} 4-7={} 8-15={} 16-31={} 32+={}",
                since[0], since[1], since[2], since[3], since[4], since[5], since[6]
            );
            let mut top: Vec<(i32, u32)> = semis.into_iter().collect();
            top.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
            let shown: Vec<String> = top
                .iter()
                .take(10)
                .map(|(st, n)| format!("{st:+}st×{n}"))
                .collect();
            eprintln!("  semitone error {}", shown.join(" "));

            // WHERE do the two sides trigger, exactly? A one-frame note offset
            // and a one-frame effect-chain offset produce an identical trace;
            // only pairing the gate rises in order tells them apart.
            let rises = |f: &[[u8; 25]]| -> Vec<usize> {
                (1..f.len())
                    .filter(|&i| f[i][ctrl] & 1 == 1 && f[i - 1][ctrl] & 1 == 0)
                    .collect()
            };
            let (ro, rs) = (rises(&ora[..]), rises(&syn[..]));
            // Cross-correlate the two trigger trains instead of pairing them in
            // order: a single missing note shifts every later pair by a whole
            // note length, which reads as a huge fake offset.
            let mapped: std::collections::BTreeSet<i64> = rs
                .iter()
                .map(|&j| ((j as f64 - shifts[v] as f64) / scale).round() as i64)
                .collect();
            let mut corr: Vec<(i32, usize)> = (-8..=8)
                .map(|d| {
                    (
                        d,
                        ro.iter()
                            .filter(|&&i| mapped.contains(&(i as i64 + d as i64)))
                            .count(),
                    )
                })
                .collect();
            corr.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
            let cs: Vec<String> = corr
                .iter()
                .take(4)
                .map(|(d, n)| format!("{d:+}\u{2192}{n}"))
                .collect();
            eprintln!(
                "  trigger train: chip={} ours={}  best align {}",
                ro.len(),
                rs.len(),
                cs.join(" ")
            );

            // Frame-by-frame around the first few attacks: a TIME shift and a
            // VALUE error look identical in the histogram above and completely
            // different here.
            let mut shown_notes = 0;
            for i in 1..o {
                if shown_notes >= 3 {
                    break;
                }
                if !(ora[i][ctrl] & 1 == 1 && ora[i - 1][ctrl] & 1 == 0) {
                    continue;
                }
                shown_notes += 1;
                let mut line = format!("note@{i:5} ours/chip:");
                for k in 0..12 {
                    let ii = i + k;
                    if ii >= o {
                        break;
                    }
                    let j = (ii as f64 * scale).round() as i32 + shifts[v];
                    if j < 0 || j as usize >= s {
                        break;
                    }
                    let j = j as usize;
                    let a = u16::from_le_bytes([syn[j][lo], syn[j][hi]]);
                    let b = u16::from_le_bytes([ora[ii][lo], ora[ii][hi]]);
                    line.push_str(&format!(" {a:5}/{b:5}"));
                }
                eprintln!("  {line}");
            }
        }
    }
}

/// Do the G2 cells actually CARRY an arpeggio? (temporary diagnostic)
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_arpeggio_reaches_the_cells() {
    use xmrs::prelude::*;
    for (key, data) in [
        (
            "lion_heart",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..],
        ),
        (
            "sun_never_shines",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid")[..],
        ),
    ] {
        let m = xmrs::tracker::import::sid::g2::to_module(data).unwrap();
        let mut n = 0usize;
        let mut zero_low = 0usize;
        let mut sample: Vec<(usize, usize)> = Vec::new();
        for t in &m.tracks {
            if let Track::Notes { rows, .. } = t {
                for c in rows {
                    for e in &c.effects {
                        if let TrackEffect::Arpeggio { half1, half2 } = e {
                            n += 1;
                            if *half2 == 0 {
                                zero_low += 1;
                            }
                            if sample.len() < 8 {
                                sample.push((*half1, *half2));
                            }
                        }
                    }
                }
            }
        }
        eprintln!(
            "{key}: {n} Arpeggio effects on cells ({zero_low} with a zero low nibble) {sample:?}"
        );
        // How many TICKS does a row last? The tracker arpeggio only advances on
        // ticks > 0, so a one-tick row can never rotate it off the base note.
        let mut speeds: std::collections::BTreeMap<u8, usize> = std::collections::BTreeMap::new();
        for e in &m.timeline_map.entries {
            *speeds.entry(e.speed_at_row).or_default() += 1;
        }
        eprintln!("  ticks per row: {speeds:?}");
    }
}

/// Where our importer places each pattern-entry boundary, against the frames the
/// replayer actually re-fetches on (`lh_rows_left` at `$1a37,X` going negative).
/// Chip reference for Lion_Heart voice 2, from
/// `sidplay --statedump --stateaddrs 1a37`:
///   235 fetches, first ones at 2 26 34 61 69 87 130 154 162 189 197 205 210 221
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_entry_boundaries_vs_replayer() {
    use xmrs::tracker::import::sid::g2::{G2Config, RowNote};
    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..];
    let c = G2Config::detect(data).expect("G2");
    let bank = c.instruments(data);
    let ptrs = c.order_pointers(data, 0);
    for v in 0..3 {
        let rows = c.voice_rows(data, ptrs[v], &bank);
        let ticks = c.row_ticks(data, 0, rows.len() + 2);
        // An ENTRY starts on a struck note or a rest. NOT on `GateOff`: the
        // expansion appends one of those to close each unheld note, so counting
        // it would report two boundaries per entry (the tell was our ticks
        // arriving in close pairs — 22/25, 30/33 — against one chip fetch).
        let starts: Vec<u32> = rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.note, RowNote::Play(_) | RowNote::Rest))
            .filter_map(|(i, _)| ticks.get(i).copied())
            .collect();
        let mut gaps: std::collections::BTreeMap<u32, u32> = std::collections::BTreeMap::new();
        for w in starts.windows(2) {
            *gaps.entry(w[1] - w[0]).or_default() += 1;
        }
        let mut top: Vec<(u32, u32)> = gaps.into_iter().collect();
        top.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
        let g: Vec<String> = top
            .iter()
            .take(8)
            .map(|(k, n)| format!("{k}x{n}"))
            .collect();
        eprintln!(
            "voice {v}: {} entries in one pass, intervals {}",
            starts.len(),
            g.join(" ")
        );
        eprintln!(
            "          first 14 start ticks {:?}",
            &starts[..starts.len().min(14)]
        );
    }
}

/// Our ping-pong counter against the REAL replayer's, frame by frame.
///
/// Needs the work-RAM dump produced by:
///   sidplay --seconds 90 --statedump lh_vib.csv \
///           --stateaddrs 1003,1006,1009,1a5b,1a5e lion_heart.sid
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_vibrato_counter_vs_ram() {
    const SECS: usize = 90;
    let path = ram_dump_path("G2_RAM_DUMP", "lh_vib.csv");
    let Ok(csv) = std::fs::read_to_string(&path) else {
        eprintln!("no RAM dump at {path} — skipping (see the command above)");
        return;
    };
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| head.iter().position(|h| *h == name);
    let cols: Vec<usize> = (0..3).filter_map(|v| col(&format!("1003_{v}"))).collect();
    assert_eq!(cols.len(), 3, "RAM dump is missing the 1003_* columns");
    // Optional: the replayer's frames-since-note-on counter ($1ace,X), against
    // our `note_frame`. Both should read k on the k-th frame after a trigger.
    let nf_cols: Vec<usize> = (0..3).filter_map(|v| col(&format!("1ace_{v}"))).collect();
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    let pick = |r: &Vec<&str>, cs: &[usize]| -> [u8; 3] {
        let mut out = [0u8; 3];
        for (v, &c) in cs.iter().enumerate() {
            out[v] = u8::from_str_radix(r.get(c).unwrap_or(&"0"), 16).unwrap_or(0);
        }
        out
    };
    let chip: Vec<[u8; 3]> = rows.iter().map(|r| pick(r, &cols)).collect();
    let chip_nf: Vec<[u8; 3]> = if nf_cols.len() == 3 {
        rows.iter().map(|r| pick(r, &nf_cols)).collect()
    } else {
        Vec::new()
    };

    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..];
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(SECS * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ours = xmrs::generators::sid::driver::state_take();

    if !chip_nf.is_empty() {
        for v in 0..3 {
            let mut best = (0i32, 0u32);
            for sh in -4..=4 {
                let same = (1..chip_nf.len())
                    .filter(|&i| {
                        let j = i as i32 - sh;
                        j >= 1
                            && (j as usize) < ours.len()
                            && chip_nf[i][v] == ours[j as usize][6 + v]
                    })
                    .count() as u32;
                if same > best.1 {
                    best = (sh, same);
                }
            }
            eprintln!(
                "voice {v}: frames-since-note-on identical {}/{} at shift {}",
                best.1,
                chip_nf.len(),
                best.0
            );
        }
    }
    // CALIBRATION FIRST. Every conclusion below depends on pairing our frame
    // with the right replayer frame, and two earlier rounds of conclusions were
    // wrong because that pairing was assumed. The vibrato ARMING is unambiguous:
    // the replayer's `lh_vib_param` ($1a5b,X) goes 0 → non-zero on one exact
    // frame, and our `interp_vib` flag does the same. The gap between them IS
    // the alignment.
    if let Some(pc) = (0..3)
        .map(|v| col(&format!("1a5b_{v}")))
        .collect::<Option<Vec<usize>>>()
    {
        for v in 0..3 {
            let chip_first = rows.iter().position(|r| {
                u8::from_str_radix(r.get(pc[v]).unwrap_or(&"0"), 16).unwrap_or(0) != 0
            });
            let ours_first = ours.iter().position(|s| s[3 + v] & 4 != 0);
            match (chip_first, ours_first) {
                (Some(c), Some(o)) => eprintln!(
                    "voice {v}: vibrato armed — chip frame {c}, ours {o} ⇒ alignment {}",
                    c as i32 - o as i32
                ),
                _ => eprintln!("voice {v}: vibrato never armed on one side"),
            }
        }
    }

    // WAVE-PROGRAM BEND ACCUMULATOR ($1a93,X lo / $1a96,X hi) against ours.
    // The replayer SUBTRACTS it from the frequency at `$16f4`, in last position,
    // so an error here overrides every modulator upstream — the exact shape of
    // "two faithful modulators, both making the sound worse".
    if let (Some(bl), Some(bh)) = (
        (0..3)
            .map(|v| col(&format!("1a93_{v}")))
            .collect::<Option<Vec<usize>>>(),
        (0..3)
            .map(|v| col(&format!("1a96_{v}")))
            .collect::<Option<Vec<usize>>>(),
    ) {
        for v in 0..3 {
            let (mut n, mut same, mut nonzero) = (0u32, 0u32, 0u32);
            for i in 1..rows.len() {
                let j = i as i32 - 1;
                if j < 1 || j as usize >= ours.len() {
                    continue;
                }
                let j = j as usize;
                let hex =
                    |c: usize| u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0);
                let chip = u16::from_le_bytes([hex(bl[v]), hex(bh[v])]);
                let mine = u16::from_le_bytes([ours[j][15 + v * 2], ours[j][16 + v * 2]]);
                n += 1;
                if chip != 0 {
                    nonzero += 1;
                }
                if chip == mine {
                    same += 1;
                }
            }
            eprintln!(
                "voice {v}: bend accumulator identical {same}/{n} ({:.0}%), chip non-zero on {nonzero}",
                100.0 * same as f64 / n.max(1) as f64
            );
        }
    }

    // WAVE-PROGRAM CURSOR ($1ad7,X) against ours. It is the one pitch modulator
    // never checked against RAM, it runs LAST and OWNS the frequency — so an
    // error here would absorb the others', which is exactly the symptom.
    if let Some(wc) = (0..3)
        .map(|v| col(&format!("1ad7_{v}")))
        .collect::<Option<Vec<usize>>>()
    {
        for v in 0..3 {
            let (mut n, mut same) = (0u32, 0u32);
            for i in 1..rows.len() {
                let j = i as i32 - 1;
                if j < 1 || j as usize >= ours.len() {
                    continue;
                }
                n += 1;
                let chip_idx =
                    u8::from_str_radix(rows[i].get(wc[v]).unwrap_or(&"0"), 16).unwrap_or(0);
                // The replayer's cursor is a BYTE offset into the table (steps
                // are 2 or 3 bytes wide); ours is a step ordinal. Compare only
                // whether both sit at the start (0) or have moved on, which is
                // enough to expose a cursor that never advances or never resets.
                if (chip_idx == 0) == (ours[j as usize][12 + v] == 0) {
                    same += 1;
                }
            }
            eprintln!(
                "voice {v}: wave cursor at-start agreement {same}/{n} ({:.0}%)",
                100.0 * same as f64 / n.max(1) as f64
            );
        }
    }

    // THE BARE PITCH PATH. Two independent modulators are provably faithful yet
    // each degrades the sound, so something downstream absorbs their error.
    // Isolate it: on frames where the replayer applies NO modulation at all —
    // vibrato parameter zero AND arpeggio parameter zero — the frequency is the
    // plain note. If that is already wrong, the note→register path is the
    // compensator and every modulator fix will keep looking bad.
    if let (Some(vp), Some(ap)) = (
        (0..3)
            .map(|v| col(&format!("1a5b_{v}")))
            .collect::<Option<Vec<usize>>>(),
        (0..3)
            .map(|v| col(&format!("1a55_{v}")))
            .collect::<Option<Vec<usize>>>(),
    ) {
        let ora_regs = load_oracle_csv("lion_heart_g2#0");
        for v in 0..3 {
            let (lo, hi) = (v * 7, 1 + v * 7);
            let (mut n, mut bad) = (0u32, 0u32);
            for i in 1..rows.len().min(ora_regs.len()) {
                let hex =
                    |c: usize| u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0);
                if hex(vp[v]) != 0 || hex(ap[v]) != 0 {
                    continue;
                }
                let j = i as i32 - 2;
                if j < 0 || j as usize >= syn.len() {
                    continue;
                }
                let a = u16::from_le_bytes([syn[j as usize][lo], syn[j as usize][hi]]) as f64;
                let b = u16::from_le_bytes([ora_regs[i][lo], ora_regs[i][hi]]) as f64;
                if a < 16.0 || b < 16.0 {
                    continue;
                }
                n += 1;
                if ((a - b).abs() / b) > 0.015 {
                    bad += 1;
                }
            }
            eprintln!(
                "voice {v}: UNMODULATED frames {bad}/{n} wrong ({:.1}%)",
                100.0 * bad as f64 / n.max(1) as f64
            );
        }
    }

    // VIBRATO ARMING, the same split that cracked the arpeggio: the replayer
    // steps its ping-pong only inside the armed block, so a frame where the two
    // sides disagree about whether a vibrato is armed AT ALL cannot be fixed by
    // any counter law — it is the import naming a different instrument state.
    if let Some(vp) = (0..3)
        .map(|v| col(&format!("1a5b_{v}")))
        .collect::<Option<Vec<usize>>>()
    {
        for v in 0..3 {
            let (mut n, mut chip_only, mut ours_only) = (0u32, 0u32, 0u32);
            for i in 1..rows.len() {
                let j = i as i32 - 1;
                if j < 1 || j as usize >= ours.len() {
                    continue;
                }
                let j = j as usize;
                // Only while OUR voice is sounding: a silent voice's chain
                // early-outs on both sides, so its counter freezes either way.
                if ours[j][3 + v] & 1 == 0 {
                    continue;
                }
                n += 1;
                let chip =
                    u8::from_str_radix(rows[i].get(vp[v]).unwrap_or(&"0"), 16).unwrap_or(0) != 0;
                let mine = ours[j][3 + v] & 4 != 0;
                match (chip, mine) {
                    (true, false) => chip_only += 1,
                    (false, true) => ours_only += 1,
                    _ => {}
                }
            }
            eprintln!(
                "voice {v}: vibrato ARMED on the chip but not here {chip_only}/{n}; \
                 here but not on the chip {ours_only}"
            );
            // And what the chip does during OUR silences — the premise of the
            // free-run. If its parameter is zero there, its counter is frozen
            // too and free-running is simply wrong; if it is armed and the
            // counter moves, the free-run is right and only its exceptions are
            // in question.
            let (mut silent, mut armed, mut moved) = (0u32, 0u32, 0u32);
            for i in 1..rows.len() {
                let j = i as i32 - 1;
                if j < 1 || j as usize >= ours.len() || ours[j as usize][3 + v] & 1 != 0 {
                    continue;
                }
                silent += 1;
                if u8::from_str_radix(rows[i].get(vp[v]).unwrap_or(&"0"), 16).unwrap_or(0) != 0 {
                    armed += 1;
                }
                if chip[i][v] != chip[i - 1][v] {
                    moved += 1;
                }
            }
            eprintln!(
                "   during our {silent} silent frames the chip was armed on {armed} \
                 and its counter moved on {moved}"
            );
        }
    }

    // Arpeggio phase ($1a58,X) against ours — only voice 1 arpeggiates here.
    if let Some(ac) = (0..3)
        .map(|v| col(&format!("1a58_{v}")))
        .collect::<Option<Vec<usize>>>()
    {
        for v in 0..3 {
            let same = (1..rows.len())
                .filter(|&i| {
                    let j = i as i32 - 1;
                    j >= 1
                        && (j as usize) < ours.len()
                        && u8::from_str_radix(rows[i].get(ac[v]).unwrap_or(&"0"), 16).unwrap_or(0)
                            == ours[j as usize][9 + v]
                })
                .count();
            eprintln!("voice {v}: arp phase identical {same}/{}", rows.len());
        }
        // Is the disagreement about the COUNTER or about whether an arpeggio is
        // armed at all? The replayer skips the entire block — phase step
        // included — when `$1a55,X` is zero, so an arming that disagrees makes
        // the phase disagree for free, and no counter law can fix it.
        if let Some(ap) = (0..3)
            .map(|v| col(&format!("1a55_{v}")))
            .collect::<Option<Vec<usize>>>()
        {
            for v in 0..3 {
                let (mut n, mut arm_bad, mut phase_bad_when_armed_ok) = (0u32, 0u32, 0u32);
                for i in 1..rows.len() {
                    let j = i as i32 - 1;
                    if j < 1 || j as usize >= ours.len() {
                        continue;
                    }
                    let j = j as usize;
                    let hex = |c: usize| {
                        u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0)
                    };
                    n += 1;
                    let chip_armed = hex(ap[v]) != 0;
                    let our_armed = ours[j][3 + v] & 8 != 0;
                    if chip_armed != our_armed {
                        arm_bad += 1;
                    } else if hex(ac[v]) != ours[j][9 + v] {
                        phase_bad_when_armed_ok += 1;
                    }
                }
                eprintln!(
                    "voice {v}: arp ARMING disagrees on {arm_bad}/{n}; phase wrong while \
                     arming agrees on {phase_bad_when_armed_ok}"
                );
                if phase_bad_when_armed_ok == 0 {
                    continue;
                }
                // Show the counter itself over the first disagreeing stretch:
                // a constant lag reads as a reset/alignment bug, a wrap landing
                // on a different value reads as a wrong cycle length.
                let mut shown = 0;
                for i in 1..rows.len() {
                    let j = i as i32 - 1;
                    if j < 1 || j as usize >= ours.len() || shown >= 22 {
                        continue;
                    }
                    let j = j as usize;
                    let hex = |c: usize| {
                        u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0)
                    };
                    // Start the trace at the note-on BEFORE the first
                    // disagreement: a counter only ever goes wrong at a reset
                    // or at a frame one side skipped, and both are invisible
                    // once the two are already out of step.
                    if hex(ap[v]) == 0 {
                        continue;
                    }
                    if shown == 0 && !(ours[j][6 + v] == 1 && i > 780) {
                        continue;
                    }
                    shown += 1;
                    eprintln!(
                        "      f{i} phase {}/{} param {:02x} nf {}/{}",
                        ours[j][9 + v],
                        hex(ac[v]),
                        hex(ap[v]),
                        ours[j][6 + v],
                        col(&format!("1ace_{v}")).map_or(255, hex),
                    );
                }
            }
        }
    }

    // THE discriminator: on frames where both counters show the SAME position,
    // is the frequency the same too? If yes the swing formula is exact and every
    // remaining error is phase. If no, the formula itself is off and the phase
    // has been masking it — which is what "the rule improves but the sound gets
    // worse" would mean.
    {
        let ora_regs = load_oracle_csv("lion_heart_g2#0");
        for v in 0..3 {
            let (lo, hi) = (v * 7, 1 + v * 7);
            let (mut m_n, mut m_bad, mut d_n, mut d_bad) = (0u32, 0u32, 0u32, 0u32);
            for i in 1..chip.len().min(ora_regs.len()) {
                // Registers at -2, state at -1 — see the calibration note above.
                let (jr, js) = (i as i32 - 2, i as i32 - 1);
                if jr < 0 || js < 1 || jr as usize >= syn.len() || js as usize >= ours.len() {
                    continue;
                }
                let (jr, js) = (jr as usize, js as usize);
                let a = u16::from_le_bytes([syn[jr][lo], syn[jr][hi]]) as f64;
                let b = u16::from_le_bytes([ora_regs[i][lo], ora_regs[i][hi]]) as f64;
                if a < 16.0 || b < 16.0 {
                    continue;
                }
                let bad = ((a - b).abs() / b) > 0.015;
                if chip[i][v] == ours[js][v] {
                    m_n += 1;
                    m_bad += bad as u32;
                } else {
                    d_n += 1;
                    d_bad += bad as u32;
                }
            }
            eprintln!(
                "voice {v}: counters AGREE → {m_bad}/{m_n} freq wrong ({:.0}%) | \
                 counters DIFFER → {d_bad}/{d_n} ({:.0}%)",
                100.0 * m_bad as f64 / m_n.max(1) as f64,
                100.0 * d_bad as f64 / d_n.max(1) as f64
            );
        }
    }
    // The register capture's -2 does not apply here: `state_push` samples at the
    // START of our frame while the RAM dump is taken AFTER the replayer's
    // `play()`, so scan for the alignment instead of assuming one.
    for v in 0..3 {
        // Two metrics, and only the second one judges the RULE.
        //   * identical positions — sensitive to phase, so a correct rule that
        //     shifts the whole cycle scores WORSE than a wrong one that happens
        //     to drift back into step. This is what made two good experiments
        //     look bad.
        //   * advance agreement — did both sides step, or both freeze, on the
        //     same frame? Phase-independent, so it measures the law directly.
        // ⚠️ Scanning for the BEST shift is a trap: a change that shifts our
        // whole phase by a constant scores better here while sounding worse,
        // because the register comparison is pinned to a FIXED alignment. The
        // free-run through silence fooled this metric exactly that way (best-
        // shift agreement up, voice-2 frequency errors 974→1280). Judge at the
        // fixed alignment; the scan is kept only to expose a moving offset.
        let mut fixed = (0u32, 0u32);
        for i in 1..chip.len() {
            // State aligns at -1, registers at -2, and the ONE-frame difference
            // is structural, not a fudge: we sample the counter at the top of
            // the frame, BEFORE `modulate` steps it, while the replayer steps
            // its counter first and uses it after — so the RAM dump is one
            // advance ahead of our snapshot. Calibrated on an unambiguous
            // signal: the vibrato arming flips 0→armed at chip frame 898/514/2
            // against our 897/513/1 on the three voices.
            let j = i as i32 - 1;
            if j < 1 || j as usize >= ours.len() {
                continue;
            }
            let j = j as usize;
            if chip[i][v] == ours[j][v] {
                fixed.0 += 1;
            }
            if (chip[i][v] != chip[i - 1][v]) == (ours[j][v] != ours[j - 1][v]) {
                fixed.1 += 1;
            }
        }
        eprintln!(
            "voice {v}: AT FIXED ALIGNMENT — {} identical, {} advance-agree",
            fixed.0, fixed.1
        );
        // The same, restricted to frames where our voice is SOUNDING. Over a
        // long silence our timeline may carry no entries at all while the
        // replayer keeps fetching them, so the unrestricted figure above partly
        // measures a gap that cannot be heard. This one measures what can.
        let mut audible = (0u32, 0u32, 0u32);
        for i in 1..chip.len() {
            let j = i as i32 - 1;
            if j < 1 || j as usize >= ours.len() || ours[j as usize][3 + v] & 1 == 0 {
                continue;
            }
            let j = j as usize;
            audible.2 += 1;
            if chip[i][v] == ours[j][v] {
                audible.0 += 1;
            }
            if (chip[i][v] != chip[i - 1][v]) == (ours[j][v] != ours[j - 1][v]) {
                audible.1 += 1;
            }
        }
        eprintln!(
            "voice {v}: WHILE SOUNDING — {}/{} identical ({:.0}%), {} advance-agree",
            audible.0,
            audible.2,
            100.0 * audible.0 as f64 / audible.2.max(1) as f64,
            audible.1
        );
        // THE frame that matters. Nothing ever resynchronises this counter — it
        // free-runs from the tune's first armed note to its last, and the
        // note-on block clears the frame counter, the wave cursor and the
        // arpeggio phase but never `$1003,X`. So ONE wrong advance offsets the
        // phase for the rest of the tune, and the percentage above is not a
        // measure of how wrong the law is, it is a measure of how early the
        // first slip happened. Find the slip; the law is downstream of it.
        let first = (2..chip.len().min(ours.len() + 1)).find(|&i| {
            let j = i - 1;
            j >= 1 && j < ours.len() && chip[i][v] != ours[j][v] && chip[i - 1][v] == ours[j - 1][v]
        });
        match first {
            Some(i) => {
                eprintln!("   first divergence at chip frame {i}:");
                for k in i.saturating_sub(4)..(i + 4).min(chip.len()) {
                    let j = k as i32 - 1;
                    if j < 1 || j as usize >= ours.len() {
                        continue;
                    }
                    let j = j as usize;
                    eprintln!(
                        "      f{k} pos {}/{} flags {:04b} nf {}",
                        ours[j][v],
                        chip[k][v],
                        ours[j][3 + v],
                        ours[j][6 + v]
                    );
                }
            }
            None => eprintln!("   never diverges"),
        }
        let mut best = (0i32, 0u32, 0u32);
        for sh in -6..=6 {
            let (mut same, mut agree, mut n) = (0u32, 0u32, 0u32);
            for i in 1..chip.len() {
                let j = i as i32 - sh;
                if j < 1 || j as usize >= ours.len() {
                    continue;
                }
                let j = j as usize;
                n += 1;
                if chip[i][v] == ours[j][v] {
                    same += 1;
                }
                if (chip[i][v] != chip[i - 1][v]) == (ours[j][v] != ours[j - 1][v]) {
                    agree += 1;
                }
            }
            let _ = n;
            if agree > best.2 {
                best = (sh, same, agree);
            }
        }
        eprintln!(
            "voice {v}: best shift {} — {} identical, {} advance-agree",
            best.0, best.1, best.2
        );
    }
    // The alignment the vibrato-arming event calibrates (chip frame N ↔ our
    // N-1), NOT the registers' -2. This block used to say 2 and so reported
    // every one of our freezes as landing one frame late — an artefact of its
    // own pairing, which is exactly the class of mistake the calibration above
    // exists to prevent.
    const SHIFT: i32 = 1;
    for v in 0..3 {
        let (mut same, mut n) = (0u32, 0u32);
        let (mut we_froze, mut chip_froze) = (0u32, 0u32);
        let (mut wf, mut cf): (Vec<usize>, Vec<usize>) = (vec![], vec![]);
        let mut why: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for i in 1..chip.len() {
            let j = i as i32 - SHIFT;
            if j < 1 || j as usize >= ours.len() {
                continue;
            }
            let j = j as usize;
            n += 1;
            if chip[i][v] == ours[j][v] {
                same += 1;
            }
            let c_adv = chip[i][v] != chip[i - 1][v];
            let o_adv = ours[j][v] != ours[j - 1][v];
            if c_adv && !o_adv {
                we_froze += 1;
                // WHY did we freeze while the chip moved? The flags say whether
                // the voice was sounding and whether its vibrato was armed at
                // all — the two early-outs on our side.
                let fl = ours[j][3 + v];
                *why.entry(match (fl & 1 != 0, fl & 4 != 0) {
                    (false, _) => "voice silent",
                    (true, false) => "vibrato not armed",
                    (true, true) => "armed and sounding (fetch frame?)",
                })
                .or_default() += 1;
                if wf.len() < 12 {
                    wf.push(i);
                }
            }
            if !c_adv && o_adv {
                chip_froze += 1;
                if cf.len() < 12 {
                    cf.push(i);
                }
            }
        }
        eprintln!(
            "voice {v}: identical {same}/{n} ({:.0}%)  we-froze-chip-moved={we_froze} \
             chip-froze-we-moved={chip_froze}",
            100.0 * same as f64 / n.max(1) as f64
        );
        // A freeze is a pattern-entry fetch. Comparing the INTERVALS between
        // consecutive fetches separates "right lengths, wrong offset" (same
        // intervals) from "wrong lengths" (different intervals) — the latter is
        // an entry-decoding bug, not a modulation one.
        let gaps = |f: &[usize]| -> std::collections::BTreeMap<usize, u32> {
            let mut m = std::collections::BTreeMap::new();
            for w in f.windows(2) {
                *m.entry(w[1] - w[0]).or_default() += 1;
            }
            m
        };
        let mine: Vec<usize> = (2..chip.len())
            .filter(|&i| {
                let j = i as i32 - 2;
                j >= 1
                    && (j as usize) < ours.len()
                    && ours[j as usize][v] == ours[j as usize - 1][v]
            })
            .collect();
        let theirs: Vec<usize> = (2..chip.len())
            .filter(|&i| chip[i][v] == chip[i - 1][v] && !(chip[i][v] == 5 && chip[i - 2][v] == 4))
            .collect();
        let top = |m: std::collections::BTreeMap<usize, u32>| {
            let mut t: Vec<(usize, u32)> = m.into_iter().collect();
            t.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
            t.into_iter()
                .take(6)
                .map(|(k, n)| format!("{k}×{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        eprintln!("   fetch intervals chip: {}", top(gaps(&theirs)));
        eprintln!("   fetch intervals ours: {}", top(gaps(&mine)));
        eprintln!("   why we froze: {why:?}");
        eprintln!("   we froze at {wf:?}");
        eprintln!("   chip froze at {cf:?}");
        // A freeze IS a pattern-entry fetch. If ours land ±1 frame off the
        // chip's rather than on them, the residual is row-boundary jitter (the
        // 8/3-frames-per-row cadence rounding differently on the two sides),
        // not a vibrato law — and no vibrato change can fix it.
        let fetches = |f: &dyn Fn(usize) -> bool, n: usize| -> Vec<usize> {
            (1..n).filter(|&i| f(i)).collect()
        };
        let ours_f = fetches(
            &|i| {
                let j = i as i32 - SHIFT;
                j >= 1
                    && (j as usize) < ours.len()
                    && ours[j as usize][v] == ours[j as usize - 1][v]
            },
            chip.len(),
        );
        let chip_f = fetches(&|i| chip[i][v] == chip[i - 1][v], chip.len());
        let mut d: std::collections::BTreeMap<i64, u32> = std::collections::BTreeMap::new();
        for &o in &ours_f {
            if let Some(c) = chip_f.iter().min_by_key(|&&c| (c as i64 - o as i64).abs()) {
                *d.entry(o as i64 - *c as i64).or_default() += 1;
            }
        }
        let mut top: Vec<(i64, u32)> = d.into_iter().collect();
        top.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
        let shown: Vec<String> = top
            .iter()
            .take(8)
            .map(|(k, n)| format!("{k:+}×{n}"))
            .collect();
        eprintln!("   our freeze vs nearest chip freeze: {}", shown.join(" "));
    }
}

/// SIGNATURE of the frames that stay wrong once every modulator is verified.
///
/// Each element of the pitch chain now agrees with the replayer's work RAM
/// (vibrato law exact, frames-since-note-on 98/85/87 %, bend accumulator
/// 99-100 %, portamento inactive, note table < 3 %), and yet voice 0 still has
/// 215 of 3466 UNMODULATED frames wrong. A residue that no single component
/// explains is either an ordering effect or a step of the voice program, so
/// stop testing laws and read the residue itself: how big is the error, is it
/// musical (a semitone / an octave) or arbitrary, is it isolated or in runs,
/// and does it coincide with a non-zero bend or with the program cursor having
/// moved.
///
/// Needs the same RAM dump as `g2_vibrato_counter_vs_ram`, plus `1ad7` and the
/// bend pair:
///   sidplay --seconds 90 --statedump lh_vib.csv \
///           --stateaddrs 1003,1a5b,1a55,1a93,1a96,1ad7,1ace lion_heart.sid
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_unmodulated_error_signature() {
    const SECS: usize = 90;
    let path = ram_dump_path("G2_RAM_DUMP", "lh_vib.csv");
    let Ok(csv) = std::fs::read_to_string(&path) else {
        eprintln!("no RAM dump at {path} — skipping (see the command above)");
        return;
    };
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| head.iter().position(|h| *h == name);
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    let cols = |base: &str| -> Option<Vec<usize>> {
        (0..3).map(|v| col(&format!("{base}_{v}"))).collect()
    };
    let (Some(vp), Some(ap)) = (cols("1a5b"), cols("1a55")) else {
        eprintln!("RAM dump lacks the vibrato/arpeggio parameter columns");
        return;
    };
    let bend = cols("1a93").zip(cols("1a96"));
    let cursor = cols("1ad7");

    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..];
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(SECS * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ours = xmrs::generators::sid::driver::state_take();
    let ora = load_oracle_csv("lion_heart_g2#0");

    for v in 0..2 {
        let (lo, hi) = (v * 7, 1 + v * 7);
        let mut wrong: Vec<usize> = Vec::new();
        let mut cents: std::collections::BTreeMap<i64, u32> = std::collections::BTreeMap::new();
        let (mut n, mut with_bend, mut with_cursor) = (0u32, 0u32, 0u32);
        let mut detail: Vec<String> = Vec::new();
        for i in 1..rows.len().min(ora.len()) {
            let hex =
                |c: usize| u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0);
            if hex(vp[v]) != 0 || hex(ap[v]) != 0 {
                continue;
            }
            // Registers align at -2, work RAM at -1 — established by the
            // vibrato-arming calibration, not assumed.
            let (jr, js) = (i as i32 - 2, i as i32 - 1);
            if jr < 0 || js < 1 || jr as usize >= syn.len() || js as usize >= ours.len() {
                continue;
            }
            let (jr, js) = (jr as usize, js as usize);
            let a = u16::from_le_bytes([syn[jr][lo], syn[jr][hi]]) as f64;
            let b = u16::from_le_bytes([ora[i][lo], ora[i][hi]]) as f64;
            if a < 16.0 || b < 16.0 {
                continue;
            }
            n += 1;
            if ((a - b).abs() / b) <= 0.015 {
                continue;
            }
            wrong.push(i);
            // Round to the nearest 10 cents: a musical error piles up on one
            // bucket (100 = a semitone, 1200 = an octave), an arbitrary one
            // spreads out.
            let c = (1200.0 * (a / b).log2() / 10.0).round() as i64 * 10;
            *cents.entry(c).or_default() += 1;
            let cb = bend
                .as_ref()
                .map(|(bl, bh)| u16::from_le_bytes([hex(bl[v]), hex(bh[v])]))
                .unwrap_or(0);
            let cc = cursor.as_ref().map(|cu| hex(cu[v])).unwrap_or(0);
            if cb != 0 {
                with_bend += 1;
            }
            if ours[js][12 + v] != 0 {
                with_cursor += 1;
            }
            if detail.len() < 14 {
                let ob = u16::from_le_bytes([ours[js][15 + v * 2], ours[js][16 + v * 2]]);
                detail.push(format!(
                    "f{i} ours={a:04x} chip={b:04x} {c:+}c ctrl {:02x}/{:02x} bend {ob}/{cb} \
                     cursor {}/{cc} nf {}",
                    syn[jr][4 + v * 7],
                    ora[i][4 + v * 7],
                    ours[js][12 + v],
                    ours[js][6 + v],
                    a = a as u32,
                    b = b as u32,
                ));
            }
        }
        // Runs: an isolated one-frame error is a timing slip, a long run is a
        // wrong value being held.
        let mut runs: std::collections::BTreeMap<usize, u32> = std::collections::BTreeMap::new();
        let mut k = 0usize;
        for w in 0..wrong.len() {
            k += 1;
            if w + 1 == wrong.len() || wrong[w + 1] != wrong[w] + 1 {
                *runs.entry(k).or_default() += 1;
                k = 0;
            }
        }
        let top = |m: &std::collections::BTreeMap<i64, u32>| {
            let mut t: Vec<(i64, u32)> = m.iter().map(|(k, n)| (*k, *n)).collect();
            t.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
            t.into_iter()
                .take(8)
                .map(|(k, n)| format!("{k:+}c×{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        };
        eprintln!(
            "voice {v}: {}/{n} unmodulated frames wrong; with chip bend {with_bend}, \
             with our cursor moved {with_cursor}",
            wrong.len()
        );
        eprintln!("   error size: {}", top(&cents));
        eprintln!(
            "   run lengths: {}",
            runs.iter()
                .map(|(k, n)| format!("{k}×{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for d in &detail {
            eprintln!("   {d}");
        }
        // WHERE the two programs part company. The run itself only shows the
        // steady state; the frame BEFORE it is where one side takes a step the
        // other does not.
        let starts: Vec<usize> = wrong
            .iter()
            .enumerate()
            .filter(|(w, &f)| *w == 0 || wrong[w - 1] + 1 != f)
            .map(|(_, &f)| f)
            .collect();
        for &s in starts.iter().take(2) {
            eprintln!("   ── window around the divergence at f{s}");
            for i in s.saturating_sub(6)..(s + 4).min(rows.len().min(ora.len())) {
                let (jr, js) = (i as i32 - 2, i as i32 - 1);
                if jr < 0 || js < 1 || jr as usize >= syn.len() || js as usize >= ours.len() {
                    continue;
                }
                let (jr, js) = (jr as usize, js as usize);
                let hex =
                    |c: usize| u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0);
                let cc = cursor.as_ref().map(|cu| hex(cu[v])).unwrap_or(0);
                eprintln!(
                    "      f{i} freq {:04x}/{:04x} ctrl {:02x}/{:02x} cursor {}/{cc} nf {}",
                    u16::from_le_bytes([syn[jr][lo], syn[jr][hi]]),
                    u16::from_le_bytes([ora[i][lo], ora[i][hi]]),
                    syn[jr][4 + v * 7],
                    ora[i][4 + v * 7],
                    ours[js][12 + v],
                    ours[js][6 + v],
                );
            }
        }
    }
}

/// ARBITRATION for Sun_Never_Shines: was the vibrato work right for it too?
///
/// Three vibrato fixes — the ping-pong seed, the `>` start-delay guard and the
/// silent-voice free-run — were judged on Lion_Heart, the only tune that had a
/// work-RAM oracle at the time. They took its position agreement from 5 % to
/// 72 % on voice 0, and took Sun_Never_Shines' `freq` counter the WRONG way
/// (1818 → 1967). That was shipped with the reason written down, which is not
/// the same as having checked. This checks: Sun_Never_Shines keeps its
/// replayer's ping-pong at the same addresses as Lion_Heart (`$1003` position,
/// `$1006` range, `$1009` direction — the first bytes of the image), so the
/// same comparison applies.
///
///   sidplay --seconds 90 --statedump sns_vib.csv \
///           --stateaddrs 1003,1006,1009,1a5b,1a5e,1a55,1a58,1ace \
///           Sun_Never_Shines.sid
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_sun_never_shines_pingpong_vs_ram() {
    const SECS: usize = 90;
    let path = ram_dump_path("SNS_RAM_DUMP", "sns_vib.csv");
    let Ok(csv) = std::fs::read_to_string(&path) else {
        eprintln!("no RAM dump at {path} — skipping (see the command above)");
        return;
    };
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |n: &str| head.iter().position(|h| *h == n);
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    let Some(pos) = (0..3)
        .map(|v| col(&format!("1003_{v}")))
        .collect::<Option<Vec<usize>>>()
    else {
        eprintln!("dump lacks the 1003_* columns");
        return;
    };
    let armed = (0..3)
        .map(|v| col(&format!("1a5b_{v}")))
        .collect::<Option<Vec<usize>>>();

    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid")[..];
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(SECS * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ours = xmrs::generators::sid::driver::state_take();
    let ora = load_oracle_csv("sun_never_shines_g2#0");

    for v in 0..3 {
        let hex = |i: usize, c: usize| {
            u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0)
        };
        let (lo, hi) = (v * 7, 1 + v * 7);
        // State pairs at -1 (the snapshot is taken at the top of our frame),
        // registers at -2 — the alignment the golden fits and the vibrato-arming
        // event calibrates. Restricted to frames where our voice is SOUNDING,
        // since a silent voice's counter freezes on our side by construction.
        let (mut n, mut same, mut adv) = (0u32, 0u32, 0u32);
        let (mut agree_n, mut agree_bad) = (0u32, 0u32);
        for i in 1..rows.len().min(ora.len()) {
            let (jr, js) = (i as i32 - 2, i as i32 - 1);
            if jr < 0 || js < 1 || jr as usize >= syn.len() || js as usize >= ours.len() {
                continue;
            }
            let (jr, js) = (jr as usize, js as usize);
            if ours[js][3 + v] & 1 == 0 {
                continue;
            }
            n += 1;
            let (c, o) = (hex(i, pos[v]), ours[js][v]);
            if c == o {
                same += 1;
            }
            if (c != hex(i - 1, pos[v])) == (o != ours[js - 1][v]) {
                adv += 1;
            }
            // The discriminator: where the two counters hold the SAME position,
            // is the frequency the same too? If yes the swing formula is exact
            // and every remaining error is phase.
            if c == o {
                let a = u16::from_le_bytes([syn[jr][lo], syn[jr][hi]]) as f64;
                let b = u16::from_le_bytes([ora[i][lo], ora[i][hi]]) as f64;
                if a >= 16.0 && b >= 16.0 {
                    agree_n += 1;
                    agree_bad += (((a - b).abs() / b) > 0.015) as u32;
                }
            }
        }
        let chip_armed = armed
            .as_ref()
            .map(|a| (1..rows.len()).filter(|&i| hex(i, a[v]) != 0).count())
            .unwrap_or(0);
        eprintln!(
            "voice {v}: WHILE SOUNDING {same}/{n} identical ({:.0}%), {adv} advance-agree; \
             chip armed on {chip_armed} frames",
            100.0 * same as f64 / n.max(1) as f64
        );
        eprintln!(
            "   counters AGREE → {agree_bad}/{agree_n} freq wrong ({:.0}%)",
            100.0 * agree_bad as f64 / agree_n.max(1) as f64
        );
    }
}

/// WHERE a tune's `fhi` divergences are, voice by voice and frame by frame.
///
/// The gate reports one number per tune, which is enough to catch a regression
/// and useless for finding a cause. This splits it: which voice carries the
/// divergences, how big they are, whether they come in runs or singly, and what
/// the two sides actually held over the worst stretch.
///
/// Written for Lakers_vs_Celtics, whose `fhi` is 893 where the other five G2
/// tunes sit between 14 and 222 — while its `freq` is BETTER than Lion_Heart's,
/// so it is not a tune that plays badly all over.
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_fhi_breakdown() {
    const SECS: usize = 90;
    for (key, data) in [
        (
            "lion_heart_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..],
        ),
        (
            "sun_never_shines_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid")[..],
        ),
        (
            "pacific_coast_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/pacific_coast.sid")[..],
        ),
        (
            "radio_ace_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/radio_ace.sid")[..],
        ),
        (
            "go_go_dash_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/go_go_dash.sid")[..],
        ),
        (
            "lakers_vs_celtics_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lakers_vs_celtics.sid")[..],
        ),
        (
            "off_the_cuff_3m#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/off_the_cuff.sid")[..],
        ),
    ] {
        let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
        xmrs::generators::sid::driver::capture_begin();
        let mut player = XmrsPlayer::new(&module, 8000, 0);
        player.set_max_loop_count(1);
        for _ in player.take(SECS * 8000 * 2) {}
        let syn = xmrs::generators::sid::driver::capture_take();
        let ora = load_oracle_csv(key);
        eprintln!("── {key}");
        for v in 0..3 {
            let fhi = 1 + v * 7;
            let mut wrong: Vec<usize> = Vec::new();
            for i in 0..ora.len() {
                let j = i as i32 - 2;
                if j < 0 || j as usize >= syn.len() {
                    continue;
                }
                if (syn[j as usize][fhi] as i32 - ora[i][fhi] as i32).abs() > 8 {
                    wrong.push(i);
                }
            }
            // Runs: one long stretch is a single event gone wrong, a scatter is
            // a law that is slightly off everywhere.
            let mut runs: Vec<(usize, usize)> = Vec::new();
            for &f in &wrong {
                match runs.last_mut() {
                    Some(r) if r.0 + r.1 == f => r.1 += 1,
                    _ => runs.push((f, 1)),
                }
            }
            runs.sort_by_key(|r| core::cmp::Reverse(r.1));
            // A wrong pitch on a voice whose gate is CLOSED cannot be heard.
            // The control register matches the chip on every frame (the `ctrl`
            // column below), so the oracle's own gate bit settles it — and the
            // distinction is the difference between "1576 frames wrong" and
            // "1576 frames wrong, of which this many matter".
            let ctrl_reg = 4 + v * 7;
            let audible = wrong.iter().filter(|&&i| ora[i][ctrl_reg] & 1 == 1).count();
            eprintln!(
                "   voice {v}: {} of {} wrong frames are AUDIBLE (gate open on the chip)",
                audible,
                wrong.len()
            );
            // A divergence you can HEAR is at least as likely to be in the
            // waveform or the envelope as in the pitch, and the golden's `ad` /
            // `sr` / `gate` counters are summed over the three voices, so they
            // cannot say WHICH one. Split them here.
            let (mut ctrl_bad, mut ad_bad, mut sr_bad, mut n) = (0u32, 0u32, 0u32, 0u32);
            for i in 0..ora.len() {
                let j = i as i32 - 2;
                if j < 0 || j as usize >= syn.len() {
                    continue;
                }
                let j = j as usize;
                n += 1;
                ctrl_bad += (syn[j][4 + v * 7] != ora[i][4 + v * 7]) as u32;
                ad_bad += (syn[j][5 + v * 7] != ora[i][5 + v * 7]) as u32;
                sr_bad += (syn[j][6 + v * 7] != ora[i][6 + v * 7]) as u32;
            }
            eprintln!(
                "   voice {v}: fhi {} wrong ({} runs, longest {:?}) | ctrl {ctrl_bad}/{n} \
                 ad {ad_bad} sr {sr_bad}",
                wrong.len(),
                runs.len(),
                runs.iter().take(3).collect::<Vec<_>>()
            );
            // What the control-register divergences actually ARE. A waveform
            // that differs is audible in a way a few register units of pitch
            // never is, so show the pairs rather than just the count.
            if ctrl_bad > 3 {
                let mut pairs: std::collections::BTreeMap<(u8, u8), u32> = Default::default();
                for i in 0..ora.len() {
                    let j = i as i32 - 2;
                    if j < 0 || j as usize >= syn.len() {
                        continue;
                    }
                    let j = j as usize;
                    let (a, b) = (syn[j][4 + v * 7], ora[i][4 + v * 7]);
                    if a != b {
                        *pairs.entry((a, b)).or_default() += 1;
                    }
                }
                let mut t: Vec<((u8, u8), u32)> = pairs.into_iter().collect();
                t.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
                eprintln!(
                    "      ctrl ours/chip: {}",
                    t.iter()
                        .take(8)
                        .map(|((a, b), n)| format!("{a:02x}/{b:02x}×{n}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                );
                // And WHEN. A window at the first one, with the pitch beside it:
                // if the frequency agrees while the waveform does not, the two
                // sides are on the same program step and disagree about what it
                // says; if both differ, we are on a different step.
                if let Some(f) = (0..ora.len()).find(|&i| {
                    let j = i as i32 - 2;
                    j >= 0
                        && (j as usize) < syn.len()
                        && syn[j as usize][4 + v * 7] != ora[i][4 + v * 7]
                }) {
                    for i in f.saturating_sub(3)..(f + 6).min(ora.len()) {
                        let j = i as i32 - 2;
                        if j < 0 || j as usize >= syn.len() {
                            continue;
                        }
                        let j = j as usize;
                        eprintln!(
                            "      f{i} ctrl {:02x}/{:02x} freq {:04x}/{:04x} ad {:02x}/{:02x}",
                            syn[j][4 + v * 7],
                            ora[i][4 + v * 7],
                            u16::from_le_bytes([syn[j][v * 7], syn[j][1 + v * 7]]),
                            u16::from_le_bytes([ora[i][v * 7], ora[i][1 + v * 7]]),
                            syn[j][5 + v * 7],
                            ora[i][5 + v * 7],
                        );
                    }
                }
            }
            let Some(&(start, len)) = runs.first() else {
                continue;
            };
            if len < 4 {
                continue;
            }
            for i in start.saturating_sub(3)..(start + len.min(8) + 1).min(ora.len()) {
                let j = i as i32 - 2;
                if j < 0 || j as usize >= syn.len() {
                    continue;
                }
                let j = j as usize;
                eprintln!(
                    "      f{i} freq {:04x}/{:04x} ctrl {:02x}/{:02x} ad {:02x}/{:02x}",
                    u16::from_le_bytes([syn[j][v * 7], syn[j][fhi]]),
                    u16::from_le_bytes([ora[i][v * 7], ora[i][fhi]]),
                    syn[j][4 + v * 7],
                    ora[i][4 + v * 7],
                    syn[j][5 + v * 7],
                    ora[i][5 + v * 7],
                );
            }
        }
    }
}

/// Frequency divergence BLOCK BY BLOCK, so drift and pitch stop being confused.
///
/// The golden counters fit ONE alignment over the whole 90 s, so a progressive
/// phase drift pins them high and a real improvement can read as a regression.
/// Splitting the capture into blocks separates the two: a change that is right
/// shows up as fewer wrong frames in the EARLY blocks, whatever it does to the
/// late ones, whereas a change that is wrong is worse from the first block on.
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_freq_divergence_by_block() {
    const SECS: usize = 90;
    for (key, data) in [
        (
            "lion_heart_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..],
        ),
        (
            "sun_never_shines_g2#0",
            &include_bytes!("../../xmrs/src/tracker/import/sid/songs/sun_never_shines.sid")[..],
        ),
    ] {
        let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
        xmrs::generators::sid::driver::capture_begin();
        let mut player = XmrsPlayer::new(&module, 8000, 0);
        player.set_max_loop_count(1);
        for _ in player.take(SECS * 8000 * 2) {}
        let syn = xmrs::generators::sid::driver::capture_take();
        let ora = load_oracle_csv(key);
        const BLOCK: usize = 750; // 15 s
        let mut out: Vec<String> = Vec::new();
        for b in 0..(ora.len() / BLOCK) {
            let (mut n, mut bad) = (0u32, 0u32);
            for i in b * BLOCK..((b + 1) * BLOCK).min(ora.len()) {
                let j = i as i32 - 2;
                if j < 0 || j as usize >= syn.len() {
                    continue;
                }
                for v in 0..3 {
                    let (lo, hi) = (v * 7, 1 + v * 7);
                    let a = u16::from_le_bytes([syn[j as usize][lo], syn[j as usize][hi]]) as f64;
                    let c = u16::from_le_bytes([ora[i][lo], ora[i][hi]]) as f64;
                    if a < 16.0 || c < 16.0 {
                        continue;
                    }
                    n += 1;
                    if ((a - c).abs() / c) > 0.015 {
                        bad += 1;
                    }
                }
            }
            out.push(format!("{:.0}%", 100.0 * bad as f64 / n.max(1) as f64));
        }
        eprintln!("{key}: wrong per 15 s block — {}", out.join(" "));
    }
}

/// Do our pattern-entry FETCHES land on the replayer's frames?
///
/// This is the residue's real name. Every per-frame counter the replayer keeps
/// — vibrato ping-pong, arpeggio phase — is stepped INSIDE the effect chain,
/// and the chain is skipped on exactly one occasion: a pattern-entry fetch
/// (`$1104…$13ca` ends in `JMP $17b7`, jumping over `$1403`). So a counter that
/// freezes on a frame the other side steps is not a counter bug at all; it is a
/// fetch one side made and the other did not. Measured directly here, with the
/// replayer's own pattern cursor as the oracle.
///
///   sidplay --seconds 90 --statedump lh_fetch.csv \
///           --stateaddrs 1a34,1a37,1a3d,1a40,1ace lion_heart.sid
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_fetch_frames_vs_replayer() {
    const SECS: usize = 90;
    let path = ram_dump_path("G2_FETCH_DUMP", "lh_fetch.csv");
    let Ok(csv) = std::fs::read_to_string(&path) else {
        eprintln!("no RAM dump at {path} — skipping (see the command above)");
        return;
    };
    let mut lines = csv.lines();
    let head: Vec<&str> = lines.next().unwrap_or_default().split(',').collect();
    let col = |name: &str| head.iter().position(|h| *h == name);
    let rows: Vec<Vec<&str>> = lines.map(|l| l.split(',').collect()).collect();
    let Some(cur) = (0..3)
        .map(|v| col(&format!("1a34_{v}")))
        .collect::<Option<Vec<usize>>>()
    else {
        eprintln!("dump lacks the 1a34_* pattern-cursor columns");
        return;
    };
    let nf = (0..3)
        .map(|v| col(&format!("1ace_{v}")))
        .collect::<Option<Vec<usize>>>();

    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..];
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(SECS * 8000 * 2) {}
    let _ = xmrs::generators::sid::driver::capture_take();
    let ours = xmrs::generators::sid::driver::state_take();

    for v in 0..3 {
        let hex = |i: usize, c: usize| {
            u8::from_str_radix(rows[i].get(c).unwrap_or(&"0"), 16).unwrap_or(0)
        };
        // Chip: the pattern cursor moved ⇒ an entry was consumed this frame.
        let chip_fetch: Vec<usize> = (1..rows.len())
            .filter(|&i| hex(i, cur[v]) != hex(i - 1, cur[v]))
            .collect();
        // Ours: the voice was sounding and its effect chain did NOT run.
        let ours_fetch: Vec<usize> = (1..rows.len())
            .filter(|&i| {
                let j = i as i32 - 1;
                j >= 1
                    && (j as usize) < ours.len()
                    && ours[j as usize][3 + v] & 1 != 0
                    && ours[j as usize][3 + v] & 16 == 0
            })
            .collect();
        let onset: std::collections::BTreeSet<usize> = chip_fetch.iter().copied().collect();
        let matched = ours_fetch.iter().filter(|f| onset.contains(f)).count();
        eprintln!(
            "voice {v}: chip fetches {}, ours {}, on the SAME frame {matched}",
            chip_fetch.len(),
            ours_fetch.len()
        );
        // Distance from each chip fetch to our nearest one: a tight histogram
        // around 0 means placement, a scatter means missing entries entirely.
        let mut d: std::collections::BTreeMap<i64, u32> = std::collections::BTreeMap::new();
        for &c in &chip_fetch {
            match ours_fetch
                .iter()
                .min_by_key(|&&o| (o as i64 - c as i64).abs())
            {
                Some(&o) => *d.entry(o as i64 - c as i64).or_default() += 1,
                None => *d.entry(999).or_default() += 1,
            }
        }
        let mut top: Vec<(i64, u32)> = d.into_iter().collect();
        top.sort_by_key(|&(_, n)| core::cmp::Reverse(n));
        eprintln!(
            "   chip fetch → nearest of ours: {}",
            top.iter()
                .take(8)
                .map(|(k, n)| format!("{k:+}×{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The ones we MISS outright, with the replayer's note-frame counter so a
        // tie (counter keeps running) is distinguishable from a fresh note-on.
        let mine: std::collections::BTreeSet<usize> = ours_fetch.iter().copied().collect();
        // Classify by the replayer's own bytes rather than by a counter: bit 5
        // of the PREVIOUS entry's length byte (`$1a40,X`) is what makes this
        // entry a tie, and `$1ace` still running distinguishes a rest — which
        // also skips the chain — from a fresh note-on.
        let prev_len = (0..3)
            .map(|v| col(&format!("1a40_{v}")))
            .collect::<Option<Vec<usize>>>();
        let kind = |c: usize| -> &'static str {
            let tied = prev_len.as_ref().is_some_and(|p| hex(c, p[v]) & 0x20 != 0);
            let fresh = nf.as_ref().is_some_and(|n| hex(c, n[v]) <= 1);
            match (tied, fresh) {
                (true, _) => "tie",
                (false, true) => "note-on",
                (false, false) => "rest",
            }
        };
        // Only a frame where OUR voice is sounding is comparable: a silent
        // voice's effect chain early-outs on our side too, so its counters
        // freeze exactly as a fetch would freeze them. Counting those as misses
        // would blame the importer for an agreement.
        let our_active = |c: usize| -> bool {
            let j = c as i32 - 1;
            j >= 1 && (j as usize) < ours.len() && ours[j as usize][3 + v] & 1 != 0
        };
        let silent = chip_fetch
            .iter()
            .filter(|&&c| !mine.contains(&c) && !our_active(c))
            .count();
        let missed: Vec<usize> = chip_fetch
            .iter()
            .copied()
            .filter(|&c| !mine.contains(&c) && our_active(c))
            .collect();
        eprintln!("   (plus {silent} on frames where our voice is silent — not comparable)");
        let mut by_kind: std::collections::BTreeMap<&str, u32> = Default::default();
        for &c in &missed {
            *by_kind.entry(kind(c)).or_default() += 1;
        }
        let mut all_kind: std::collections::BTreeMap<&str, u32> = Default::default();
        for &c in &chip_fetch {
            *all_kind.entry(kind(c)).or_default() += 1;
        }
        eprintln!(
            "   chip fetches we never make: {} of {} — missed {by_kind:?} out of {all_kind:?}",
            missed.len(),
            chip_fetch.len(),
        );
        let rowsleft = (0..3)
            .map(|v| col(&format!("1a37_{v}")))
            .collect::<Option<Vec<usize>>>();
        let lenb = (0..3)
            .map(|v| col(&format!("1a3d_{v}")))
            .collect::<Option<Vec<usize>>>();
        for &c in missed.iter().take(6) {
            eprintln!(
                "      missed f{c} ({}) len={:02x} prevlen={:02x} rows-left={:02x} nf={}",
                kind(c),
                lenb.as_ref().map_or(0, |l| hex(c, l[v])),
                prev_len.as_ref().map_or(0, |p| hex(c, p[v])),
                rowsleft.as_ref().map_or(0, |r| hex(c, r[v])),
                nf.as_ref().map_or(0, |n| hex(c, n[v])),
            );
        }
    }
}

/// The ping-pong POSITION, frame by frame, on both sides (temporary diagnostic).
///
/// Four reasoned attempts at the vibrato law all measured worse, so stop
/// comparing laws and read the counter itself: rebuild `pos` from the captured
/// registers as `(value - min) / step`, where `step` is the note's smallest
/// non-zero frequency move. Period, plateaus, direction and phase all become
/// visible at once.
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic"]
fn g2_vibrato_position_trace() {
    const SECS: usize = 90;
    let data = &include_bytes!("../../xmrs/src/tracker/import/sid/songs/lion_heart.sid")[..];
    let module = xmrs::tracker::import::sid::g2::to_module(data).expect("decodes");
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(&module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(SECS * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ora = load_oracle_csv("lion_heart_g2#0");
    let (_, shifts, _) = aligned_voice_counts(&syn, &ora, 8);

    // Voice 2 carries 1047 sub-semitone divergent frames — pure vibrato.
    let v = 2usize;
    let (lo, hi, ctrl) = (v * 7, 1 + v * 7, 4 + v * 7);
    let freq = |f: &[[u8; 25]], i: usize| u16::from_le_bytes([f[i][lo], f[i][hi]]) as i32;

    // Rebuild positions from a run of frames: step = smallest non-zero move.
    let positions = |vals: &[i32]| -> (i32, Vec<i32>) {
        let mut step = i32::MAX;
        for w in vals.windows(2) {
            let d = (w[1] - w[0]).abs();
            if d > 0 && d < step {
                step = d;
            }
        }
        if step == i32::MAX || step == 0 {
            return (0, vec![]);
        }
        let min = *vals.iter().min().unwrap_or(&0);
        (
            step,
            vals.iter().map(|x| (x - min + step / 2) / step).collect(),
        )
    };

    let mut shown = 0;
    for i in 1..ora.len() {
        if shown >= 4 {
            break;
        }
        if !(ora[i][ctrl] & 1 == 1 && ora[i - 1][ctrl] & 1 == 0) {
            continue;
        }
        // How long does the chip hold this note gated?
        let mut len = 0usize;
        while i + len < ora.len() && ora[i + len][ctrl] & 1 == 1 {
            len += 1;
        }
        if len < 24 {
            continue; // too short to show a full swing period
        }
        shown += 1;
        let len = len.min(40);
        let ov: Vec<i32> = (0..len).map(|k| freq(&ora, i + k)).collect();
        let sv: Vec<i32> = (0..len)
            .filter_map(|k| {
                let j = i as i32 + k as i32 + shifts[v];
                (j >= 0 && (j as usize) < syn.len()).then(|| freq(&syn, j as usize))
            })
            .collect();
        let (ostep, opos) = positions(&ov);
        let (sstep, spos) = positions(&sv);
        eprintln!("\nnote@{i} len={len}  step chip={ostep} ours={sstep}");
        eprintln!("  chip pos {opos:?}");
        eprintln!("  ours pos {spos:?}");
    }

    // WHERE does the phase drift in? Per note, the position both sides show on
    // the first swing frame, and how long the voice was silent before it. A
    // steady ±1 per note means a per-note off-by-one; a jump after a long gap
    // means the counter free-runs through silence on one side only.
    eprintln!(
        "\nper-note phase (chip/ours at first swing frame, gap = frames since prev note-off)"
    );
    let mut prev_end = 0usize;
    let mut line = String::new();
    let mut shown = 0;
    for i in 1..ora.len() {
        if !(ora[i][ctrl] & 1 == 1 && ora[i - 1][ctrl] & 1 == 0) {
            continue;
        }
        let mut len = 0usize;
        while i + len < ora.len() && ora[i + len][ctrl] & 1 == 1 {
            len += 1;
        }
        let gap = i.saturating_sub(prev_end);
        prev_end = i + len;
        if len < 12 {
            continue;
        }
        let k = 5usize; // past the flat window
        if i + k + 6 >= ora.len() {
            break;
        }
        let ov: Vec<i32> = (0..len.min(16)).map(|x| freq(&ora, i + x)).collect();
        let sv: Vec<i32> = (0..len.min(16))
            .filter_map(|x| {
                let j = i as i32 + x as i32 + shifts[v];
                (j >= 0 && (j as usize) < syn.len()).then(|| freq(&syn, j as usize))
            })
            .collect();
        let (_, opos) = positions(&ov);
        let (_, spos) = positions(&sv);
        let (Some(&o), Some(&s)) = (opos.get(k), spos.get(k)) else {
            continue;
        };
        line.push_str(&format!(" {o}/{s}(g{gap})"));
        shown += 1;
        if shown % 8 == 0 {
            eprintln!("  {line}");
            line.clear();
        }
        if shown >= 40 {
            break;
        }
    }
    if !line.is_empty() {
        eprintln!("  {line}");
    }
}

/// Does the pulse WIDTH match the chip? `$D402` / `$D403`, directly.
///
/// The fidelity golden counts gate / ad / sr / freq and **nothing else**, so a
/// change to the pulse sweep is invisible to it — "the gate did not move" is
/// then a statement about the metric, not about the change. This exists because
/// that trap was walked into: correcting Sanxion song 2's `fx_v2` address moved
/// no counter at all, and only this probe showed it had worked.
///
/// Measured on that correction ($C5F5, which was the song LIST, → $C555, the
/// table the song's own player names at `$c366`):
///
/// | voice | pw-hi before → after |
/// |---|---|
/// | 0 | 28.6 % → 41.4 % |
/// | 1 | 40.7 % → 51.5 % |
/// | 2 | 10.2 % → 18.5 % |
///
/// Kept rather than thrown away: the project's PW metric used to live in a
/// scratch script that is long gone, and its absence is why a wrong address
/// survived in the record for so long.
#[cfg(feature = "import_sid")]
#[test]
#[ignore = "diagnostic — run alongside the SID fidelity gate"]
fn sanxion2_pulse_width_vs_chip() {
    use xmrs::tracker::import::sid::sid_module::SidModule;
    const SECS: usize = 16;
    let modules = SidModule::get_sid_sanxion_song_2().to_modules();
    xmrs::generators::sid::driver::capture_begin();
    let mut p = XmrsPlayer::new(&modules[0], 8000, 0);
    p.set_max_loop_count(1);
    for _ in p.take(SECS * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ora = load_oracle_csv("sanxion_song_2#0");
    for v in 0..3 {
        let (lo, hi) = (2 + v * 7, 3 + v * 7);
        let (mut ok_lo, mut ok_hi, mut n) = (0u32, 0u32, 0u32);
        for i in 0..ora.len() {
            let j = i as i32 - 2;
            if j < 0 || j as usize >= syn.len() {
                continue;
            }
            n += 1;
            if syn[j as usize][lo] == ora[i][lo] {
                ok_lo += 1;
            }
            if syn[j as usize][hi] == ora[i][hi] {
                ok_hi += 1;
            }
        }
        eprintln!(
            "  voice {v}: pw-lo {:.1}%  pw-hi {:.1}%  of {n}",
            100.0 * ok_lo as f64 / n as f64,
            100.0 * ok_hi as f64 / n as f64
        );
    }
}
