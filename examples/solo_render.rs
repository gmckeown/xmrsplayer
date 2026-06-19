//! Render one soloed channel from a given order position to WAV, for
//! diagnosing per-note playback (pitch / porta / filter) in isolation.
//! Usage: solo_render <module> <out.wav> <channel_1based> <order_pos> [seconds] [rate]
use std::io::Write;
use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let out = &a[2];
    let ch: usize = a[3].parse().unwrap();
    let pos: usize = a[4].parse().unwrap();
    let secs: f64 = a.get(5).and_then(|s| s.parse().ok()).unwrap_or(8.0);
    let rate: u32 = a.get(6).and_then(|s| s.parse().ok()).unwrap_or(44100);
    let bytes = std::fs::read(path).unwrap();
    let module = Module::load(&bytes).unwrap();
    let mut player = XmrsPlayer::new(&module, rate, 0);
    let row0: usize = std::env::var("XMRS_ROW")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    if pos > 0 || row0 > 0 {
        player.goto(pos, row0, 0);
    }
    let invert = std::env::var("INV").is_ok();
    if ch > 0 {
        if invert {
            // mute ONLY this channel, keep the rest
            player.set_mute_channel(ch - 1, true);
        } else {
            player.mute_all(true);
            player.set_mute_channel(ch - 1, false);
        }
    }
    eprintln!("snapshots={}", player.channel_snapshots().len());
    let n = (secs * rate as f64) as usize;
    let mut s = Vec::with_capacity(n);
    for _ in 0..n {
        s.push(player.sample(true).unwrap_or((0, 0)));
    }
    let data = (s.len() * 4) as u32;
    let mut f = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
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
    eprintln!("wrote {} frames -> {}", s.len(), out);
}
