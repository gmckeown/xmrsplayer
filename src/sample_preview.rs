//! Audition one instrument/sample of a module that has **no song**.
//!
//! [`XmrsPlayer`](crate::xmrsplayer::XmrsPlayer) is timeline-driven: it
//! advances a [`Module`](xmrs::core::module::Module)'s clips/lanes and **stops the instant there is
//! nothing to play** (verified: a module with an empty timeline yields
//! zero samples). Some modules carry instruments but no playable
//! timeline — most notably David Whittaker sound-effect banks
//! (`*-sfx.dw`), which the `.dw` importer turns into instrument-only
//! [`Module`](xmrs::core::module::Module)s (`tracks=0`). The samples are right there, but there is
//! nothing for the player to tick through.
//!
//! [`render_instrument`](crate::sample_preview::render_instrument) auditions one of them **without building a
//! second module and without cloning instruments**: it temporarily
//! splices a one-note timeline into the module *in place* (your "add a
//! temporary element, play it, then erase it"), renders it through the
//! normal player, and restores the module to exactly its prior state
//! before returning. Because it reuses the module, the audition cell
//! references the real instrument index directly.
//!
//! ```no_run
//! use xmrs::prelude::*;
//! use xmrsplayer::sample_preview::render_instrument;
//!
//! # fn demo(bank: &mut Module) {
//! // Audition instrument 2 at C-4, up to ~1 s at 48 kHz.
//! let pcm = render_instrument(bank, 2, Pitch::C4, 48_000, 48_000);
//! // `bank` is untouched afterwards; feed `pcm` to your audio sink.
//! # let _ = pcm;
//! # }
//! ```

use alloc::vec;
use alloc::vec::Vec;

use xmrs::core::fixed::units::Volume;
use xmrs::core::module::Module;
use xmrs::core::pitch::Pitch;
use xmrs::tracker::import::build::{build_timeline_layer, Pattern, Row};
use xmrs::tracker::import::unit::TrackImportUnit;

use crate::xmrsplayer::XmrsPlayer;

/// The [`Module`] timeline fields [`build_timeline_layer`] overwrites,
/// stashed so [`render_instrument`] can put the module back exactly as
/// it found it. Kept private — it is an implementation detail of the
/// save/restore dance, never handed to callers.
struct TimelineBackup {
    tracks: Vec<xmrs::core::daw::track::Track>,
    clips: xmrs::core::daw::sorted_clips::SortedClips,
    automation: Vec<xmrs::core::daw::automation::AutomationLane>,
    timeline_map: xmrs::core::daw::timeline::TimelineMap,
    song_loop_to: Option<u32>,
    quirks: xmrs::core::compatibility::PlaybackQuirks,
}

impl TimelineBackup {
    /// Move the timeline-shaped fields out of `module` (leaving them at
    /// `Default`), so a temporary timeline can be installed.
    fn take(module: &mut Module) -> Self {
        Self {
            tracks: core::mem::take(&mut module.tracks),
            clips: core::mem::take(&mut module.clips),
            automation: core::mem::take(&mut module.automation),
            timeline_map: core::mem::take(&mut module.timeline_map),
            song_loop_to: module.song_loop_to, // Copy
            quirks: module.quirks,             // Copy
        }
    }

    /// Put the saved fields back, discarding the temporary timeline.
    fn restore(self, module: &mut Module) {
        module.tracks = self.tracks;
        module.clips = self.clips;
        module.automation = self.automation;
        module.timeline_map = self.timeline_map;
        module.song_loop_to = self.song_loop_to;
        module.quirks = self.quirks;
    }
}

/// One pattern that triggers `instrument` at `pitch` on row 0, then
/// `rows - 1` empty rows to keep the timeline alive while the (one-shot)
/// voice rings. The cell references the live module's instrument index
/// directly — no reindexing, no cloning.
fn audition_pattern(instrument: usize, pitch: Pitch, rows: usize) -> Pattern {
    let rows = rows.max(2);
    let mut trigger: Row = vec![TrackImportUnit::default()];
    trigger[0].note = xmrs::core::cell_note::CellNote::Play(pitch);
    trigger[0].instrument = Some(instrument);
    trigger[0].velocity = Volume::FULL;
    let mut pattern: Pattern = Vec::with_capacity(rows);
    pattern.push(trigger);
    for _ in 1..rows {
        pattern.push(vec![TrackImportUnit::default()]);
    }
    pattern
}

/// Row count whose timeline outlasts `max_samples` at `sample_rate`, so
/// a sample longer than one row isn't cut off when the timeline ends.
/// Tracker cadence: `samples_per_row = rate * 2.5 / bpm * tempo`.
fn rows_for_samples(module: &Module, sample_rate: u32, max_samples: usize) -> usize {
    let bpm = (module.default_bpm as u64).max(1);
    let tempo = (module.default_tempo as u64).max(1);
    let samples_per_row = ((sample_rate as u64 * 5 / 2) / bpm).max(1) * tempo;
    ((max_samples as u64 / samples_per_row) + 2).max(2) as usize
}

