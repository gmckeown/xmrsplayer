//! Cycle-exact playback smoke tests. Importers produce a populated
//! DAW timeline; the player walks that timeline. There's a single
//! cell source now (post-3b), so the tests verify that the player
//! actually produces non-silent audio on representative bundled
//! demos rather than comparing two modes.

#![cfg(all(feature = "std", any(feature = "import_xm", feature = "import")))]

use xmrs::prelude::*;
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
    let sid = xmrs::import::sid::sid_module::SidModule::get_sid_commando();
    let modules = sid.to_modules(false);
    let module = &modules[0];
    let mut player = XmrsPlayer::new(module, 48000, 0);
    let pcm = render_pcm(&mut player, 4096);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "SID playback should produce audio");
}

fn make_euclidian_module(humanize_prob: xmrs::fixed::fixed::Q15, advance_max: u8) -> Module {
    use xmrs::daw::build_timeline::build_timeline_layer;
    use xmrs::instr_ekn::InstrEkn;

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
    module.instrument.push(Instrument {
        name: "ekn".into(),
        instr_type: InstrumentType::Euclidean(InstrEkn {
            events: 3,
            steps: 8,
            rotation: 0,
            instr: Some(0),
            humanize_advance_max_ticks: advance_max,
            humanize_probability: humanize_prob,
        }),
        muted: false,
    });

    // A 1-pattern song with Play(C4) on channel 0 at row 0 and row
    // 4 — two pulses of the same wrapper Euclidean. The second
    // pulse is the one the humanisation can advance.
    let mut play_row: Row = (0..1).map(|_| TrackUnit::default()).collect();
    play_row[0].note = CellNote::Play(Pitch::C4);
    play_row[0].instrument = Some(1);
    let pattern: Pattern = (0..8)
        .map(|r| {
            if r == 0 || r == 4 {
                play_row.clone()
            } else {
                vec![TrackUnit::default()]
            }
        })
        .collect();
    build_timeline_layer(&mut module, &[vec![0]], &[pattern]);
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

/// `InstrumentType::Euclidean` wraps a Default sample-based
/// instrument via `ekn.instr`. The player follows the pointer and
/// triggers the wrapped sample. Smoke test: non-silent audio.
#[test]
fn euclidian_indirection_plays_wrapped_instrument() {
    let module = make_euclidian_module(xmrs::fixed::fixed::Q15::ZERO, 0);
    let mut player = XmrsPlayer::new(&module, 48000, 0);
    let pcm = render_pcm(&mut player, 2048);
    let nonzero = pcm.iter().filter(|s| **s != 0).count();
    assert!(nonzero > 0, "Euclidean-wrapped sample should produce audio");
}

/// Build a Module with two single-channel patterns, each carrying
/// one trigger of `pitch_a` / `pitch_b` at row 0. Order = `[0, 1]`.
/// If `restart_byte > 0` the helper injects a `PositionJump(restart_byte)`
/// on the last played row via the import pipeline.
fn make_two_pattern_module(pitch_a: Pitch, pitch_b: Pitch, restart_byte: usize) -> Module {
    use xmrs::daw::build_timeline::{build_timeline_layer, inject_restart_position_jump};

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

    let mut row_a: Row = vec![TrackUnit::default()];
    row_a[0].note = CellNote::Play(pitch_a);
    row_a[0].instrument = Some(0);
    let mut row_b: Row = vec![TrackUnit::default()];
    row_b[0].note = CellNote::Play(pitch_b);
    row_b[0].instrument = Some(0);

    let pat_a: Pattern = (0..4)
        .map(|r| {
            if r == 0 {
                row_a.clone()
            } else {
                vec![TrackUnit::default()]
            }
        })
        .collect();
    let pat_b: Pattern = (0..4)
        .map(|r| {
            if r == 0 {
                row_b.clone()
            } else {
                vec![TrackUnit::default()]
            }
        })
        .collect();

    let mut pattern_order = vec![vec![0, 1]];
    let mut patterns = vec![pat_a, pat_b];
    inject_restart_position_jump(&mut pattern_order[0], &mut patterns, restart_byte);

    build_timeline_layer(&mut module, &pattern_order, &patterns);
    module
}

/// Regression guard for the historical "restart byte" (Soundtracker
/// MOD, FastTracker XM). Pre-3b the sequencer wrapped to
/// `Module.restart_position` at end-of-song; post-3b the importer
/// realises it as a `GlobalEffect::PositionJump` on the last played
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

/// §12.1 cycle-exactness: a Track with a Bjorklund(3, 8) pattern
/// rendered before vs. after `auto_convert_strict_euclidean_tracks`
/// must produce bit-identical PCM. The auto-conversion is lossless
/// by definition (§12.1).
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
        use xmrs::daw::build_timeline::{
            build_timeline_map, dedupe_tracks_by_content, extract_tracks_and_clips,
        };
        use xmrs::daw::euclidean::auto_convert_strict_euclidean_tracks;
        use xmrs::daw::sorted_clips::SortedClips;

        let mut m = Module::default();
        m.instrument.push(Instrument {
            name: "sq".into(),
            instr_type: InstrumentType::Default(sample_instr),
            muted: false,
        });
        // Pattern of 8 rows, Play(C4) at rows 0, 3, 6 — canonical
        // Bjorklund(3, 8). Pattern length = 8 = K.
        let mut play_row: Row = (0..1).map(|_| TrackUnit::default()).collect();
        play_row[0].note = CellNote::Play(Pitch::C4);
        play_row[0].instrument = Some(0);
        let pattern: Pattern = (0..8)
            .map(|r| {
                if r == 0 || r == 3 || r == 6 {
                    play_row.clone()
                } else {
                    vec![TrackUnit::default()]
                }
            })
            .collect();
        // Run the same pipeline as `build_timeline_layer` but skip
        // the optional §12.1 pass when `convert` is false. This is
        // the only way to compare pre/post Euclidean conversion on
        // identical underlying timeline data — the previous "undo"
        // hack can't restore the row collapse the conversion now
        // performs.
        let pattern_order = vec![vec![0usize]];
        let patterns = vec![pattern];
        m.timeline_map = build_timeline_map(&m, &pattern_order, &patterns);
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

    // Sanity: the converted module has an Euclidean wrapper.
    assert!(
        module_ekn
            .instrument
            .iter()
            .any(|i| matches!(i.instr_type, InstrumentType::Euclidean(_))),
        "expected an InstrumentType::Euclidean after auto-conversion"
    );
    assert!(
        !module_raw
            .instrument
            .iter()
            .any(|i| matches!(i.instr_type, InstrumentType::Euclidean(_))),
        "raw module should not carry a wrapper"
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
        "§12.1 auto-conversion must be lossless — same PCM pre/post"
    );
}

/// §12.2: with `HumanizeMode::Deterministic(seed)` AND
/// `humanize_probability > 0`, the player must produce a PCM
/// *different* from the same module played with the default
/// `HumanizeMode::Disabled`. Proves humanisation actually fires.
#[test]
fn humanise_deterministic_changes_pcm() {
    use xmrs::fixed::fixed::Q15;
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

/// §12.2 cycle-exactness: with `HumanizeMode::Disabled` (the default),
/// the PCM must be identical regardless of the `InstrEkn`'s
/// `humanize_*` parameter values. The instrument-side params describe
/// the *shape*; the player must obey its mode-side gate.
#[test]
fn humanise_disabled_is_cycle_exact() {
    use xmrs::fixed::fixed::Q15;
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
