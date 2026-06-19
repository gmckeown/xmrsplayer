//! Per-instrument OPL (FM) fidelity oracle.
//!
//! For every AdLib (SCRI / `InstrumentType::Opl`) instrument in an `.s3m`,
//! render a single isolated note at several pitches through **both**
//! `xmrsplayer` and **Schism Tracker**, then compare the onset spectra:
//! pitch error (cents), linear-magnitude spectral cosine (timbre), and a
//! bright/dull balance ratio. This isolates each FM instrument from the
//! full mix (where PCM voices mask OPL errors) so timbre regressions are
//! visible per instrument and per frequency.
//!
//! Usage:
//!   cargo run --release --example opl_compare -- <module.s3m> [notes...]
//!
//! `notes` are semitone indices (`C-0 = 0`, `C-4 = 48`) or names
//! (`A-0`, `C#3`, `C-5`); default spread is A-0/C-3/C-4/C-5/C-6.
//!
//! Schism binary: `$SCHISM_BIN`, else the local build path. The build must
//! support `--headless --diskwrite`. Without it, only xmrs pitch is shown.
//!
//! Reusable on any FM-bearing `.s3m`.

use std::f64::consts::PI;
use std::process::Command;

use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const RATE: u32 = 44100;
const RENDER_SECS: f64 = 0.45;
const WIN_START: f64 = 0.02; // skip the very first ms (key-on transient)
const WIN_LEN: f64 = 0.16;
const FFT_N: usize = 8192;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: opl_compare <module.s3m> [notes...]");
        std::process::exit(2);
    }
    let path = &args[1];
    let notes: Vec<u8> = if args.len() > 2 {
        args[2..].iter().filter_map(|s| parse_note(s)).collect()
    } else {
        vec![9, 36, 48, 60, 72] // A-0, C-3, C-4, C-5, C-6
    };
    let schism = std::env::var("SCHISM_BIN").unwrap_or_else(|_| {
        "/home/user/Documents/schismtracker-20251014/build_xmrs/schismtracker".into()
    });
    let have_schism = std::path::Path::new(&schism).exists();
    if !have_schism {
        eprintln!("warning: Schism not found at {schism} (set $SCHISM_BIN) — pitch only");
    }

    let src = std::fs::read(path).expect("read module");
    let opl = find_opl_instruments(&src);
    println!("module: {}   OPL instruments: {}\n", path, opl.len());
    println!(
        "{:<5} {:<22} {:>5}  {:>8} {:>9} {:>6}  {:>8} {:>6}  {:>11}",
        "instr", "name", "note", "xmrs_Hz", "schism_Hz", "cents", "spec_cos", "bright", "decay x/s"
    );
    println!("{}", "-".repeat(92));

    let mut worst: Vec<(f64, String)> = Vec::new();
    for (num, rec, name) in &opl {
        for &note in &notes {
            let s3m = build_minimal_s3m(rec, note);
            let xm = render_xmrs(&s3m);
            let xf = fundamental(&onset_mag(&xm));
            if have_schism {
                let sm = render_schism(&s3m, &schism);
                if sm.iter().any(|&v| v != 0.0) {
                    let (sf, cos, bright) = compare(&xm, &sm);
                    let cents = if sf > 0.0 && xf > 0.0 {
                        1200.0 * (xf / sf).log2()
                    } else {
                        f64::NAN
                    };
                    let (xd, sd) = (decay_db(&xm), decay_db(&sm));
                    println!(
                        "#{:<4} {:<22} {:>5}  {:8.1} {:9.1} {:+6.0}  {:8.3} {:6.2}  {:+5.1}/{:+5.1}",
                        num,
                        truncate(name, 22),
                        note_name(note),
                        xf,
                        sf,
                        cents,
                        cos,
                        bright,
                        xd,
                        sd
                    );
                    let badness = (cents.abs().min(1200.0)) / 100.0 + (1.0 - cos) * 5.0;
                    worst.push((
                        badness,
                        format!("#{num} {} {}", note_name(note), truncate(name, 20)),
                    ));
                    continue;
                }
            }
            println!(
                "#{:<4} {:<26} {:>5}  {:8.1} {:>9} {:>6}  {:>8} {:>6}",
                num,
                truncate(name, 26),
                note_name(note),
                xf,
                "-",
                "-",
                "-",
                "-"
            );
        }
    }

    if !worst.is_empty() {
        worst.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
        println!("\nworst (pitch-cents/100 + 5·(1−cos)):");
        for (b, label) in worst.iter().take(12) {
            println!("  {:5.2}  {}", b, label);
        }
    }
}

