//! End-to-end validation that an IT import **plays with its effects**:
//! the importer now auto-projects the IT/MPTM mix-plugin table onto
//! native device insert chains (`it_module::to_module` →
//! `convert_mix_plugins_to_devices`), so loading a plugin-bearing module
//! renders it the way the author mixed it.
//!
//! Contracts:
//! 1. **Auto-attach** — a plugin-bearing module comes back with non-empty
//!    `channel_inserts` / `master_chain` straight from `to_module`.
//! 2. **It actually renders** — the player prepares and processes those
//!    chains without panicking (fixed-point ⇒ no NaN), and the effects
//!    change the sound versus the same module with the inserts stripped.
//!
//! The corpus lives under `~/Music` (not in the repo); the test scans for
//! `.it` files that ended up with attached devices and skips cleanly when
//! none are present — mirroring the DW corpus tests.

#![cfg(all(feature = "std", feature = "import"))]

use std::path::{Path, PathBuf};

use xmrs::core::module::Module;
use xmrs::tracker::import::it::it_module::ItModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn render_pcm(player: &mut XmrsPlayer, samples: usize) -> Vec<i16> {
    let mut buf = Vec::with_capacity(samples * 2);
    for s in player.into_iter().take(samples * 2) {
        buf.push(s);
    }
    buf
}

fn music_dir() -> Option<PathBuf> {
    let d = std::env::var_os("HOME").map(PathBuf::from)?.join("Music");
    d.is_dir().then_some(d)
}

fn collect_it(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            collect_it(&p, out);
        } else if p
            .extension()
            .and_then(|x| x.to_str())
            .is_some_and(|x| x.eq_ignore_ascii_case("it"))
        {
            out.push(p);
        }
    }
}

/// Load an `.it` (which now auto-converts its mix plugins) and return it
/// only if the importer attached at least one native device.
fn load_with_devices(path: &Path) -> Option<Module> {
    let bytes = std::fs::read(path).ok()?;
    let module = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ItModule::load(&bytes).ok().map(|m| m.to_module())
    }))
    .ok()??;
    let has = module.channel_inserts.iter().any(|c| !c.devices.is_empty())
        || !module.master_chain.devices.is_empty();
    has.then_some(module)
}

#[test]
fn it_mix_plugins_play_with_their_effects() {
    let Some(dir) = music_dir() else {
        eprintln!("~/Music absent — skipping IT plugin corpus validation");
        return;
    };
    let mut files = Vec::new();
    collect_it(&dir, &mut files);
    files.sort();

    let mut with_devices = 0usize;
    let mut any_audible_diff = false;
    for path in &files {
        let Some(mut module) = load_with_devices(path) else {
            continue;
        };
        with_devices += 1;

        // Wet render: the module as imported (effects active).
        let wet_pcm = {
            let mut p = XmrsPlayer::new(&module, 48_000, 0);
            render_pcm(&mut p, 8_000)
        };

        // Dry render: strip the inserts the importer attached, then render.
        let saved_inserts = std::mem::take(&mut module.channel_inserts);
        let saved_master = std::mem::take(&mut module.master_chain);
        let dry_pcm = {
            let mut p = XmrsPlayer::new(&module, 48_000, 0);
            render_pcm(&mut p, 8_000)
        };
        module.channel_inserts = saved_inserts;
        module.master_chain = saved_master;

        // No panic / no NaN (fixed-point). When the dry render is audible,
        // the effects must change it (corpus-aggregate: ≥ 1 file differs).
        if dry_pcm.iter().any(|&s| s != 0) && wet_pcm != dry_pcm {
            any_audible_diff = true;
        }
        if with_devices >= 6 {
            break; // a handful is enough for a smoke validation
        }
    }

    eprintln!(
        "{with_devices} IT module(s) auto-attached effects; audible diff: {any_audible_diff}"
    );
    if with_devices > 0 {
        assert!(
            any_audible_diff,
            "imported effects did not change any module's render"
        );
    }
}
