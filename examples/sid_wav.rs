//! Rend un tune SID en WAV mono 16-bit (pour écoute / A-B vs l'oracle sidplay).
//! Usage: sid_wav <tune> <out.wav> <secs> <song>
use std::env;
use std::fs::File;
use std::io::Write;
use xmrs::tracker::import::sid::sid_module::SidModule;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const RATE: u32 = 44100;

fn main() {
    let tune = env::args().nth(1).unwrap_or_else(|| "spellbound".into());
    let out = env::args().nth(2).unwrap_or_else(|| "/tmp/sid.wav".into());
    let secs: usize = env::args()
        .nth(3)
        .and_then(|s| s.parse().ok())
        .unwrap_or(30);
    let song: usize = env::args().nth(4).and_then(|s| s.parse().ok()).unwrap_or(0);
    let sid = match tune.as_str() {
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
    let nsamp = secs * RATE as usize;
    let mut pcm: Vec<u8> = Vec::with_capacity(nsamp * 2);
    let mut it = player;
    let mut taken = 0;
    while taken < nsamp {
        match (it.next(), it.next()) {
            (Some(l), Some(r)) => {
                let m = ((l as i32 + r as i32) / 2) as i16;
                pcm.extend_from_slice(&m.to_le_bytes());
                taken += 1;
            }
            _ => break,
        }
    }
    let data_len = pcm.len() as u32;
    let mut f = File::create(&out).unwrap();
    f.write_all(b"RIFF").unwrap();
    f.write_all(&(36 + data_len).to_le_bytes()).unwrap();
    f.write_all(b"WAVEfmt ").unwrap();
    f.write_all(&16u32.to_le_bytes()).unwrap();
    f.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
    f.write_all(&1u16.to_le_bytes()).unwrap(); // mono
    f.write_all(&RATE.to_le_bytes()).unwrap();
    f.write_all(&(RATE * 2).to_le_bytes()).unwrap();
    f.write_all(&2u16.to_le_bytes()).unwrap();
    f.write_all(&16u16.to_le_bytes()).unwrap();
    f.write_all(b"data").unwrap();
    f.write_all(&data_len.to_le_bytes()).unwrap();
    f.write_all(&pcm).unwrap();
    eprintln!("wrote {} ({} samples) to {}", tune, taken, out);
}
