use clap::Parser;
use console::{Key, Term};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::{Arc, Mutex};

use xmrs::prelude::*;
use xmrsplayer::prelude::*;

#[cfg(feature = "import_sid")]
use xmrs::import::sid::sid_module::SidModule;

#[derive(Parser)]
struct Cli {
    /// Choose XM or XmRs File
    #[arg(short = 'f', long, required = true, value_name = "filename")]
    filename: Option<String>,

    /// song number (default: 0)
    #[arg(short = 's', long, default_value = "0")]
    song: usize,

    /// Choose output wave file
    #[arg(short = 'o', long, value_name = "output filename")]
    output: Option<String>,

    /// Choose amplification (1.0 = unity after volume-chain fix; lower
    /// to avoid clipping on modules with many simultaneous channels)
    #[arg(short = 'a', long, default_value = "0.25")]
    amplification: f32,

    /// Play only a specific channel (from 1 to n, 0 for all)
    #[arg(short = 'c', long, default_value = "0")]
    ch: u8,

    /// Turn debugging information on
    #[arg(short = 'd', long, default_value = "false")]
    debug: bool,

    /// How many loop (default: infinity)
    #[arg(short = 'l', long, default_value = "0")]
    loops: usize,

    /// Start at a specific pattern order table position
    #[arg(short = 'p', long, default_value = "0")]
    position: usize,

    /// Force speed
    #[arg(short = 'e', long, default_value = "0")]
    speed: usize,

    /// Test SID player as a Proof of Concept
    #[cfg(feature = "import_sid")]
    #[arg(short = 'z', long, default_value = "false")]
    sid_test_player: bool,
}

#[cfg(feature = "import_sid")]
fn sid_test_player(cli: &Cli) {
    // let sidmodule = SidModule::get_sid_commando();
    // let sidmodule = SidModule::get_sid_crazy_comets();
    let sidmodule = SidModule::get_sid_monty_on_the_run();
    // let sidmodule = SidModule::get_sid_last_v8();
    // let sidmodule = SidModule::get_sid_thing_on_a_spring();
    // let sidmodule = SidModule::get_sid_zoid();
    // let sidmodule = SidModule::get_sid_ace_2();
    // let sidmodule = SidModule::get_sid_delta();
    // let sidmodule = SidModule::get_sid_human_race();
    // let sidmodule = SidModule::get_sid_international_karate();
    // let sidmodule = SidModule::get_sid_lightforce();
    // let sidmodule = SidModule::get_sid_sanxion_song_1();
    // let sidmodule = SidModule::get_sid_sanxion_song_2();
    // let sidmodule = SidModule::get_sid_spellbound();

    let modules = sidmodule.to_modules(false);

    let leaked_modules: &'static [Module] = Box::leak(modules.into_boxed_slice());
    let module_ref: &'static Module = &leaked_modules[0];

    play_music(
        module_ref,
        cli.song,
        cli.amplification,
        cli.position,
        cli.loops,
        cli.debug,
        cli.ch,
        cli.speed,
        cli.output.clone(),
    );
}

