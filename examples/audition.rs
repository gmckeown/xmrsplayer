//! Audition one instrument at a given semitone, report peak/rms.
//! Usage: audition <module> <instr_index> <semitone>
use xmrs::core::module::Module;
use xmrs::core::pitch::Pitch;
use xmrsplayer::sample_preview::render_instrument;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let bytes = std::fs::read(&a[1]).unwrap();
    let mut module = Module::load(&bytes).unwrap();
    let instr: usize = a[2].parse().unwrap();
    let semi: u8 = a[3].parse().unwrap();
    let rate = 44100u32;
    let pcm = render_instrument(
        &mut module,
        instr,
        Pitch::try_from(semi).unwrap(),
        rate,
        rate as usize * 2,
    );
    let peak = pcm
        .iter()
        .map(|(l, r)| l.unsigned_abs().max(r.unsigned_abs()))
        .max()
        .unwrap_or(0);
    let n = pcm.len().max(1);
    let energy: f64 = pcm
        .iter()
        .map(|(l, _)| (*l as f64) * (*l as f64))
        .sum::<f64>()
        / n as f64;
    let name = match &module.instrument.get(instr).map(|i| i.name.clone()) {
        Some(s) => s.clone(),
        None => "?".into(),
    };
    println!(
        "instr[{instr}] '{name}' semi={semi} -> {} frames, peak={peak}, rms={:.0}",
        pcm.len(),
        energy.sqrt()
    );
}
