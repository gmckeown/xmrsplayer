//! DW Tier-2 fidelity — player-side trace driver.
//!
//! Renders a `.dw` through `XmrsPlayer` one replayer tick at a time and
//! emits a CSV in the **exact same schema** as the Ghidra Paula oracle
//! goldens in `xmrs/src/tracker/import/dw/oracle/*.csv`, so the two can
//! be diffed frame-to-frame to build `BASELINE.md`.
//!
//! The only library surface this needs is the read-only trace API
//! (`XmrsPlayer::advance_trace_tick` + `channel_snapshots`); everything
//! else — CSV formatting, the synthetic DMACON mask, alignment — lives
//! here in the example, not in the core crate.
//!
//! ```text
//! cargo run --release --no-default-features --features std,import_dw \
//!     --example dw_oracle_trace -- <file.dw> [ticks=1500] [out.csv]
//! ```
//!
//! Schema (matches the oracle):
//! `tick,dma,c0g,c0per,c0vol,c1g,c1per,c1vol,c2g,c2per,c2vol,c3g,c3per,c3vol,f0`
//! `dma` is synthesised from the gates (`0x200` master | per-channel
//! bit); `f0` is always `-1` (the player has no replayer Stop flag).

use std::fmt::Write as _;

use xmrs::tracker::import::dw::dw_module::DwModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn main() -> Result<(), String> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        return Err(format!(
            "usage: {} <file.dw> [ticks=1500] [out.csv]",
            args[0]
        ));
    }
    let path = &args[1];
    let ticks: usize = args.get(2).map(|s| s.parse().unwrap_or(1500)).unwrap_or(1500);
    let out = args.get(3).cloned();

    // Optional sub-song selector (`DW_SUBSONG=2`), default the primary
    // song 0. The importer renders each DW sub-song as its own xmrs
    // song; this picks which one to trace.
    let song: usize = std::env::var("DW_SUBSONG")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let data = std::fs::read(path).map_err(|e| format!("read {}: {}", path, e))?;
    let dw = DwModule::load(&data).map_err(|e| format!("DwModule::load: {:?}", e))?;
    let module = dw.to_module();

    // Global delay-counter throttle (`play_tick` head: an 8-bit
    // accumulator `DAT_5ea += DAT_5e8` per 50 Hz frame; on byte-carry
    // the whole channel loop is SKIPPED — a frozen frame). The oracle
    // CSV is in 50 Hz frames *including* those skips; the player trace
    // advances one replayer tick per call, with the slowdown folded
    // into BPM instead (clean row grid). To line the two up frame-for-
    // frame we replay the same accumulator here and, on a carry frame,
    // emit a *frozen* row (snapshot without advancing). Modules with no
    // delay (`e8 = 0`) never carry, so this is a no-op for them.
    let e8: u32 = if dw.features.enable_delay_counter {
        let d = dw
            .sub_songs
            .get(song)
            .or(dw.sub_song.as_ref())
            .map(|s| s.delay_speed as u32)
            .unwrap_or(0);
        if dw.features.enable_delay_multiply {
            d.wrapping_mul(16) & 0xFF
        } else {
            d & 0xFF
        }
    } else {
        0
    };
    let mut delay_acc: u32 = 0;

    // 48 kHz is arbitrary: period/volume/gate are sample-rate independent.
    let mut player = XmrsPlayer::new(&module, 48000, song);
    // Keep looping so we capture a full `ticks` window even on short songs.
    player.set_max_loop_count(1000);

    let nch = module.get_num_channels();

    let mut csv = String::new();
    csv.push_str("tick,dma,c0g,c0per,c0vol,c0s,c1g,c1per,c1vol,c1s,c2g,c2per,c2vol,c2s,c3g,c3per,c3vol,c3s,f0\n");

    for t in 0..ticks {
        // Replay the replayer's delay accumulator: on a byte-carry this
        // 50 Hz frame is a *skip* (channels frozen), so snapshot without
        // advancing — matching the oracle's frozen frame.
        delay_acc += e8;
        let frozen = delay_acc > 0xFF;
        delay_acc &= 0xFF;
        if !frozen && !player.advance_trace_tick() {
            let _ = writeln!(csv, "# song_end@{}", t);
            break;
        }
        let snap = player.channel_snapshots();

        // Synthetic DMACON: master enable | one bit per gated channel,
        // matching the oracle's audio-DMA mask layout (bit i = AUDi).
        let mut dma = 0u16;
        for (i, s) in snap.iter().enumerate().take(4) {
            if s.gate {
                dma |= 1 << i;
            }
        }
        if dma != 0 {
            dma |= 0x200;
        }

        let _ = write!(csv, "{},{:x}", t, dma);
        for i in 0..4 {
            match snap.get(i) {
                // `effective_period` (post arpeggio / vibrato / pitch
                // envelope) is the value to diff against the Paula
                // oracle's `AUDxPER`; the bare `period` is the
                // un-modulated note and stays blind to vibrato/arp.
                Some(s) if i < nch => {
                    // On a frozen (delay-counter carry) frame the player did
                    // NOT advance — `struck_this_tick` still holds the prior
                    // real tick's value, so it must be reported as 0 here or
                    // every note onset on a delay-counter module shows a
                    // spurious second strike (the real replayer skips its
                    // whole channel loop on a carry frame too).
                    let struck = if frozen { 0 } else { s.struck as u8 };
                    let _ = write!(
                        csv,
                        ",{},{},{},{}",
                        s.gate as u8, s.effective_period, s.volume, struck
                    );
                }
                // Pad missing channels (oracle is always 4-wide).
                _ => csv.push_str(",0,0,0,0"),
            }
        }
        csv.push_str(",-1\n");
    }

    match out {
        Some(p) => std::fs::write(&p, csv).map_err(|e| format!("write {}: {}", p, e))?,
        None => print!("{}", csv),
    }
    Ok(())
}
