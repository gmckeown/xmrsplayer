//! Tier-2 fidelity oracle: compare our render of a module (XM / MOD /
//! S3M / IT — `Module::load` autodetect) against a reference render
//! (libopenmpt / `openmpt123 --render`).
//!
//! We can **never** match bit-for-bit: OpenMPT and xmrsplayer differ in
//! interpolation, volume ramping, pan law and summation order. So this
//! does not test equality — it measures a *bounded structural distance*
//! that is robust to phase and to a constant level offset, yet sensitive
//! to the things that mean "wrong playback": wrong notes, wrong timing,
//! wrong dynamics, missing channels.
//!
//! Three numbers, all phase-robust:
//!   * `env_corr`  Pearson correlation of the short-time log-energy
//!     envelopes (best of a small lag search). 1.0 = same
//!     dynamics at the same times. Catches dropped notes,
//!     wrong timing, dead channels.
//!   * `spec_sim`  mean per-frame cosine similarity of magnitude spectra
//!     (log-binned). 1.0 = same pitches/timbre. Catches wrong
//!     notes / wrong samples / wrong tuning.
//!   * `level_db`  20·log10(rms_dut / rms_ref). A constant offset is
//!     expected and harmless; large values flag a gain bug.
//!
//! The reference WAV is produced out-of-band (so no third-party code is
//! linked into the test), e.g.:
//!   openmpt123 --render --samplerate 48000 --channels 2 \
//!       --no-interpolation --no-ramping --gain 0 <in.it>   # writes <in.it>.wav
//! then:
//!   cargo run --example it_oracle_diff --features import_it -- \
//!       <in.it> <in.it.wav> [seconds] [rate]
//!
//! Exit code is 0 always; the caller (BASELINE capture) reads the printed
//! metrics. A pass/fail threshold lives in the plan, not here.

use xmrsplayer::xmrsplayer::XmrsPlayer;

const FRAME: usize = 1024;
const HOP: usize = 512;
const SPEC_BINS: usize = 24; // log-spaced magnitude bands
const MAX_LAG_FRAMES: i64 = 8; // ±8 hops ≈ ±85 ms latency tolerance

/// Render the module under test, in-process, to mono f32 at `rate`.
///
/// The player now applies the module's `mix_volume` natively in its final
/// gain stage (matching libopenmpt's master scaling), so no oracle-side
/// level compensation is needed — the residual `level_db` is a genuine
/// fidelity signal.
fn render_dut(path: &str, rate: u32, frames: usize) -> Vec<f32> {
    let bytes = std::fs::read(path).expect("read module");
    // Format-general: autodetect XM / MOD / S3M / IT so the same oracle
    // validates every importer's `mix_volume` against openmpt123.
    let module = xmrs::core::module::Module::load(&bytes).expect("Module::load");
    let mut player = XmrsPlayer::new(&module, rate, 0);
    let mut out = Vec::with_capacity(frames);
    for _ in 0..frames {
        match player.sample(true) {
            Some((l, r)) => out.push((l as f32 + r as f32) * 0.5),
            None => break,
        }
    }
    out.resize(frames, 0.0);
    out
}

/// Read a headerless interleaved s16le stream (`openmpt123 --render
/// --output-type raw --no-float`) as mono f32. Rate/channels can't be
/// inferred from a raw stream, so they are passed in.
fn read_raw_mono(path: &str, rate: u32, channels: usize) -> (u32, Vec<f32>) {
    let b = std::fs::read(path).expect("read ref raw");
    let ch = channels.max(1);
    let mut out = Vec::with_capacity(b.len() / (2 * ch));
    let mut j = 0;
    while j + 2 * ch <= b.len() {
        let mut acc = 0f32;
        for c in 0..ch {
            acc += i16::from_le_bytes([b[j + 2 * c], b[j + 2 * c + 1]]) as f32;
        }
        out.push(acc / ch as f32);
        j += 2 * ch;
    }
    (rate, out)
}