fn main() -> Result<(), std::io::Error> {
    let cli = Cli::parse();

    // Term::stdout().clear_screen().unwrap();
    println!("--===~ XmRs Player Example ~===--");
    println!("(c) 2023-2024 Sébastien Béchet\n");
    println!("Because demo scene can't die :)\n");

    // Ugly Hack just for fun
    #[cfg(feature = "import_sid")]
    if cli.sid_test_player {
        sid_test_player(&cli);
        return Ok(());
    }

    if let Some(filename) = cli.filename {
        println!("opening {}", filename);
        let contents = std::fs::read(filename.trim())?;
        match Module::load(&contents) {
            Ok(module) => {
                drop(contents); // cleanup memory
                println!("Playing {} !", module.name);

                let module = Box::new(module);
                let module_ref: &'static Module = Box::leak(module);
                play_music(
                    module_ref,
                    cli.song,
                    cli.amplification,
                    cli.position,
                    cli.loops,
                    cli.debug,
                    cli.ch,
                    cli.speed,
                    cli.output.clone(),
                );
            }
            Err(e) => {
                println!("{:?}", e);
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn play_music(
    module: &'static Module,
    song: usize,
    amplification: f32,
    position: usize,
    loops: usize,
    debug: bool,
    ch: u8,
    speed: usize,
    output: Option<String>,
) {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .expect("no output device available");

    let config = device
        .default_output_config()
        .expect("failed to get default output config");
    let sample_rate = config.sample_rate();

    let player = Arc::new(Mutex::new(XmrsPlayer::new(
        module,
        sample_rate as f32,
        song,
    )));

    {
        let mut player_lock = player.lock().unwrap();
        player_lock.set_amplification(amplification);
        if debug {
            println!("Debug on");
            println!("Module format: {:?}", module.format);
            // In 0.10+ the inline `debug(bool)` toggle has been replaced by a
            // standalone observer — register it explicitly.
            player_lock.add_observer(Box::new(DebugObserver::new()));
        }
        if ch != 0 {
            player_lock.mute_all(true);
            player_lock.set_mute_channel((ch - 1).into(), false);
        }
        player_lock.set_max_loop_count(loops);
        player_lock.goto(position, 0, speed);
    }

    if let Some(output) = output {
        println!("writing {}...", output);
        write_wave(player, output.as_str()).unwrap();
    } else {
        let player_clone = Arc::clone(&player);
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut player_lock = player_clone.lock().unwrap();
                    data.iter_mut()
                        .zip(player_lock.by_ref()) // itère sur les deux en parallèle
                        .for_each(|(sample, value)| *sample = value);
                },
                |_: cpal::StreamError| {},
                None,
            )
            .expect("failed to build output stream");

        stream.play().expect("failed to play stream");

        let stdout = Term::stdout();
        println!(
            "Enter and i keys for info, Space for pause, left or right arrow to move, escape key to exit..."
        );
        let mut playing = true;
        loop {
            if let Ok(character) = stdout.read_key() {
                match character {
                    Key::Enter => {
                        let ti = player.lock().unwrap().get_current_table_index();
                        let p = player.lock().unwrap().get_current_pattern();
                        println!("current table index:{:02x}, current pattern:{:02x}", ti, p);
                    }
                    Key::Escape => {
                        println!("Have a nice day!");
                        return;
                    }
                    Key::Char('q') => {
                        println!("Have a nice day!");
                        return;
                    }
                    Key::ArrowLeft => {
                        let i = player.lock().unwrap().get_current_table_index();
                        if i != 0 {
                            player.lock().unwrap().goto(i - 1, 0, 0);
                        }
                    }
                    Key::ArrowRight => {
                        let len = module.pattern_order[song].len();
                        let i = player.lock().unwrap().get_current_table_index();
                        if i + 1 < len {
                            player.lock().unwrap().goto(i + 1, 0, 0);
                        }
                    }
                    Key::Char(' ') => {
                        if playing {
                            println!("Pause, press space to continue");
                            player.lock().unwrap().pause(true);
                            playing = false;
                            {
                                let player_lock = player.lock().unwrap();
                                let ti = player_lock.get_current_table_index();
                                let p = player_lock.get_current_pattern();
                                let row = player_lock.get_current_row();
                                println!("Pattern [{:02X}]={:02X}, Row {:02X}", ti, p, row);
                            }
                        } else {
                            println!("Playing");
                            player.lock().unwrap().pause(false);
                            playing = true;
                        }
                    }
                    Key::Char('i') => {
                        let player_lock = player.lock().unwrap();
                        println!(
                            "name:{}\ncomment:{}",
                            player_lock.module.name, player_lock.module.comment
                        );
                        println!(
                            "speed={}, generated samples:{}, loop count:{}",
                            player_lock.get_tempo(),
                            player_lock.generated_samples(),
                            player_lock.get_loop_count()
                        );
                        for (i, instr) in player_lock.module.instrument.iter().enumerate() {
                            if !instr.name.is_empty() {
                                println!("instrument {:2}: {}", i, instr.name);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
}

use hound::{SampleFormat, WavSpec, WavWriter};

fn write_wave(
    amp: Arc<Mutex<XmrsPlayer>>,
    output_file: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let spec = WavSpec {
        channels: 2,
        sample_rate: 44100,
        bits_per_sample: 16,
        sample_format: SampleFormat::Int,
    };

    let mut writer = WavWriter::create(output_file, spec)?;
    amp.lock().unwrap().set_max_loop_count(1);
    let player_clone = Arc::clone(&amp);
    let mut player_lock = player_clone.lock().unwrap();

    for sample in player_lock.by_ref() {
        let sample_i16 = (sample.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        writer.write_sample(sample_i16)?;
    }

    writer.finalize()?;
    Ok(())
}
