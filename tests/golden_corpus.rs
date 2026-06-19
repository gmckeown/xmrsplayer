//! Phase 0 — the compatibility contract (DAW_GENERALIZATION_RFC §0).
//!
//! The "golden corpus" is the backbone that makes every later
//! generalization phase *verifiable* rather than merely promised:
//! a frozen, hashed reference render over a representative sample of
//! every supported format. Any *behavior-preserving* generalization
//! PR must keep these hashes bit-identical; any *additive* PR must
//! leave the tracker render path untouched, which this test proves by
//! construction (the importers never emit the new constructs).
//!
//! Coverage (RFC §0.1): one small representative per format —
//! XM + IT + MOD + S3M — plus the DW hard cases that exercise the
//! trickiest importer paths (bubble bobble, grimblood, archipelagos,
//! bad company). The fixtures are bundled (`include_bytes!`) so the
//! gate runs in CI with no dependency on `~/Music`; the corpus-wide
//! robustness sweep over the full `~/Music/dw` set still lives in
//! `dw_robustness.rs`.
//!
//! **Regenerating the reference.** When an *intentional* renderer
//! change shifts the canonical output, rebake the hash table:
//!
//! ```sh
//! XM_GOLDEN_REGEN=1 cargo test -p xmrsplayer --test golden_corpus
//! ```
//!
//! This rewrites `tests/fixtures_corpus/golden_hashes.txt`. Review the
//! diff: a behavior-preserving phase that changes *any* line is a
//! regression, not a rebake.

#![cfg(all(feature = "std", feature = "import"))]

use std::collections::BTreeMap;
use std::path::PathBuf;

use xmrs::prelude::*;
use xmrs::tracker::import::dw::dw_module::DwModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

/// Render rate and length. 1 s at 48 kHz captures each song's intro
/// (note triggers, the first envelope/LFO motion, and a sustained
/// tail) — enough surface for a bit-exact signature without bloating
/// the run.
const SAMPLE_RATE: u32 = 48_000;
const FRAMES: usize = 48_000;

const HASHES_FILE: &str = "tests/fixtures_corpus/golden_hashes.txt";

/// FNV-1a over the little-endian PCM byte stream. Same construction
/// as the `hash_pcm` used by the cycle-exact PCM tests, so a drift
/// here and a drift there mean the same thing.
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

fn render_pcm(module: &Module, frames: usize) -> Vec<i16> {
    let player = XmrsPlayer::new(module, SAMPLE_RATE, 0);
    let mut buf = Vec::with_capacity(frames * 2);
    for s in player.into_iter().take(frames * 2) {
        buf.push(s);
    }
    while buf.len() < frames * 2 {
        buf.push(0);
    }
    buf
}

/// The frozen corpus. Each entry decodes a bundled fixture to a
/// `Module` through its real importer; the closure isolates the
/// per-format load call so the table reads as a manifest.
fn corpus() -> Vec<(&'static str, Module)> {
    vec![
        (
            "xm:lamb",
            Module::load_xm(include_bytes!("fixtures_corpus/lamb.xm")).expect("load lamb.xm"),
        ),
        (
            "it:myheart",
            Module::load_it(include_bytes!("fixtures_corpus/myheart.it")).expect("load myheart.it"),
        ),
        (
            "mod:lizardking",
            Module::load_mod(include_bytes!("fixtures_corpus/lizardking.mod"))
                .expect("load lizardking.mod"),
        ),
        (
            "s3m:star_chips",
            Module::load_s3m(include_bytes!("fixtures_corpus/star_chips.s3m"))
                .expect("load star_chips.s3m"),
        ),
        (
            // OPL/AdLib FM render path: two voices (FM-feedback + additive),
            // triggers, sustains and note-cuts. Freezes the clean-room OPL
            // chip output (see `xmrs/src/generators/opl/`). Built by
            // `examples/gen_opl_fixture.rs`.
            "s3m:opl_min",
            Module::load_s3m(include_bytes!("fixtures_corpus/opl_min.s3m"))
                .expect("load opl_min.s3m"),
        ),
        (
            "dw:grimblood",
            DwModule::load(include_bytes!("fixtures_corpus/grimblood.dw"))
                .expect("load grimblood.dw")
                .to_module(),
        ),
        (
            "dw:bubble_bobble",
            DwModule::load(include_bytes!("fixtures_corpus/bubble_bobble.dw"))
                .expect("load bubble_bobble.dw")
                .to_module(),
        ),
        (
            "dw:archipelagos",
            DwModule::load(include_bytes!("fixtures_corpus/archipelagos.dw"))
                .expect("load archipelagos.dw")
                .to_module(),
        ),
        (
            "dw:bad_company",
            DwModule::load(include_bytes!("fixtures_corpus/bad_company.dw"))
                .expect("load bad_company.dw")
                .to_module(),
        ),
    ]
}

fn hashes_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push(HASHES_FILE);
    p
}

