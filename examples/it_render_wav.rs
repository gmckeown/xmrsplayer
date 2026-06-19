//! Render an `.it` (or any importable module) through `XmrsPlayer` to a
//! 16-bit stereo WAV file. This is the *device-under-test* side of the
//! IT fidelity oracle: the reference side is `openmpt123 --render`, and
//! `it_oracle_diff.rs` compares the two.
//!
//! Usage:
//!   cargo run --example it_render_wav --features import_it -- <in.it> <out.wav> [seconds] [rate]
//!
//! Defaults: 30 seconds, 48000 Hz. The render is hard-capped at the
//! requested length (no loop following) so it always terminates and
//! aligns 1:1 with a same-length reference render.

use std::io::Write;
use xmrs::tracker::import::it::it_module::ItModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn write_wav_i16_stereo(path: &str, rate: u32, samples: &[(i16, i16)]) -> std::io::Result<()> {
    let n = samples.len();
    let data_bytes = (n * 2 * 2) as u32; // frames * 2 ch * 2 bytes
    let byte_rate = rate * 2 * 2;
    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);
    f.write_all(b"RIFF")?;
    f.write_all(&(36 + data_bytes).to_le_bytes())?;
    f.write_all(b"WAVE")?;
    f.write_all(b"fmt ")?;
    f.write_all(&16u32.to_le_bytes())?; // PCM fmt chunk size
    f.write_all(&1u16.to_le_bytes())?; // PCM
    f.write_all(&2u16.to_le_bytes())?; // channels
    f.write_all(&rate.to_le_bytes())?;
    f.write_all(&byte_rate.to_le_bytes())?;
    f.write_all(&4u16.to_le_bytes())?; // block align
    f.write_all(&16u16.to_le_bytes())?; // bits/sample
    f.write_all(b"data")?;
    f.write_all(&data_bytes.to_le_bytes())?;
    for (l, r) in samples {
        f.write_all(&l.to_le_bytes())?;
        f.write_all(&r.to_le_bytes())?;
    }
    f.flush()
}

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args
        .next()
        .expect("usage: <in.it> <out.wav> [seconds] [rate]");
    let output = args
        .next()
        .expect("usage: <in.it> <out.wav> [seconds] [rate]");
    let seconds: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let rate: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48_000);

    let bytes = std::fs::read(&input).expect("read input");
    let module = ItModule::load(&bytes).expect("ItModule::load").to_module();
    let mut player = XmrsPlayer::new(&module, rate, 0);

    let frames = (seconds * rate as f64) as usize;
    let mut out = Vec::with_capacity(frames);
    for _ in 0..frames {
        match player.sample(true) {
            Some(s) => out.push(s),
            None => break,
        }
    }
    // Pad to the exact requested length with silence so two reference
    // renders of the same nominal duration are bit-aligned.
    out.resize(frames, (0, 0));

    write_wav_i16_stereo(&output, rate, &out).expect("write wav");
    let peak = out
        .iter()
        .map(|(l, r)| l.unsigned_abs().max(r.unsigned_abs()))
        .max()
        .unwrap_or(0);
    eprintln!(
        "rendered {} frames ({:.1}s @ {} Hz) -> {}  peak={}",
        out.len(),
        seconds,
        rate,
        output,
        peak
    );
}