/// Minimal 16-bit PCM WAV reader → mono f32. Tolerates extra chunks and
/// mono/stereo; assumes 16-bit PCM (what openmpt123 --render emits by
/// default).
fn read_wav_mono(path: &str) -> (u32, Vec<f32>) {
    let b = std::fs::read(path).expect("read ref wav");
    assert!(
        &b[0..4] == b"RIFF" && &b[8..12] == b"WAVE",
        "not a RIFF/WAVE"
    );
    let (mut channels, mut rate, mut bits) = (2u16, 0u32, 16u16);
    let mut data: &[u8] = &[];
    let mut i = 12;
    while i + 8 <= b.len() {
        let id = &b[i..i + 4];
        let sz = u32::from_le_bytes([b[i + 4], b[i + 5], b[i + 6], b[i + 7]]) as usize;
        let body = &b[i + 8..(i + 8 + sz).min(b.len())];
        if id == b"fmt " {
            channels = u16::from_le_bytes([body[2], body[3]]);
            rate = u32::from_le_bytes([body[4], body[5], body[6], body[7]]);
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            data = body;
        }
        i += 8 + sz + (sz & 1); // chunks are word-aligned
    }
    assert_eq!(bits, 16, "only 16-bit PCM reference supported");
    let ch = channels.max(1) as usize;
    let mut out = Vec::with_capacity(data.len() / (2 * ch));
    let mut j = 0;
    while j + 2 * ch <= data.len() {
        let mut acc = 0f32;
        for c in 0..ch {
            let s = i16::from_le_bytes([data[j + 2 * c], data[j + 2 * c + 1]]);
            acc += s as f32;
        }
        out.push(acc / ch as f32);
        j += 2 * ch;
    }
    (rate, out)
}

fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

/// Short-time log-energy envelope (one value per hop).
fn log_energy_envelope(x: &[f32]) -> Vec<f32> {
    let mut env = Vec::new();
    let mut k = 0;
    while k + FRAME <= x.len() {
        let e = rms(&x[k..k + FRAME]);
        env.push((e + 1e-9).ln());
        k += HOP;
    }
    env
}

fn pearson(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    if n < 2 {
        return 0.0;
    }
    let (a, b) = (&a[..n], &b[..n]);
    let ma = a.iter().sum::<f32>() / n as f32;
    let mb = b.iter().sum::<f32>() / n as f32;
    let mut num = 0f32;
    let mut da = 0f32;
    let mut db = 0f32;
    for i in 0..n {
        let (x, y) = (a[i] - ma, b[i] - mb);
        num += x * y;
        da += x * x;
        db += y * y;
    }
    if da <= 0.0 || db <= 0.0 {
        return 0.0;
    }
    num / (da.sqrt() * db.sqrt())
}

/// Best envelope correlation over a small integer lag search (absorbs the
/// few-ms latency difference between the two renderers).
fn env_corr_best(dut: &[f32], reference: &[f32]) -> f32 {
    let mut best = -1f32;
    for lag in -MAX_LAG_FRAMES..=MAX_LAG_FRAMES {
        let c = if lag >= 0 {
            pearson(&dut[lag as usize..], reference)
        } else {
            pearson(dut, &reference[(-lag) as usize..])
        };
        if c > best {
            best = c;
        }
    }
    best
}

/// In-place iterative radix-2 Cooley–Tukey FFT (real input via complex
/// buffers). `re`/`im` length must be a power of two.
fn fft(re: &mut [f32], im: &mut [f32]) {
    let n = re.len();
    // bit-reversal permutation
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
        let ang = -2.0 * core::f32::consts::PI / len as f32;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1f32, 0f32);
            for k in 0..len / 2 {
                let a = i + k;
                let b = i + k + len / 2;
                let tr = re[b] * cr - im[b] * ci;
                let ti = re[b] * ci + im[b] * cr;
                re[b] = re[a] - tr;
                im[b] = im[a] - ti;
                re[a] += tr;
                im[a] += ti;
                let ncr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = ncr;
            }
            i += len;
        }
        len <<= 1;
    }
}

