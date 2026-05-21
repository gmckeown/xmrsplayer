//! Diagnostic inspector for the DAW layer of an imported Module.
//!
//! Loads a `.xm` / `.mod` / `.s3m` / `.it` file via the standard
//! `Module::load` autodetection and prints a structural summary:
//! per-song length, instrument count, track and clip counts,
//! Euclidean wrappers detected by `auto_convert_strict_euclidean_tracks`,
//! and the result of `verify_layers_consistent`.
//!
//! Useful when chasing import-time regressions ("which channel does
//! this module skip?", "is the timeline_map covering every order?").
//! Not part of the player runtime — gated behind the `demo` feature.
//!
//! ```text
//! cargo run --release --features=demo --example inspect_mod -- path/to/module.s3m
//! ```

use xmrs::instrument::InstrumentType;
use xmrs::module::Module;

fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: inspect_mod <path-to-module>");
    let data = std::fs::read(&path).expect("read");
    let m = Module::load(&data).expect("load");

    println!("loaded {} — {:?}", path, m.name);
    println!("  default_tempo = {}", m.default_tempo);
    println!("  default_bpm   = {}", m.default_bpm);
    println!("  channels      = {}", m.get_num_channels());
    println!("  instruments   = {}", m.instrument.len());
    println!("  tracks        = {}", m.tracks.len());
    println!(
        "  Σ track rows  = {}",
        m.tracks.iter().map(|t| t.rows.len()).sum::<usize>()
    );
    println!("  clips         = {}", m.clips.len());
    println!("  timeline      = {} entries", m.timeline_map.entries.len());

    let euclidean = m
        .instrument
        .iter()
        .filter(|i| matches!(i.instr_type, InstrumentType::Euclidean(_)))
        .count();
    println!("  Euclidean wrappers = {}", euclidean);

    let n_songs = m
        .timeline_map
        .entries
        .iter()
        .map(|e| e.song)
        .max()
        .map(|s| s as usize + 1)
        .unwrap_or(0);
    for song in 0..n_songs {
        let len = m.song_length(song);
        if len > 0 {
            println!("  song {}: {} orders", song, len);
        }
    }

    match m.verify_layers_consistent() {
        Ok(()) => println!("  verify        = ok"),
        Err(e) => println!("  verify        = FAILED: {:?}", e),
    }
}