// ---- S3M parsing / building -------------------------------------------

/// Find every SCRI (AdLib) instrument: `(file-1-based index, 80-byte
/// record, name)`.
fn find_opl_instruments(d: &[u8]) -> Vec<(usize, Vec<u8>, String)> {
    let ordnum = u16::from_le_bytes([d[0x20], d[0x21]]) as usize;
    let insnum = u16::from_le_bytes([d[0x22], d[0x23]]) as usize;
    let ipp = 0x60 + ordnum;
    let mut out = Vec::new();
    for i in 0..insnum {
        let pp = u16::from_le_bytes([d[ipp + 2 * i], d[ipp + 2 * i + 1]]) as usize;
        let off = pp * 16;
        if off + 80 > d.len() {
            continue;
        }
        if d[off] == 2 {
            // type 2 = AdLib melody (SCRI)
            let rec = d[off..off + 80].to_vec();
            let name = String::from_utf8_lossy(&d[off + 0x30..off + 0x4c])
                .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string();
            out.push((i + 1, rec, name));
        }
    }
    out
}

/// Build a minimal valid S3M: one AdLib channel, the given instrument
/// record, one pattern playing `semitone` at row 0 and holding it.
fn build_minimal_s3m(rec: &[u8], semitone: u8) -> Vec<u8> {
    let mut hdr = vec![0u8; 96];
    hdr[0..7].copy_from_slice(b"OPLTEST");
    hdr[0x1c] = 0x1a;
    hdr[0x1d] = 16;
    hdr[0x20..0x22].copy_from_slice(&2u16.to_le_bytes()); // ordnum
    hdr[0x22..0x24].copy_from_slice(&1u16.to_le_bytes()); // insnum
    hdr[0x24..0x26].copy_from_slice(&1u16.to_le_bytes()); // patnum
    hdr[0x28..0x2a].copy_from_slice(&0x1320u16.to_le_bytes()); // cwtv
    hdr[0x2a..0x2c].copy_from_slice(&2u16.to_le_bytes()); // ffi = unsigned
    hdr[0x2c..0x30].copy_from_slice(b"SCRM");
    hdr[0x30] = 64; // global volume
    hdr[0x31] = 6; // speed
    hdr[0x32] = 94; // tempo
    hdr[0x33] = 192; // master volume
    for i in 0..32 {
        hdr[0x40 + i] = 0xff; // all channels disabled…
    }
    hdr[0x40] = 0x10; // …except channel 0 = AdLib melody voice 1

    let instr_para = 7usize; // 0x70
    let instr_off = instr_para * 16;
    let mut pat_off = instr_off + 80;
    if !pat_off.is_multiple_of(16) {
        pat_off += 16 - (pat_off % 16);
    }
    let pat_para = pat_off / 16;

    // S3M note byte: (octave << 4) | note-in-octave.
    let s3m_note = ((semitone / 12) << 4) | (semitone % 12);
    let mut pdata = vec![0x20u8, s3m_note, 1, 0x00]; // ch0: note+instr, instr #1, end row
    pdata.extend(std::iter::repeat_n(0u8, 63)); // 63 empty rows
    let mut pat = (pdata.len() as u16).to_le_bytes().to_vec();
    pat.extend_from_slice(&pdata);

    let mut f = hdr;
    f.extend_from_slice(&[0, 0xff]); // orders: [0, end]
    f.extend_from_slice(&(instr_para as u16).to_le_bytes());
    f.extend_from_slice(&(pat_para as u16).to_le_bytes());
    while f.len() < instr_off {
        f.push(0);
    }
    f.extend_from_slice(rec);
    while f.len() < pat_off {
        f.push(0);
    }
    f.extend_from_slice(&pat);
    f
}

// ---- rendering ---------------------------------------------------------

fn render_xmrs(s3m: &[u8]) -> Vec<f64> {
    let m = match Module::load(s3m) {
        Ok(m) => m,
        Err(_) => return vec![0.0; (RATE as f64 * RENDER_SECS) as usize],
    };
    let mut p = XmrsPlayer::new(&m, RATE, 0);
    let n = (RATE as f64 * RENDER_SECS) as usize;
    (0..n)
        .map(|_| {
            let (l, r) = p.sample(true).unwrap_or((0, 0));
            (l as f64 + r as f64) * 0.5
        })
        .collect()
}