/// Splice a one-note audition timeline for `instrument` into `module`
/// **in place and leave it there** — for callers that own the module
/// and will play it straight through [`XmrsPlayer`] (e.g. the CLI), and
/// don't need it restored afterwards. `rows` holds the one-shot voice
/// after the trigger (clamped to >= 2); with the player looping, the
/// audition repeats every `rows` rows. Returns `false` and leaves
/// `module` untouched if `instrument` is out of range.
///
/// Use [`render_instrument`] instead when the module must survive intact.
pub fn install_instrument_audition(
    module: &mut Module,
    instrument: usize,
    pitch: Pitch,
    rows: usize,
) -> bool {
    if instrument >= module.instrument.len() {
        return false;
    }
    let pattern = audition_pattern(instrument, pitch, rows);
    build_timeline_layer(module, &[vec![0]], &[pattern]);
    true
}

/// Render instrument `instrument` of `module` at `pitch` to interleaved
/// stereo PCM, up to `max_samples` frames.
///
/// Works by temporarily installing a one-note audition timeline into
/// `module`, rendering it through [`XmrsPlayer`], then restoring the
/// module to its prior state — so it can be called on a real song
/// module too without corrupting it. Reuses the module's instruments in
/// place; nothing is cloned and no second module is created.
///
/// * `instrument` — index into `module.instrument`; out of range yields
///   an empty buffer.
/// * `pitch` — the note the sample plays at (playback rate scales with
///   it, per the module's `frequency_type`).
/// * `sample_rate` — output rate in Hz.
/// * `max_samples` — hard cap on returned frames; rendering also stops
///   early when the one-note timeline runs out.
///
/// Needs `&mut module` because the audition timeline is spliced in and
/// out in place. The returned [`Vec`] is owned, so the module is fully
/// restored by the time this returns.
pub fn render_instrument(
    module: &mut Module,
    instrument: usize,
    pitch: Pitch,
    sample_rate: u32,
    max_samples: usize,
) -> Vec<(i16, i16)> {
    if instrument >= module.instrument.len() || max_samples == 0 {
        return Vec::new();
    }

    // The player stops at the end of the timeline, so size the audition
    // pattern to outlast `max_samples` — otherwise a sample longer than
    // one row would be cut off when the timeline ends.
    let rows = rows_for_samples(module, sample_rate, max_samples);

    let backup = TimelineBackup::take(module);
    build_timeline_layer(
        module,
        &[vec![0]],
        &[audition_pattern(instrument, pitch, rows)],
    );

    // Render with the temporary timeline (immutable reborrow of
    // `module`); the player is dropped at the end of this block so the
    // `&mut` is free again to restore.
    let pcm = {
        let mut player = XmrsPlayer::new(module, sample_rate, 0);
        player.set_max_loop_count(1);
        let mut out = Vec::new();
        while out.len() < max_samples {
            match player.sample(true) {
                Some(s) => out.push(s),
                None => break,
            }
        }
        out
    };

    backup.restore(module);
    pcm
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use xmrs::prelude::*;

    /// Build a synthetic instrument-only module — the shape a `.dw`
    /// SFX bank imports to (instruments present, no timeline).
    fn instrument_only_bank() -> Module {
        let data: Vec<i8> = (0..64).map(|i| if i < 32 { 100 } else { -100 }).collect();
        let sample = Sample {
            name: "sfx".into(),
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
        instr.keyboard.map_all_to(0);

        let mut bank = Module {
            frequency_type: FrequencyType::AmigaFrequencies,
            ..Module::default()
        };
        bank.instrument.push(Instrument {
            name: "sfx".into(),
            instr_type: InstrumentType::Default(instr),
            muted: false,
        });
        bank
    }

    #[test]
    fn renders_audible_pcm_and_restores_module() {
        let mut bank = instrument_only_bank();
        assert!(bank.clips.is_empty() && bank.tracks.is_empty());

        let pcm = render_instrument(&mut bank, 0, Pitch::C4, 48_000, 48_000);
        assert!(!pcm.is_empty(), "render produced no samples");
        let peak = pcm
            .iter()
            .map(|(l, r)| (*l as i32).abs().max((*r as i32).abs()))
            .max()
            .unwrap_or(0);
        assert!(peak > 0, "render produced silence");

        // The temporary timeline was erased: module is back to
        // instrument-only.
        assert!(bank.clips.is_empty() && bank.tracks.is_empty());
        assert!(bank.timeline_map.entries.is_empty());
    }

    #[test]
    fn out_of_range_instrument_is_empty_and_harmless() {
        let mut bank = instrument_only_bank();
        let pcm = render_instrument(&mut bank, 9, Pitch::C4, 48_000, 1_000);
        assert!(pcm.is_empty());
        assert!(bank.clips.is_empty() && bank.tracks.is_empty());
    }
}
