use clap::Parser;
use console::{Key, Term};
use std::sync::{Arc, Mutex};

use xmrs::import::amiga::amiga_module::AmigaModule;
use xmrs::import::s3m::s3m_module::S3mModule;
use xmrs::import::xm::xmmodule::XmModule;
use xmrs::prelude::*;

mod bufferedsource;
use bufferedsource::BufferedSource;
use xmrsplayer::prelude::*;

const SAMPLE_RATE: u32 = 48000;

#[derive(Parser)]
struct Cli {
    /// Choose XM or XmRs File
    #[arg(short = 'f', long, required = true, value_name = "filename")]
    filename: Option<String>,

    /// Choose amplification
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

    /// Force speed (0 = don't force)
    #[arg(short = 's', long, default_value = "0")]
    speed: usize,

    /// Song number inside the module (for modules with several songs)
    #[arg(long, default_value = "0")]
    song: usize,
}

fn main() -> Result<(), std::io::Error> {
    let cli = Cli::parse();

    match cli.filename {
        Some(filename) => {
            // Term::stdout().clear_screen().unwrap();
            println!("--===~ XmRs Player Example ~===--");
            println!("(c) 2023-2024 Sébastien Béchet\n");
            println!("Because demo scene can't die :)\n");
            // let path = std::env::current_dir()?;
            // println!("The current directory is {}", path.display());
            println!("opening {}", filename);
            let contents = std::fs::read(filename.trim())?;
            match filename.split('.').last() {
                Some(extension) if extension == "xm" || extension == "XM" => {
                    match XmModule::load(&contents) {
                        Ok(xm) => {
                            drop(contents); // cleanup memory
                            let module = xm.to_module();
                            drop(xm);
                            println!("Playing {} !", module.name);

                            let module = Box::new(module);
                            let module_ref: &'static Module = Box::leak(module);
                            rodio_play(
                                module_ref,
                                cli.song,
                                cli.amplification,
                                cli.position,
                                cli.loops,
                                cli.debug,
                                cli.ch,
                                cli.speed,
                            );
                        }
                        Err(e) => {
                            println!("{:?}", e);
                        }
                    }
                }
                Some(extension) if extension == "mod" || extension == "MOD" => {
                    match AmigaModule::load(&contents) {
                        Ok(amiga) => {
                            drop(contents); // cleanup memory
                            let module = amiga.to_module();
                            drop(amiga);
                            println!("Playing {} !", module.name);
                            let module = Box::new(module);
                            let module_ref: &'static Module = Box::leak(module);
                            rodio_play(
                                module_ref,
                                cli.song,
                                cli.amplification,
                                cli.position,
                                cli.loops,
                                cli.debug,
                                cli.ch,
                                cli.speed,
                            );
                        }
                        Err(e) => {
                            println!("{:?}", e);
                        }
                    }
                }
                Some(extension) if extension == "s3m" || extension == "S3M" => {
                    match S3mModule::load(&contents) {
                        Ok(s3m) => {
                            drop(contents); // cleanup memory
                            let module = s3m.to_module();
                            drop(s3m);
                            println!("Playing {} !", module.name);
                            let module = Box::new(module);
                            let module_ref: &'static Module = Box::leak(module);
                            rodio_play(
                                module_ref,
                                cli.song,
                                cli.amplification,
                                cli.position,
                                cli.loops,
                                cli.debug,
                                cli.ch,
                                cli.speed,
                            );
                        }
                        Err(e) => {
                            println!("{:?}", e);
                        }
                    }
                }
                Some(_) | None => {
                    println!("File unknown?");
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn rodio_play(
    module: &'static Module,
    song: usize,
    amplification: f32,
    position: usize,
    loops: usize,
    debug: bool,
    ch: u8,
    speed: usize,
) {
    // rodio 0.22 renamed the API terminology: the old `Sink` (audio track
    // you append sources to) became `Player`; the old `OutputStream` (the
    // OS output handle) became `MixerDeviceSink`, obtained via
    // `DeviceSinkBuilder`. Mechanical rename, identical semantics. See
    // rodio UPGRADE.md ("Sink terms are replaced with Player and Stream
    // terms replaced with Sink"). The device-sink must stay alive for
    // the player to play, hence the `let` binding here.
    let stream_handle =
        rodio::DeviceSinkBuilder::open_default_sink().expect("open default audio stream");
    // Silence rodio's "dropped" log line which would clobber our CLI UI.
    // Omit this call if you actually want the warning.
    // stream_handle.log_on_drop(false);
    let sink = rodio::Player::connect_new(&stream_handle.mixer());

    let player = Arc::new(Mutex::new(XmrsPlayer::new(
        module,
        SAMPLE_RATE as f32,
        song,
    )));
    {
        let mut player_lock = player.lock().unwrap();
        player_lock.amplification = amplification;
        if debug {
            println!("Debug on");
            println!("Module format: {:?}", module.format);
        }
        player_lock.debug(debug);
        if ch != 0 {
            player_lock.mute_all(true);
            player_lock.set_mute_channel((ch - 1).into(), false);
        }
        player_lock.set_max_loop_count(loops);
        player_lock.goto(position, 0, speed);
    }
    let source = BufferedSource::new(Arc::clone(&player), SAMPLE_RATE);
    sink.append(source);
    // sink.append(player.buffered());
    sink.play();

    let stdout = Term::stdout();
    println!(
        "Enter key for info, Space for pause, left or right arrow to move, escape key to exit..."
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
                    sink.stop();
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
                        sink.pause();
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
                        sink.play();
                        playing = true;
                    }
                }
                _ => {}
            }
        }
    }
}