fn render_schism(s3m: &[u8], bin: &str) -> Vec<f64> {
    let dir = std::env::temp_dir();
    let inp = dir.join("opl_compare_in.s3m");
    let outp = dir.join("opl_compare_out.wav");
    let _ = std::fs::write(&inp, s3m);
    let _ = std::fs::remove_file(&outp);
    let _ = Command::new(bin)
        .arg("--headless")
        .arg(format!("--diskwrite={}", outp.display()))
        .arg(&inp)
        .output();
    read_wav_mono(&outp)
}

fn read_wav_mono(path: &std::path::Path) -> Vec<f64> {
    let Ok(d) = std::fs::read(path) else {
        return Vec::new();
    };
    if d.len() < 44 || &d[0..4] != b"RIFF" || &d[8..12] != b"WAVE" {
        return Vec::new();
    }
    // Walk chunks for fmt (channels) and data.
    let mut ch = 1usize;
    let mut pos = 12;
    let mut data: Option<(usize, usize)> = None;
    while pos + 8 <= d.len() {
        let id = &d[pos..pos + 4];
        let sz = u32::from_le_bytes([d[pos + 4], d[pos + 5], d[pos + 6], d[pos + 7]]) as usize;
        let body = pos + 8;
        if id == b"fmt " && body + 2 <= d.len() {
            ch = u16::from_le_bytes([d[body + 2], d[body + 3]]).max(1) as usize;
        } else if id == b"data" {
            data = Some((body, sz.min(d.len().saturating_sub(body))));
        }
        pos = body + sz + (sz & 1);
    }
    let Some((start, len)) = data else {
        return Vec::new();
    };
    let want = (RATE as f64 * RENDER_SECS) as usize;
    let mut out = Vec::with_capacity(want);
    let mut i = start;
    let end = start + len;
    while i + 2 * ch <= end && out.len() < want {
        let mut acc = 0i32;
        for c in 0..ch {
            acc += i16::from_le_bytes([d[i + 2 * c], d[i + 2 * c + 1]]) as i32;
        }
        out.push(acc as f64 / ch as f64);
        i += 2 * ch;
    }
    out
}

// ---- analysis ----------------------------------------------------------

/// Hann-windowed, zero-padded onset magnitude spectrum (first FFT_N/2 bins).
fn onset_mag(a: &[f64]) -> Vec<f64> {
    let i0 = (RATE as f64 * WIN_START) as usize;
    let n = (RATE as f64 * WIN_LEN) as usize;
    let mut re = vec![0.0; FFT_N];
    let mut im = vec![0.0; FFT_N];
    for (k, re_k) in re.iter_mut().enumerate().take(n.min(FFT_N)) {
        let w = 0.5 - 0.5 * (2.0 * PI * k as f64 / n as f64).cos();
        *re_k = a.get(i0 + k).copied().unwrap_or(0.0) * w;
    }
    fft(&mut re, &mut im);
    (0..FFT_N / 2)
        .map(|k| (re[k] * re[k] + im[k] * im[k]).sqrt())
        .collect()
}

/// Robust fundamental: the **lowest** strong spectral peak, not the global
/// argmax. FM spectra often have a harmonic louder than the fundamental, so
/// argmax would report a harmonic and fabricate octave "pitch errors" that
/// are really timbre differences. We scan upward for the first local maximum
/// exceeding a fraction of the peak.
fn fundamental(mag: &[f64]) -> f64 {
    let binhz = RATE as f64 / FFT_N as f64;
    let lo = (15.0 / binhz) as usize; // ignore < 15 Hz / DC
    let peak = mag.iter().cloned().fold(0.0_f64, f64::max);
    if peak <= 0.0 {
        return 0.0;
    }
    let thresh = peak * 0.18;
    for k in lo.max(2)..mag.len() - 1 {
        if mag[k] >= thresh && mag[k] >= mag[k - 1] && mag[k] >= mag[k + 1] {
            // Parabolic interpolation around the peak for sub-bin accuracy.
            let (a, b, c) = (mag[k - 1], mag[k], mag[k + 1]);
            let denom = a - 2.0 * b + c;
            let delta = if denom.abs() > 1e-9 {
                0.5 * (a - c) / denom
            } else {
                0.0
            };
            return (k as f64 + delta) * binhz;
        }
    }
    // Fallback: global argmax.
    let mut bi = lo.max(1);
    for k in lo..mag.len() {
        if mag[k] > mag[bi] {
            bi = k;
        }
    }
    bi as f64 * binhz
}

