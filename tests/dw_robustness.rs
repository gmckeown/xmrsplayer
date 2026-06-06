//! Tier 1 — DW corpus playback robustness (PLAN.md §2, Session A).
//!
//! Complements the *loader* gate in `xmrs` (which only asserts the
//! `.dw` decodes to a consistent DAW layer): this test actually
//! **renders** every supported `~/Music/dw` module through
//! [`XmrsPlayer`] for one full song loop and asserts:
//!
//! * the render never panics / hangs (bounded by a frame cap);
//! * every channel that *carries notes* produces non-silent audio —
//!   a channel that triggers a `NoteOn` yet mixes pure digital
//!   silence is a real bug (dead instrument binding, lane that zeroes
//!   the voice, etc.), not an approximation.
//!
//! It needs no oracle, so it covers the whole 116-file corpus. The
//! fidelity tier (Paula-trace diff against the Ghidra oracle) lives
//! elsewhere; see `xmrs/src/tracker/import/dw/PLAN.md`.
//!
//! Skipped silently when `~/Music/dw` is absent (CI / fresh checkout).

#![cfg(all(feature = "std", any(feature = "import_dw", feature = "import")))]

use xmrs::core::cell::CellEvent;
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::module::Module;
use xmrs::tracker::import::bin_reader::ImportError;
use xmrs::tracker::import::dw::dw_module::DwModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

/// The player renders song 0 only; the corpus-wide robustness check
/// must scope its expectations to the same sub-song.
const RENDERED_SONG: u16 = 0;

/// Render sample rate. Lower than 48 kHz to keep the corpus-wide
/// render cheap — robustness and per-channel energy don't depend on
/// the exact rate.
const SAMPLE_RATE: u32 = 24_000;
/// Hard upper bound per module (~60 s at `SAMPLE_RATE`). One song
/// loop is normally far shorter; the cap only guards against a
/// pathological never-ending render.
const FRAME_CAP: usize = SAMPLE_RATE as usize * 60;

/// Per-channel note census for the *rendered* sub-song.
struct ChannelNotes {
    /// Carries at least one `NoteOn` / `NoteOnGhost` (any velocity).
    has_notes: Vec<bool>,
    /// Carries at least one note with **non-zero velocity** — the
    /// only kind we require to be audible. DW bakes the per-frame
    /// volume envelope down to the row; many channels legitimately
    /// trigger their notes at row-velocity 0 and ramp up through a
    /// continuous volume lane that isn't modelled yet (PLAN.md
    /// backlog #2/#3). Those are reported, not failed.
    has_audible: Vec<bool>,
}

fn channel_notes(module: &Module) -> ChannelNotes {
    let num_ch = module.get_num_channels();
    let mut has_notes = vec![false; num_ch];
    let mut has_audible = vec![false; num_ch];
    for entry in &module.timeline_map.entries {
        if entry.song != RENDERED_SONG {
            continue;
        }
        let cells = module.row_at(
            entry.song as usize,
            entry.pattern_idx as usize,
            entry.row_idx as usize,
        );
        for (ch, (cell, _)) in cells.iter().enumerate() {
            if ch >= num_ch {
                break;
            }
            if let CellEvent::NoteOn { velocity, .. } | CellEvent::NoteOnGhost { velocity, .. } =
                &cell.event
            {
                has_notes[ch] = true;
                if velocity.raw() != Q15::ZERO {
                    has_audible[ch] = true;
                }
            }
        }
    }
    ChannelNotes {
        has_notes,
        has_audible,
    }
}

/// Render one song loop, accumulating per-channel sum-of-squares.
/// Returns `(sumsq_per_channel, frames_rendered)`.
fn render_channel_energy(module: &Module) -> (Vec<f64>, usize) {
    let num_ch = module.get_num_channels();
    let mut player = XmrsPlayer::new(module, SAMPLE_RATE, 0);
    // One full traversal then stop — DW songs loop forever otherwise.
    player.set_max_loop_count(1);

    let mut sumsq = vec![0.0f64; num_ch];
    let mut frames = 0usize;
    while frames < FRAME_CAP {
        match player.samples_from_channels() {
            Some(chans) => {
                for (i, (l, r)) in chans.iter().enumerate() {
                    if i >= num_ch {
                        break;
                    }
                    let v = (*l as f64 + *r as f64) * 0.5;
                    sumsq[i] += v * v;
                }
                frames += 1;
            }
            None => break,
        }
    }
    (sumsq, frames)
}

#[test]
fn render_all_dw_modules_robustness() {
    let dir = "/home/user/Music/dw";
    let Ok(entries) = std::fs::read_dir(dir) else {
        eprintln!("skipped: {} not found", dir);
        return;
    };

    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("dw"))
        .collect();
    paths.sort();

    let mut failures: Vec<String> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    let mut rendered = 0usize;
    let mut skipped = 0usize;

    for path in &paths {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("<?>");
        let Ok(data) = std::fs::read(path) else {
            failures.push(format!("{}: read failed", name));
            continue;
        };

        let dw = match DwModule::load(&data) {
            Ok(m) => m,
            // SFX / placeholder banks legitimately carry no song.
            Err(ImportError::InvalidMagic("dw_no_song")) => {
                skipped += 1;
                continue;
            }
            Err(e) => {
                failures.push(format!("{}: DwModule::load: {:?}", name, e));
                continue;
            }
        };

        let module = dw.to_module();
        let census = channel_notes(&module);
        let (sumsq, frames) = render_channel_energy(&module);

        // Hard failure: a channel that triggers an audible (velocity
        // > 0) note yet mixes pure digital silence is a real bug —
        // dead instrument binding, a lane that zeroes the voice, etc.
        // Soft warning: a channel whose every note is row-velocity 0
        // and renders silent — the unmodelled continuous volume lane
        // (PLAN.md backlog #2/#3). Tracked, not failed.
        let mut hard: Vec<usize> = Vec::new();
        let mut soft: Vec<usize> = Vec::new();
        for ch in 0..census.has_notes.len() {
            let silent = sumsq.get(ch).copied().unwrap_or(0.0) == 0.0;
            if !silent {
                continue;
            }
            if census.has_audible[ch] {
                hard.push(ch);
            } else if census.has_notes[ch] {
                soft.push(ch);
            }
        }
        if !hard.is_empty() {
            failures.push(format!(
                "{}: {} frames, audible channel(s) {:?} rendered pure silence",
                name, frames, hard
            ));
        }
        if !soft.is_empty() {
            warnings.push(format!(
                "{}: channel(s) {:?} carry only row-velocity-0 notes and render \
                 silent (volume-lane modelling gap)",
                name, soft
            ));
        }
        rendered += 1;
    }

    eprintln!(
        "render_all_dw_modules_robustness: {} rendered, {} sfx/placeholder, {} warnings, {} failures",
        rendered,
        skipped,
        warnings.len(),
        failures.len()
    );
    for w in &warnings {
        eprintln!("  WARN {}", w);
    }
    if !failures.is_empty() {
        for f in &failures {
            eprintln!("  FAIL {}", f);
        }
        panic!("{} module(s) failed robustness render", failures.len());
    }
}
