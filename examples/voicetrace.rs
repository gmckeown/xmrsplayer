//! Trace allocated voice count over time with a given pool capacity.
//! Usage: voicetrace <module> <capacity> [seconds] [rate]
use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let module = Module::load(&std::fs::read(&a[1]).unwrap()).unwrap();
    let cap: usize = a[2].parse().unwrap();
    let secs: f64 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let rate: u32 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(44100);
    let mut player = XmrsPlayer::new_with_voice_pool_capacity(&module, rate, 0, cap);
    let n = (secs * rate as f64) as usize;
    let step = rate as usize / 2; // every 0.5s
    let mut peak = 0;
    for i in 0..n {
        let _ = player.sample(true);
        let c = player.allocated_voice_count();
        if c > peak {
            peak = c;
        }
        if i % step == 0 {
            println!(
                "{:5.1}s  voices={:4}  peak={:4}",
                i as f64 / rate as f64,
                c,
                peak
            );
        }
    }
}
