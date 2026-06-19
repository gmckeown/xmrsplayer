//! Fidelity probe: render a SID tune through the sidera synth path and compare
//! its average magnitude spectrum (cosine) + RMS to the `sidplay` WAV oracle
//! (the 6502 + SID engine). Usage: sid_cos <tune> <oracle.wav> [seconds]
//!
//! Use a LONG window (≥30 s). A short window (≤8 s) is unreliable: many tunes
//! open with a several-second single/2-voice intro before a third voice enters
//! (Delta's voice 1 starts at row 96), so an 8 s window captures only the intro
//! and reports spuriously low cosine / RMS for tunes that are actually faithful
//! (measured: sanxion 0.75→0.97, thrust 0.60→0.87, monty 0.92→0.99 going 8 s→30 s).
//! Default is 30 s for this reason.
//!
//! NB: IRQ-install PSIDs (play address 0, e.g. last_v8) cannot be driven by the
//! minimal `sidplay` player — their oracle WAV is a ~1 s init transient then
//! silence, so they have NO usable oracle here and must be skipped.
use std::env;
use std::fs;
use xmrs::tracker::import::sid::sid_module::SidModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const RATE: u32 = 44100;
const N: usize = 1024;

fn load_wav_mono(path: &str) -> Vec<f32> {
    let b = fs::read(path).unwrap();
    // minimal PCM16 mono WAV: data starts at 44
    let data = &b[44..];
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}

fn render_synth(tune: &str, secs: usize, song: usize) -> Vec<f32> {
    let sid = match tune {
        "monty" => SidModule::get_sid_monty_on_the_run(),
        "crazy_comets" => SidModule::get_sid_crazy_comets(),
        "last_v8" => SidModule::get_sid_last_v8(),
        "thing_on_a_spring" => SidModule::get_sid_thing_on_a_spring(),
        "delta" => SidModule::get_sid_delta(),
        "sanxion" => SidModule::get_sid_sanxion_song_1(),
        "lightforce" => SidModule::get_sid_lightforce(),
        "spellbound" => SidModule::get_sid_spellbound(),
        "ace_2" => SidModule::get_sid_ace_2(),
        "human_race" => SidModule::get_sid_human_race(),
        "international_karate" => SidModule::get_sid_international_karate(),
        "thrust" => SidModule::get_sid_thrust(),
        "zoids" => SidModule::get_sid_zoid(),
        _ => SidModule::get_sid_commando(),
    };
    let modules = sid.to_modules();
    let module = &modules[song.min(modules.len().saturating_sub(1))];
    let mut player = XmrsPlayer::new(module, RATE, 0);
    player.set_max_loop_count(1);
    let it = player;
    let nsamp = secs * RATE as usize;
    let mut mono = Vec::with_capacity(nsamp);
    let mut s = it.take(nsamp * 2);
    while let (Some(l), Some(r)) = (s.next(), s.next()) {
        mono.push((l as f32 + r as f32) / 65536.0);
    }
    mono
}

