//! Campaign probe: locate where a SID tune diverges from the sidplay oracle.
//! Renders the tune (via its constructor), captures the virtual-chip registers,
//! time-aligns each voice to the oracle CSV (same scale+shift as the fidelity
//! gate), then prints, PER VOICE: the divergence count per register class and a
//! handful of example diverging frames (oracle vs synth) so the "trouver le
//! lieu" step lands on a concrete (voice, frame, register, values) to take to
//! Ghidra. Usage: sid_oracle_probe <tune> [song_index] [secs]  (song i ↔ <tune>#i.csv)
use std::fs;
use xmrs::tracker::import::sid::sid_module::SidModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const W: i32 = 8;

fn ctor(name: &str) -> Option<fn() -> SidModule> {
    Some(match name {
        "commando" => SidModule::get_sid_commando,
        "crazy_comets" => SidModule::get_sid_crazy_comets,
        "last_v8" => SidModule::get_sid_last_v8,
        "monty" => SidModule::get_sid_monty_on_the_run,
        "thing_on_a_spring" => SidModule::get_sid_thing_on_a_spring,
        "zoid" => SidModule::get_sid_zoid,
        "ace_2" => SidModule::get_sid_ace_2,
        "delta" => SidModule::get_sid_delta,
        "human_race" => SidModule::get_sid_human_race,
        "international_karate" => SidModule::get_sid_international_karate,
        "lightforce" => SidModule::get_sid_lightforce,
        "sanxion_song_1" => SidModule::get_sid_sanxion_song_1,
        "sanxion_song_2" => SidModule::get_sid_sanxion_song_2,
        "spellbound" => SidModule::get_sid_spellbound,
        "thrust" => SidModule::get_sid_thrust,
        _ => return None,
    })
}

fn load(name: &str) -> Vec<[u8; 25]> {
    use flate2::read::GzDecoder;
    use std::io::Read;
    // Fixtures are gzip-packed (`<name>.csv.gz`) — see tests/cycle_exact_pcm.rs.
    let p = format!(
        "{}/tests/fixtures_sid_oracle/{name}.csv.gz",
        env!("CARGO_MANIFEST_DIR")
    );
    let bytes = fs::read(&p).unwrap_or_else(|e| panic!("oracle {p}: {e}"));
    let mut text = String::new();
    GzDecoder::new(&bytes[..])
        .read_to_string(&mut text)
        .unwrap_or_else(|e| panic!("oracle {p} (gunzip): {e}"));
    text.lines()
        .skip(1)
        .map(|l| {
            let mut a = [0u8; 25];
            for (i, x) in l.split(',').skip(1).enumerate().take(25) {
                a[i] = u8::from_str_radix(x.trim(), 16).unwrap_or(0);
            }
            a
        })
        .collect()
}

