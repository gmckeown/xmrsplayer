//! DAW-generalization capstone (DAW_GENERALIZATION_RFC).
//!
//! Two guarantees that the per-phase tests don't cover on their own:
//!
//! 1. **Composition** — one module that exercises the *entire* new
//!    stack at once (Arc samples, per-channel insert chain with a
//!    stateful filter, `DeviceParam` automation, return bus + send,
//!    master bus, `ModScope::Voice` modulation onto an NNA ghost,
//!    per-note `NoteExpression`, and a `Track::Audio` clip) renders
//!    non-silent and deterministically — the pieces actually work
//!    together.
//!
//! 2. **Serde round-trip (§0.3 contract)** — that same module survives
//!    a `serde_json` round-trip with a **bit-identical render**. This
//!    is the runtime check the compatibility contract demands and that
//!    the rest of the suite only proves at compile time (every new core
//!    type carries `Serialize`/`Deserialize`; the round-trip exercises
//!    the `rc`-feature `Arc<[T]>`, the `#[serde(skip)]` filter state,
//!    the `#[serde(default)]` additive fields, and the new enums).

#![cfg(all(feature = "std", feature = "import"))]

use xmrs::core::cell::{Cell, CellEvent, NoteExpression};
use xmrs::core::daw::automation::{
    AutomationLane, AutomationPoint, AutomationTarget, AutomationValue, LaneKind, LfoEvent,
    ModScope,
};
use xmrs::core::daw::clip::Clip;
use xmrs::core::daw::device::{Bus, Device, DeviceChain, Send};
use xmrs::core::daw::sorted_clips::SortedClips;
use xmrs::core::daw::timeline::{TimelineEntry, TimelineMap};
use xmrs::core::daw::track::{AudioSource, Track};
use xmrs::core::fixed::fixed::{Q15, Q8_8};
use xmrs::core::instrument_behavior::{DuplicateCheckType, NewNoteAction};
use xmrs::prelude::*;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const ROWS: u32 = 16;

fn square_sample(looped: bool) -> Sample {
    let data: Vec<i8> = (0..64).map(|i| if i < 32 { 90i8 } else { -90i8 }).collect();
    Sample {
        name: "sq".into(),
        relative_pitch: 0,
        finetune: Finetune::default(),
        volume: ChannelVolume::FULL,
        default_note_volume: Volume::FULL,
        panning: Panning::CENTER,
        loop_flag: if looped {
            LoopType::Forward
        } else {
            LoopType::No
        },
        loop_start: 0,
        loop_length: 64,
        sustain_loop_flag: LoopType::No,
        sustain_loop_start: 0,
        sustain_loop_length: 0,
        data: Some(SampleDataType::Mono8(data.into())),
    }
}

