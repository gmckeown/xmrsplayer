//! throwaway: windowed spectral-cosine of the sidera synth vs a sidplay WAV
//! oracle, per 1-second window, to see WHERE (in time) the audio diverges.
//! Usage: sid_winco <tune> <oracle.wav> <secs> <songidx>
use std::env;
use std::fs;
use xmrs::tracker::import::sid::sid_module::SidModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const RATE: u32 = 44100;
const N: usize = 1024;

fn load_wav_mono(path: &str) -> Vec<f32> {
    let b = fs::read(path).unwrap();
    let data = &b[44..];
    data.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
        .collect()
}

fn render_synth(tune: &str, secs: usize, song: usize) -> Vec<f32> {
    let sid = match tune {
        "spellbound" => SidModule::get_sid_spellbound(),
        "delta" => SidModule::get_sid_delta(),
        "monty" => SidModule::get_sid_monty_on_the_run(),
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

/// average magnitude spectrum over a sample window
fn avg_spectrum(x: &[f32]) -> Vec<f32> {
    let mut acc = vec![0.0f32; N / 2];
    let mut frames = 0;
    let mut i = 0;
    while i + N <= x.len() {
        let mut re: Vec<f32> = x[i..i + N].to_vec();
        let mut im = vec![0.0f32; N];
        // Hann
        for k in 0..N {
            let w = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * k as f32 / N as f32).cos();
            re[k] *= w;
        }
        fft(&mut re, &mut im);
        for k in 0..N / 2 {
            acc[k] += (re[k] * re[k] + im[k] * im[k]).sqrt();
        }
        frames += 1;
        i += N;
    }
    if frames > 0 {
        for v in &mut acc {
            *v /= frames as f32;
        }
    }
    acc
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let mut dot = 0.0;
    let mut na = 0.0;
    let mut nb = 0.0;
    for k in 0..a.len() {
        dot += a[k] * b[k];
        na += a[k] * a[k];
        nb += b[k] * b[k];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

fn main() {
    let tune = env::args().nth(1).unwrap_or_else(|| "spellbound".into());
    let oracle = env::args().nth(2).expect("oracle wav");
    let secs: usize = env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let song: usize = env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(0);

    let syn = render_synth(&tune, secs, song);
    let orc = load_wav_mono(&oracle);
    let win = RATE as usize; // 1 s
    let rms = |x: &[f32]| -> f32 {
        if x.is_empty() {
            return 0.0;
        }
        (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
    };
    let rms_syn_all = rms(&syn);
    let rms_orc_all = rms(&orc);
    println!(
        "GLOBAL rms: synth={:.4} oracle={:.4} ratio={:.3}  (synth {} samp, oracle {} samp)",
        rms_syn_all,
        rms_orc_all,
        rms_syn_all / rms_orc_all.max(1e-9),
        syn.len(),
        orc.len()
    );
    println!(
        "win  cosine  rmsS    rmsO    S/O    {}",
        "(S/O<<1 = chunk manquant)"
    );
    let nwin = (syn.len().min(orc.len())) / win;
    for w in 0..nwin {
        let s = &syn[w * win..(w + 1) * win];
        let o = &orc[w * win..(w + 1) * win];
        let a = avg_spectrum(s);
        let b = avg_spectrum(o);
        let (rs, ro) = (rms(s), rms(o));
        let flag = if ro > 1e-4 && rs / ro < 0.5 {
            "  <-- TROU"
        } else {
            ""
        };
        println!(
            "{:3}  {:.4}  {:.4}  {:.4}  {:.2}{}",
            w,
            cosine(&a, &b),
            rs,
            ro,
            rs / ro.max(1e-9),
            flag
        );
    }
}