// iterative radix-2 FFT (in place), magnitude returned by caller
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    let mut j = 0;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j ^= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * std::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0f32, 0.0f32);
            for k in 0..len / 2 {
                let ur = re[i + k];
                let ui = im[i + k];
                let vr = re[i + k + len / 2] * cr - im[i + k + len / 2] * ci;
                let vi = re[i + k + len / 2] * ci + im[i + k + len / 2] * cr;
                re[i + k] = ur + vr;
                im[i + k] = ui + vi;
                re[i + k + len / 2] = ur - vr;
                im[i + k + len / 2] = ui - vi;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

fn avg_spectrum(sig: &[f32]) -> Vec<f32> {
    let mut acc = vec![0f32; N / 2];
    let mut frames = 0;
    let mut pos = 0;
    while pos + N <= sig.len() {
        let mut re: Vec<f32> = (0..N)
            .map(|i| {
                // Hann window
                let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / N as f32).cos();
                sig[pos + i] * w
            })
            .collect();
        let mut im = vec![0f32; N];
        fft(&mut re, &mut im);
        for k in 0..N / 2 {
            acc[k] += (re[k] * re[k] + im[k] * im[k]).sqrt();
        }
        frames += 1;
        pos += N / 2;
    }
    if frames > 0 {
        for v in &mut acc {
            *v /= frames as f32;
        }
    }
    acc
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let (mut dot, mut na, mut nb) = (0f32, 0f32, 0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

fn rms(s: &[f32]) -> f32 {
    (s.iter().map(|x| x * x).sum::<f32>() / s.len().max(1) as f32).sqrt()
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let tune = args[1].clone();
    let oracle_path = &args[2];
    let secs: usize = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(30);
    let song: usize = args.get(4).and_then(|s| s.parse().ok()).unwrap_or(0);

    let oracle = load_wav_mono(oracle_path);
    let synth = render_synth(&tune, secs, song);
    // Optional: dump the synth mono to a PCM16 WAV for offline tempo analysis.
    if let Ok(path) = env::var("SID_SYNTH_WAV") {
        let mut bytes = Vec::with_capacity(44 + synth.len() * 2);
        let n = synth.len() as u32;
        let data_len = n * 2;
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&(36 + data_len).to_le_bytes());
        bytes.extend_from_slice(b"WAVEfmt ");
        bytes.extend_from_slice(&16u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&(RATE).to_le_bytes());
        bytes.extend_from_slice(&(RATE * 2).to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&16u16.to_le_bytes());
        bytes.extend_from_slice(b"data");
        bytes.extend_from_slice(&data_len.to_le_bytes());
        for &x in &synth {
            bytes.extend_from_slice(&((x.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
        }
        std::fs::write(&path, &bytes).unwrap();
        eprintln!("wrote synth wav to {path}");
    }
    let n = oracle.len().min(synth.len());
    let oracle = &oracle[..n];
    let synth = &synth[..n];

    let so = avg_spectrum(oracle);
    let ss = avg_spectrum(synth);
    let cos = cosine(&so, &ss);
    println!(
        "frames {} | cosine(avg-spectrum) = {:.4} | rms oracle={:.4} synth={:.4} (ratio {:.2})",
        n,
        cos,
        rms(oracle),
        rms(synth),
        rms(synth) / rms(oracle).max(1e-9)
    );
    let peak = synth.iter().fold(0f32, |m, x| m.max(x.abs()));
    println!("synth peak = {:.3} (clip if >= 1.0)", peak);

    // RMS-envelope correlation: a more reliable fidelity measure than the
    // averaged-spectrum cosine for rhythmic / percussive content (which the
    // cosine penalises because broadband noise spectra de-correlate even when
    // the timing matches). Window the signal into ~23 ms frames, take RMS per
    // window, and Pearson-correlate the two envelopes.
    let win = 1024;
    let env = |s: &[f32]| -> Vec<f32> {
        s.chunks(win)
            .map(|c| (c.iter().map(|x| x * x).sum::<f32>() / c.len() as f32).sqrt())
            .collect()
    };
    let eo = env(oracle);
    let es = env(synth);
    let m = eo.len().min(es.len());
    let (eo, es) = (&eo[..m], &es[..m]);
    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len().max(1) as f32;
    let (mo, ms) = (mean(eo), mean(es));
    let (mut cov, mut vo, mut vs) = (0f32, 0f32, 0f32);
    for i in 0..m {
        cov += (eo[i] - mo) * (es[i] - ms);
        vo += (eo[i] - mo).powi(2);
        vs += (es[i] - ms).powi(2);
    }
    let corr = if vo > 0.0 && vs > 0.0 {
        cov / (vo.sqrt() * vs.sqrt())
    } else {
        0.0
    };
    println!("rms-envelope correlation = {:.4}", corr);
}