/// A single module wiring up the whole DAW-generalization feature set.
fn daw_project_module() -> Module {
    let mut module = Module::default();

    // --- instrument 0: a sampled square, NNA = Continue so a second
    //     note leaves a still-ringing ghost on channel 0. ---
    let mut instr = InstrDefault::default();
    instr.sample.push(Some(square_sample(true)));
    instr.keyboard.sample_for_pitch = [Some(0); 120];
    instr.behavior.duplicate_check = DuplicateCheckType::Off(NewNoteAction::Continue);
    module.instrument.push(Instrument {
        name: "sq".into(),
        instr_type: InstrumentType::Default(instr),
        muted: false,
    });

    let speed = module.default_tempo as u32;

    // --- track 0: Notes on channel 0. Two C-4 triggers (the second
    //     forks a ghost); the first carries a per-note pitch bend. ---
    let mut rows = vec![Cell::default(); ROWS as usize];
    rows[0].event = CellEvent::NoteOn {
        pitch: Pitch::C4,
        velocity: Volume::FULL,
    };
    rows[0].expression = NoteExpression {
        pitch_bend: PitchDelta::from_semitones(4),
        ..NoteExpression::NEUTRAL
    };
    rows[2].event = CellEvent::NoteOn {
        pitch: Pitch::C4,
        velocity: Volume::FULL,
    };
    module.tracks.push(Track::Notes {
        name: "lead".into(),
        instrument: 0,
        rows,
        muted: false,
    });

    // --- track 1: a recorded-audio clip on channel 1, sourced from the
    //     shared asset pool (RFC §4 — stored once, referenced by index). ---
    let rec_asset = module.push_asset(square_sample(true));
    module.tracks.push(Track::Audio {
        name: "rec".into(),
        muted: false,
        source: AudioSource::Pooled(rec_asset),
        source_rate: 32_000, // ≠ 48 kHz output ⇒ exercises the resampler
    });

    // --- clips: both tracks span the whole timeline. ---
    let clip = |track: u32, ch: u8| Clip {
        track,
        song: 0,
        target_channel: ch,
        position_tick: 0,
        speed_at_start: speed as u8,
        track_row_offset: 0,
        source_start_row: 0,
        end_tick: ROWS * speed,
    };
    module.clips = SortedClips::from_unsorted(vec![clip(0, 0), clip(1, 1)]);

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

    // --- per-channel insert chain on channel 0: gain + a stateful
    //     low-pass. ---
    let half = Volume::from_q15(Q15::from_raw(16_384));
    module.channel_inserts = vec![
        DeviceChain {
            devices: vec![
                Device::Gain(half),
                Device::LowPass {
                    cutoff: Q15::from_raw(8_000),
                    state: (0, 0),
                },
            ],
            bypass: false,
        },
        DeviceChain::default(),
    ];

    // --- return bus + a send from channel 0. ---
    module.buses = vec![Bus {
        name: "aux".into(),
        chain: DeviceChain {
            devices: vec![Device::Gain(Volume::FULL)],
            bypass: false,
        },
    }];
    module.channel_sends = vec![vec![Send {
        bus: 0,
        level: half,
    }]];

    // --- master bus. ---
    module.master_chain = DeviceChain {
        devices: vec![Device::Gain(Volume::from_q15(Q15::from_raw(29_000)))],
        bypass: false,
    };

    // --- automation: a Voice-scoped vibrato (reaches the ghost) and a
    //     DeviceParam lane driving channel 0's insert gain. ---
    let vib_events = (0..ROWS)
        .map(|row| LfoEvent::Set {
            tick: row * speed,
            speed: Q8_8::from_raw(0x40),
            depth: Q15::from_raw(6_000),
            waveform: Waveform::BipolarSine,
            retrig: true,
        })
        .collect();
    let mut vib = AutomationLane::new_with_kind(
        AutomationTarget::TrackPitch(0),
        LaneKind::Lfo { events: vib_events },
    );
    vib.scope = ModScope::Voice;
    module.automation.push(vib);

    let mut dev = AutomationLane::new(AutomationTarget::DeviceParam {
        track: 0,
        device: 0,
        param: 0,
    });
    dev.kind = LaneKind::Points(vec![AutomationPoint {
        tick: 0,
        value: AutomationValue::Normalized(Q15::from_raw(24_000)),
    }]);
    module.automation.push(dev);

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

fn render(module: &Module, frames: usize) -> Vec<i16> {
    let player = XmrsPlayer::new(module, 48000, 0);
    let mut buf = Vec::with_capacity(frames * 2);
    for s in player.into_iter().take(frames * 2) {
        buf.push(s);
    }
    while buf.len() < frames * 2 {
        buf.push(0);
    }
    buf
}

const FRAMES: usize = 24_000;

#[test]
fn full_daw_stack_composes_and_renders() {
    let m = daw_project_module();

    // Structural: every generalization construct is present.
    assert!(!m.instrument.is_empty());
    assert!(m.tracks.iter().any(|t| t.is_audio()), "has a Track::Audio");
    assert!(!m.assets.is_empty(), "has a pooled audio asset (§4)");
    assert!(
        m.tracks
            .iter()
            .any(|t| matches!(t.audio_source(), Some(AudioSource::Pooled(_)))),
        "the audio track references the pool"
    );
    assert!(
        m.channel_inserts[0]
            .devices
            .iter()
            .any(|d| matches!(d, Device::LowPass { .. })),
        "channel 0 has a stateful LowPass insert"
    );
    assert!(!m.buses.is_empty(), "has a return bus");
    assert!(!m.channel_sends.is_empty(), "has a send");
    assert!(m.master_chain.is_active(), "master bus active");
    assert!(
        m.automation.iter().any(|l| l.scope == ModScope::Voice),
        "has a Voice-scoped lane"
    );
    assert!(
        m.automation
            .iter()
            .any(|l| matches!(l.target, AutomationTarget::DeviceParam { .. })),
        "has a DeviceParam lane"
    );

    // Behavioural: it renders non-silent and is deterministic.
    let pcm = render(&m, FRAMES);
    let energy: i64 = pcm.iter().map(|s| (*s as i64).abs()).sum();
    assert!(energy > 0, "the full DAW project must produce audio");
    assert_eq!(
        hash_pcm(&pcm),
        hash_pcm(&render(&m, FRAMES)),
        "render must be deterministic"
    );
}

#[test]
fn daw_project_survives_serde_round_trip() {
    let m = daw_project_module();
    let h1 = hash_pcm(&render(&m, FRAMES));

    // §0.3: serialise → deserialise must preserve the render exactly.
    let json = serde_json::to_string(&m).expect("serialize Module to JSON");

    // Sanity: the new constructs really are in the serialised form.
    for marker in [
        "LowPass",
        "Audio",
        "Voice",
        "DeviceParam",
        "expression",
        "Pooled",
        "assets",
    ] {
        assert!(
            json.contains(marker),
            "serialised module should mention {marker:?}"
        );
    }

    let m2: Module = serde_json::from_str(&json).expect("deserialize Module from JSON");
    let h2 = hash_pcm(&render(&m2, FRAMES));

    assert_eq!(
        h1, h2,
        "a serde round-trip must render bit-identically (§0.3 contract): \
         the Arc PCM, filter state-skip, additive defaults and new enums \
         must all survive serialise → deserialise"
    );
}
