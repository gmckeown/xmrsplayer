//! Period-trace oracle: print a channel's effective Amiga period per row.
//! Usage: period_trace <file.s3m> <channel_idx> <num_rows>
use std::env;
use xmrs::prelude::*;
use xmrsplayer::prelude::*;

fn main() {
    let a: Vec<String> = env::args().collect();
    let path = &a[1];
    let ch: usize = a[2].parse().unwrap();
    let rows: usize = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(8);
    let data = std::fs::read(path).unwrap();
    let module = Module::load_s3m(&data).unwrap();
    let mut p = XmrsPlayer::new(&module, 48000, 0);
    p.goto(0, 0, 0);
    let mut last_row = usize::MAX;
    let mut printed = 0;
    // step tick by tick, print the snapshot at the FIRST tick of each new row
    for _ in 0..(rows * 16) {
        p.advance_trace_tick();
        let row = p.get_current_row();
        if row != last_row {
            last_row = row;
            let s = &p.channel_snapshots()[ch];
            println!(
                "row {:2}: period={} eff_period={} vol={} gate={}",
                row, s.period, s.effective_period, s.volume, s.gate
            );
            printed += 1;
            if printed >= rows {
                break;
            }
        }
    }
}