// The frame-alignment loops index the oracle by frame number `i` while ALSO
// mapping it to a synth frame `j = round(i·scale)`; the index is a time
// coordinate driving arithmetic, not a plain array cursor, so a range loop is
// the clear form here.
#[allow(clippy::needless_range_loop)]
fn main() {
    let name = std::env::args().nth(1).unwrap_or_else(|| "zoid".into());
    let song: usize = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let secs: usize = std::env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(16);
    let ctor = ctor(&name).expect("unknown tune");
    let key = format!("{name}#{song}");

    let modules = ctor().to_modules();
    let module = &modules[song];
    xmrs::generators::sid::driver::capture_begin();
    let mut player = XmrsPlayer::new(module, 8000, 0);
    player.set_max_loop_count(1);
    for _ in player.take(secs * 8000 * 2) {}
    let syn = xmrs::generators::sid::driver::capture_take();
    let ora = load(&key);
    let (o, s) = (ora.len(), syn.len());
    let scale = s as f64 / o as f64;
    println!("tune={key} oracle_frames={o} synth_frames={s} scale={scale:.4}");

    // Optional 4th arg = voice index → dump gate+AD+SR frame-by-frame (0..60).
    if let Some(vv) = std::env::args()
        .nth(4)
        .and_then(|x| x.parse::<usize>().ok())
    {
        let b = vv * 7;
        println!("\n-- voice {vv} trace: f | O gate ad sr | S gate ad sr --");
        for i in 0..60.min(o).min(s) {
            println!(
                "{i:2} | {} {:02x} {:02x} | {} {:02x} {:02x}{}",
                ora[i][4 + b] & 1,
                ora[i][5 + b],
                ora[i][6 + b],
                syn[i][4 + b] & 1,
                syn[i][5 + b],
                syn[i][6 + b],
                if ora[i][5 + b] != syn[i][5 + b] {
                    "  <"
                } else {
                    ""
                }
            );
        }
    }

    // Per-voice best shift (gate+pitch) — the SAME alignment the gate uses; the
    // split MUST be computed on aligned frames or a capture-offset tune (e.g.
    // thrust, shift -1) shows phantom gate-ON diffs.
    let best_shift = |b: usize| -> i32 {
        let (c, fhi) = (4 + b, 1 + b);
        let (mut bsh, mut bsc) = (0i32, f64::INFINITY);
        for sh in -W..=W {
            let (mut bad, mut val) = (0i64, 0i64);
            for i in 0..o {
                let j = (i as f64 * scale).round() as i32 + sh;
                if j >= 0 && (j as usize) < s {
                    val += 1;
                    let j = j as usize;
                    bad += ((syn[j][c] & 1) != (ora[i][c] & 1)) as i64;
                    bad += ((syn[j][fhi] as i32 - ora[i][fhi] as i32).abs() > 8) as i64;
                }
            }
            if val > 0 {
                let sc = bad as f64 / val as f64;
                if sc < bsc {
                    bsc = sc;
                    bsh = sh;
                }
            }
        }
        bsh
    };

    // Split AD/SR divergence (ALIGNED) by ORACLE gate state: gate-ON diffs = real
    // envelope bug; gate-OFF diffs = idle-register residue (inaudible, accepted).
    println!(
        "\n-- AD/SR divergence (aligned) split by oracle gate (ON=real bug, OFF=idle residue) --"
    );
    for v in 0..3 {
        let b = v * 7;
        let sh = best_shift(b);
        let (mut on_ad, mut off_ad, mut on_sr, mut off_sr) = (0, 0, 0, 0);
        for i in 0..o {
            let j = (i as f64 * scale).round() as i32 + sh;
            if j < 0 || (j as usize) >= s {
                continue;
            }
            let j = j as usize;
            let gon = ora[i][4 + b] & 1 == 1;
            let ad = ora[i][5 + b] != syn[j][5 + b];
            let sr = ora[i][6 + b] != syn[j][6 + b];
            match (gon, ad) {
                (true, true) => on_ad += 1,
                (false, true) => off_ad += 1,
                _ => {}
            }
            match (gon, sr) {
                (true, true) => on_sr += 1,
                (false, true) => off_sr += 1,
                _ => {}
            }
        }
        println!("  v{v}: shift={sh}  AD diff gateON={on_ad} gateOFF={off_ad} | SR gateON={on_sr} gateOFF={off_sr}");
    }

    let names = ["freqlo", "freqhi", "pwlo", "pwhi", "ctrl", "ad", "sr"];
    for v in 0..3 {
        let base = v * 7;
        let (c, fhi) = (4 + base, 1 + base);
        // best shift on gate+pitch
        let (mut bsh, mut bsc) = (0i32, f64::INFINITY);
        for sh in -W..=W {
            let (mut bad, mut val) = (0i64, 0i64);
            for i in 0..o {
                let j = (i as f64 * scale).round() as i32 + sh;
                if j >= 0 && (j as usize) < s {
                    val += 1;
                    let j = j as usize;
                    bad += ((syn[j][c] & 1) != (ora[i][c] & 1)) as i64;
                    bad += ((syn[j][fhi] as i32 - ora[i][fhi] as i32).abs() > 8) as i64;
                }
            }
            if val > 0 {
                let sc = bad as f64 / val as f64;
                if sc < bsc {
                    bsc = sc;
                    bsh = sh;
                }
            }
        }
        // per-register counts + examples
        let mut cnt = [0u32; 7];
        let mut ex: Vec<String> = Vec::new();
        for i in 0..o {
            let j = (i as f64 * scale).round() as i32 + bsh;
            if j < 0 || (j as usize) >= s {
                continue;
            }
            let j = j as usize;
            for r in 0..7 {
                let (ov, sv) = (ora[i][base + r], syn[j][base + r]);
                let diff = if r == 1 {
                    (ov as i32 - sv as i32).abs() > 8
                } else if r == 4 {
                    (ov & 1) != (sv & 1)
                } else {
                    ov != sv
                };
                if diff {
                    cnt[r] += 1;
                    if ex.len() < 8 && (r == 1 || r == 4 || r == 5 || r == 6) {
                        ex.push(format!(
                            "  f{i}(syn{j}) {}: oracle={ov:02x} synth={sv:02x}",
                            names[r]
                        ));
                    }
                }
            }
        }
        println!(
            "\nvoice {v}  shift={bsh}{}  counts: {}",
            if bsh.abs() == W { " [SATURATED]" } else { "" },
            (0..7)
                .map(|r| format!("{}={}", names[r], cnt[r]))
                .collect::<Vec<_>>()
                .join(" ")
        );
        for e in &ex {
            println!("{e}");
        }
    }
}