fn read_expected() -> BTreeMap<String, u64> {
    let path = hashes_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) => panic!(
            "missing golden hash table {} ({}). Run with `XM_GOLDEN_REGEN=1` \
             to bake a fresh reference.",
            path.display(),
            e
        ),
    };
    let mut map = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (name, hex) = line
            .split_once('=')
            .unwrap_or_else(|| panic!("malformed golden line: {:?}", line));
        let h = u64::from_str_radix(hex.trim(), 16)
            .unwrap_or_else(|_| panic!("malformed golden hash for {}: {:?}", name, hex));
        map.insert(name.trim().to_string(), h);
    }
    map
}

/// RFC §1B additive invariant: no importer emits **more than one**
/// Lfo / Slide / Glide lane for the same `(target, kind)`. This is the
/// premise that makes the player's summed-modulator fold
/// behavior-preserving — with ≤1 lane the sum reduces to the single
/// lane, identical to the pre-1B `find_map` first-match. (`Points`
/// lanes are exempt: the player deliberately *sums* them, e.g. the DW
/// per-frame arpeggio transpose, so multiple are legitimate.)
///
/// The DW importer attaches lanes per sub-song and so used to leave
/// dead duplicate `TrackPitch` Lfo lanes on multi-sub-song tunes
/// (grimblood); `collapse_dead_duplicate_modulation_lanes` in the DW
/// importer now removes them. This test is the regression guard for
/// that invariant across the whole representative corpus.
#[test]
fn no_importer_stacks_modulation_lanes() {
    use std::collections::BTreeMap;
    use xmrs::core::daw::automation::LaneKind;

    let mut violations: Vec<String> = Vec::new();
    for (name, module) in corpus() {
        // key: (debug-formatted target, kind discriminant) → count
        let mut counts: BTreeMap<(String, u8), usize> = BTreeMap::new();
        for lane in &module.automation {
            let disc = match &lane.kind {
                LaneKind::Points(_) => continue, // summed — exempt
                LaneKind::Lfo { .. } => 1u8,
                LaneKind::Slide { .. } => 2,
                LaneKind::Glide { .. } => 3,
            };
            *counts
                .entry((format!("{:?}", lane.target), disc))
                .or_default() += 1;
        }
        for ((target, disc), n) in counts {
            if n > 1 {
                let kind = match disc {
                    1 => "Lfo",
                    2 => "Slide",
                    _ => "Glide",
                };
                violations.push(format!("  {}: {} × {} on {}", name, n, kind, target));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "importer emitted >1 modulation lane per (target, kind) — the §1B \
         summed fold would silently change these renders:\n{}",
        violations.join("\n")
    );
}

#[test]
fn golden_corpus_is_bit_identical() {
    let entries = corpus();

    // Each render must be reproducible within the same process — the
    // contract is meaningless if the renderer isn't deterministic.
    let actual: BTreeMap<String, u64> = entries
        .iter()
        .map(|(name, module)| {
            let h1 = hash_pcm(&render_pcm(module, FRAMES));
            let h2 = hash_pcm(&render_pcm(module, FRAMES));
            assert_eq!(
                h1, h2,
                "{}: render is non-deterministic — same module, same seed, \
                 two different PCM hashes ({:#018x} vs {:#018x})",
                name, h1, h2
            );
            (name.to_string(), h1)
        })
        .collect();

    if std::env::var("XM_GOLDEN_REGEN").is_ok() {
        let mut out = String::new();
        out.push_str("# DAW_GENERALIZATION_RFC §0 — frozen golden corpus.\n");
        out.push_str("# Rebake: XM_GOLDEN_REGEN=1 cargo test -p xmrsplayer --test golden_corpus\n");
        out.push_str("# A behavior-preserving phase must NOT change any line below.\n");
        for (name, h) in &actual {
            out.push_str(&format!("{} = {:016x}\n", name, h));
        }
        std::fs::write(hashes_path(), out).expect("write golden hash table");
        eprintln!("golden_corpus: rebaked {} hashes", actual.len());
        return;
    }

    let expected = read_expected();

    let mut drifts: Vec<String> = Vec::new();
    for (name, h) in &actual {
        match expected.get(name) {
            Some(e) if e == h => {}
            Some(e) => drifts.push(format!(
                "  {}: expected {:016x}, rendered {:016x}",
                name, e, h
            )),
            None => drifts.push(format!(
                "  {}: no reference hash (new corpus entry) — rendered {:016x}",
                name, h
            )),
        }
    }
    for name in expected.keys() {
        if !actual.contains_key(name) {
            drifts.push(format!(
                "  {}: reference hash has no matching corpus entry",
                name
            ));
        }
    }

    if !drifts.is_empty() {
        panic!(
            "golden corpus drift ({} entr{}):\n{}\n\nIf this renderer change \
             is INTENTIONAL, rebake with `XM_GOLDEN_REGEN=1 cargo test -p \
             xmrsplayer --test golden_corpus` and review the diff. A \
             behavior-preserving generalization phase must leave every hash \
             unchanged.",
            drifts.len(),
            if drifts.len() == 1 { "y" } else { "ies" },
            drifts.join("\n"),
        );
    }
}
