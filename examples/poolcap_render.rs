//! Render full mix continuously with a configurable voice-pool capacity,
//! to test whether a quiet section is caused by NNA voice-pool exhaustion.
//! Usage: poolcap_render <module> <out.wav> <capacity> [seconds] [rate]
use std::io::Write;
use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let module = Module::load(&std::fs::read(&a[1]).unwrap()).unwrap();
    let cap: usize = a[3].parse().unwrap();
    let secs: f64 = a.get(4).and_then(|s| s.parse().ok()).unwrap_or(60.0);
    let rate: u32 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(44100);
    let mut player = XmrsPlayer::new_with_voice_pool_capacity(&module, rate, 0, cap);
    let n = (secs * rate as f64) as usize;
    let mut s = Vec::with_capacity(n);
    for _ in 0..n {
        s.push(player.sample(true).unwrap_or((0, 0)));
    }
    let data = (s.len() * 4) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(&a[2]).unwrap());
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&rate.to_le_bytes()).unwrap();
    f.write_all(&(rate * 4).to_le_bytes()).unwrap();
    f.write_all(&4u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data.to_le_bytes()).unwrap();
    for (l, r) in &s {
        f.write_all(&l.to_le_bytes()).unwrap();
        f.write_all(&r.to_le_bytes()).unwrap();
    }
}