/// Returns `(schism_fundamental_Hz, spectral_cosine, bright_ratio)`.
/// `bright_ratio` = (xmrs energy above 4× fundamental) / (schism's),
/// normalised so 1.0 = same brightness, >1 too bright, <1 too dull.
fn compare(x: &[f64], s: &[f64]) -> (f64, f64, f64) {
    let xm = onset_mag(x);
    let sm = onset_mag(s);
    let sf = fundamental(&sm);
    let binhz = RATE as f64 / FFT_N as f64;

    let dot: f64 = xm.iter().zip(&sm).map(|(a, b)| a * b).sum();
    let nx: f64 = xm.iter().map(|a| a * a).sum::<f64>().sqrt();
    let ns: f64 = sm.iter().map(|a| a * a).sum::<f64>().sqrt();
    let cos = if nx * ns > 0.0 { dot / (nx * ns) } else { 0.0 };

    let cut = ((4.0 * sf) / binhz) as usize;
    let share = |m: &[f64]| -> f64 {
        let tot: f64 = m.iter().sum();
        if tot <= 0.0 {
            return 0.0;
        }
        m.iter().skip(cut).sum::<f64>() / tot
    };
    let (xs, ss) = (share(&xm), share(&sm));
    let bright = if ss > 1e-9 { xs / ss } else { 0.0 };
    (sf, cos, bright)
}

/// Envelope decay: dB change from an early window (~0.05 s) to a late one
/// (~0.35 s) of the render. Negative = the note decays; ~0 = sustains.
/// Comparing xmrs vs Schism reveals envelope-rate (attack/decay) mismatch.
fn decay_db(a: &[f64]) -> f64 {
    let rms = |t0: f64, t1: f64| -> f64 {
        let (i0, i1) = ((t0 * RATE as f64) as usize, (t1 * RATE as f64) as usize);
        let seg = &a[i0.min(a.len())..i1.min(a.len())];
        if seg.is_empty() {
            return 0.0;
        }
        (seg.iter().map(|v| v * v).sum::<f64>() / seg.len() as f64).sqrt()
    };
    let early = rms(0.03, 0.08) + 1.0;
    let late = rms(0.30, 0.40) + 1.0;
    20.0 * (late / early).log10()
}

fn fft(re: &mut [f64], im: &mut [f64]) {
    let n = re.len();
    let mut j = 0usize;
    for i in 1..n {
        let mut bit = n >> 1;
        while j & bit != 0 {
            j ^= bit;
            bit >>= 1;
        }
        j |= bit;
        if i < j {
            re.swap(i, j);
            im.swap(i, j);
        }
    }
    let mut len = 2;
    while len <= n {
        let ang = -2.0 * PI / len as f64;
        let (wr, wi) = (ang.cos(), ang.sin());
        let mut i = 0;
        while i < n {
            let (mut cr, mut ci) = (1.0, 0.0);
            for k in 0..len / 2 {
                let a = i + k;
                let b = a + len / 2;
                let vr = re[b] * cr - im[b] * ci;
                let vi = re[b] * ci + im[b] * cr;
                re[b] = re[a] - vr;
                im[b] = im[a] - vi;
                re[a] += vr;
                im[a] += vi;
                let nr = cr * wr - ci * wi;
                ci = cr * wi + ci * wr;
                cr = nr;
            }
            i += len;
        }
        len <<= 1;
    }
}

// ---- note helpers ------------------------------------------------------

const NOTE_NAMES: [&str; 12] = [
    "C-", "C#", "D-", "D#", "E-", "F-", "F#", "G-", "G#", "A-", "A#", "B-",
];

fn note_name(sem: u8) -> String {
    format!("{}{}", NOTE_NAMES[(sem % 12) as usize], sem / 12)
}

fn parse_note(s: &str) -> Option<u8> {
    if let Ok(v) = s.parse::<u8>() {
        return Some(v.min(119));
    }
    let b = s.as_bytes();
    if b.len() < 2 {
        return None;
    }
    let base = match b[0].to_ascii_uppercase() {
        b'C' => 0,
        b'D' => 2,
        b'E' => 4,
        b'F' => 5,
        b'G' => 7,
        b'A' => 9,
        b'B' => 11,
        _ => return None,
    };
    let (acc, oct_idx) = match b[1] {
        b'#' => (1i32, 2),
        b'b' => (-1, 2),
        b'-' => (0, 2),
        _ => (0, 1),
    };
    let oct: i32 = s[oct_idx..].parse().ok()?;
    let v = oct * 12 + base + acc;
    if (0..=119).contains(&v) {
        Some(v as u8)
    } else {
        None
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}