/// Per-frame log-binned magnitude spectrum, Hann-windowed.
fn spectra(x: &[f32]) -> Vec<[f32; SPEC_BINS]> {
    let mut frames = Vec::new();
    let half = FRAME / 2;
    // log-spaced bin edges over [1, half)
    let edges: Vec<usize> = (0..=SPEC_BINS)
        .map(|i| {
            let t = i as f32 / SPEC_BINS as f32;
            ((half as f32).powf(t)).round() as usize
        })
        .collect();
    let mut k = 0;
    while k + FRAME <= x.len() {
        let mut re = vec![0f32; FRAME];
        let mut im = vec![0f32; FRAME];
        for n in 0..FRAME {
            // Hann window
            let w = 0.5 - 0.5 * (2.0 * core::f32::consts::PI * n as f32 / FRAME as f32).cos();
            re[n] = x[k + n] * w;
        }
        fft(&mut re, &mut im);
        let mut bands = [0f32; SPEC_BINS];
        for (bi, band) in bands.iter_mut().enumerate() {
            let (lo, hi) = (edges[bi].max(1), edges[bi + 1].max(edges[bi] + 1).min(half));
            let mut acc = 0f32;
            for bin in lo..hi {
                acc += (re[bin] * re[bin] + im[bin] * im[bin]).sqrt();
            }
            *band = acc / (hi - lo).max(1) as f32;
        }
        frames.push(bands);
        k += HOP;
    }
    frames
}

fn cosine(a: &[f32; SPEC_BINS], b: &[f32; SPEC_BINS]) -> f32 {
    let mut dot = 0f32;
    let mut na = 0f32;
    let mut nb = 0f32;
    for i in 0..SPEC_BINS {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0; // both silent → treat as neutral, skip in mean
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn main() {
    let mut args = std::env::args().skip(1);
    let it = args
        .next()
        .expect("usage: <in.it> <ref.wav> [seconds] [rate]");
    let refwav = args
        .next()
        .expect("usage: <in.it> <ref.wav> [seconds] [rate]");
    let seconds: f64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30.0);
    let rate: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(48_000);

    // `.raw` = headerless s16le stereo at `rate` (openmpt123 raw output,
    // since the WAV writer needs libsndfile which we don't link). `.wav`
    // = self-describing.
    let (rrate, reference) = if refwav.to_ascii_lowercase().ends_with(".raw") {
        read_raw_mono(&refwav, rate, 2)
    } else {
        read_wav_mono(&refwav)
    };
    assert_eq!(rrate, rate, "ref rate {} != requested {}", rrate, rate);
    let frames = (seconds * rate as f64) as usize;
    let dut = render_dut(&it, rate, frames);
    let n = dut.len().min(reference.len());
    let (dut, reference) = (&dut[..n], &reference[..n]);

    // Level
    let (rd, rr) = (rms(dut), rms(reference));
    let level_db = if rr > 0.0 && rd > 0.0 {
        20.0 * (rd / rr).log10()
    } else {
        f32::NAN
    };

    // Envelope correlation
    let env_corr = env_corr_best(&log_energy_envelope(dut), &log_energy_envelope(reference));

    // Spectral similarity (mean cosine over frames where at least one
    // side has energy)
    let sd = spectra(dut);
    let sr = spectra(reference);
    let fcount = sd.len().min(sr.len());
    let mut ssum = 0f32;
    let mut scnt = 0usize;
    for i in 0..fcount {
        let energetic = sd[i].iter().sum::<f32>() > 1.0 || sr[i].iter().sum::<f32>() > 1.0;
        if energetic {
            ssum += cosine(&sd[i], &sr[i]);
            scnt += 1;
        }
    }
    let spec_sim = if scnt > 0 {
        ssum / scnt as f32
    } else {
        f32::NAN
    };

    // Per-third level, to tell a *constant* per-module gain offset (same
    // in every window) from a *growing* one (e.g. NNA voices ringing on
    // that the reference cuts — a real playback bug).
    let third = n / 3;
    let level_window = |a: usize, b: usize| {
        let rd = rms(&dut[a..b.min(dut.len())]);
        let rr = rms(&reference[a..b.min(reference.len())]);
        if rr > 0.0 && rd > 0.0 {
            20.0 * (rd / rr).log10()
        } else {
            f32::NAN
        }
    };
    let (l0, l1, l2) = (
        level_window(0, third),
        level_window(third, 2 * third),
        level_window(2 * third, n),
    );

    let name = std::path::Path::new(&it)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("?");
    println!(
        "{:<40} env_corr={:+.3}  spec_sim={:.3}  level_db={:+.2} [{:+.1}/{:+.1}/{:+.1}]  frames={}",
        name, env_corr, spec_sim, level_db, l0, l1, l2, n
    );
}
