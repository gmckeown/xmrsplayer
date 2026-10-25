use clap::Parser;
use console::{Key, Term};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::Duration;

use xmrs::core::duration::ModuleDuration;
use xmrs::prelude::*;
use xmrsplayer::prelude::*;

/// Render a `Duration` as `MM:SS`. Seconds are truncated (not
/// rounded) to match what the listener saw on the playback
/// counter at the moment the player exited. Songs longer than
/// 99 minutes overflow the minute field naturally
/// (`123:45` for 2:03:45) — fine for the rare progressive-trance
/// outlier and avoids any conditional hour-field branching.
fn format_mm_ss(d: Duration) -> String {
    let total = d.as_secs();
    format!("{:02}:{:02}", total / 60, total % 60)
}

/// `println!` analogue that renders correctly when the tty is
/// in raw mode. The reader thread in `play_music` keeps the
/// terminal raw nearly all the time (via the
/// `console::Term::read_key` loop, which toggles `termios` on
/// every read), and in that mode the kernel's `\n → \r\n`
/// translation is off — a plain `println!` then produces
/// staircase output, each line starting at the previous
/// line's last column instead of column 0.
///
/// Emitting `\r\n` explicitly works in both raw and cooked
/// modes: the extra `\r` is a no-op in cooked mode because the
/// `ONLCR` postprocessing already moved the cursor to column 0
/// when it expanded the `\n`.
macro_rules! tprintln {
    () => {{
        use std::io::Write as _;
        let _ = std::io::stdout().write_all(b"\r\n");
    }};
    ($($arg:tt)*) => {{
        use std::io::Write as _;
        print!($($arg)*);
        let _ = std::io::stdout().write_all(b"\r\n");
    }};
}

/// Restore the controlling terminal to a sane cooked state.
/// Called at the tail of `play_music` so the listener's shell
/// is usable after the binary returns.
///
/// The keyboard reader thread holds the tty in raw mode (the
/// `console::Term::read_key` API saves and restores `termios`
/// around each individual `read`, but the thread spends
/// essentially 100 % of its time *inside* `read_key` waiting
/// for the next byte — so the tty is effectively always
/// raw). When `play_music` returns and main exits, the OS
/// reaps the reader thread without giving its current
/// `read_key` call a chance to run the cleanup path, and the
/// `termios` settings *persist* in the tty driver. The next
/// process to use that tty — typically the user's shell —
/// inherits the broken state (no echo, no line buffering, no
/// `\n → \r\n`).
///
/// `stty sane` is the canonical fix for that condition. We
/// only invoke it on Unix-like targets — Windows consoles use
/// a different API that doesn't have the same persistence
/// issue.
fn restore_terminal() {
    #[cfg(unix)]
    {
        // Ignore failures: if `stty` is missing or the
        // process isn't attached to a tty, there's nothing
        // useful to do, and we're on the exit path anyway.
        let _ = std::process::Command::new("stty").arg("sane").status();
    }
}

/// Observer that latches a shared boolean when the player fires
/// `on_song_end`. The interactive event loop polls the flag
/// every keyboard-poll tick so the binary exits as soon as
/// the song finishes, instead of waiting for the listener to
/// press a key.
///
/// `on_row` is mandatory on `PlayerObserver` (no default body),
/// so we provide a no-op implementation; this observer only
/// cares about the terminal event.
struct ExitOnSongEnd {
    flag: Arc<AtomicBool>,
}

impl PlayerObserver for ExitOnSongEnd {
    fn on_row(&mut self, _ctx: &RowContext<'_>) {}
    fn on_song_end(&mut self) {
        // `Relaxed` is enough — there's no other state guarded
        // by this flag, and the main loop only needs eventual
        // visibility (it polls on a 50 ms cadence anyway).
        self.flag.store(true, Ordering::Relaxed);
    }
}

#[derive(Parser)]
struct Cli {
    /// The music file to play. Every format the build can import is
    /// recognised from its own bytes — MOD / XM / S3M / IT / DW / SID.
    #[arg(short = 'f', long, required = true, value_name = "filename")]
    filename: Option<String>,

    /// Sub-song number, counted from 1 (1 = first sub-song; most
    /// formats have only one, DW and SID files expose several).
    /// Default: 1.
    #[arg(short = 's', long, default_value = "1")]
    song: usize,

    /// Choose output wave file
    #[arg(short = 'o', long, value_name = "output filename")]
    output: Option<String>,

    /// Output amplification. The mixer is now calibrated so that
    /// `1.0` is the natural unity gain (matches schism's mix levels
    /// after the engine's `MIXER_HEADROOM_DIV` attenuation). Lower
    /// it if you want quieter playback; raise it for more presence,
    /// at your own clipping risk on busy modules.
    #[arg(short = 'a', long, default_value = "1.0")]
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

    /// Audition a single instrument/sample (0-based index) instead of
    /// playing a song — for sound-effect banks (e.g. `*-sfx.dw`) that
    /// carry samples but no song timeline, or to preview any module's
    /// instruments. Pair with `--note` to choose the pitch; `--loops`
    /// controls repeats (default: repeat forever).
    #[arg(short = 'i', long, value_name = "index")]
    instrument: Option<usize>,

    /// Note used by `--instrument`, as a semitone index 0..=119
    /// (C-0 = 0, C-4 = 48, the default).
    #[arg(short = 'n', long, default_value = "48")]
    note: u8,

    /// Compute the song's theoretical duration and exit without
    /// playing anything. The duration is reported as `MM:SS`,
    /// using the row-scheduler walker from `xmrs::duration` —
    /// every tempo / BPM / pattern-jump / pattern-loop /
    /// position-jump effect on the song's flow control is
    /// honoured, so the result is the same length the
    /// interactive player would produce (modulo the same caveats
    /// the estimator itself documents: infinite loops are
    /// capped, pattern-delay stacking is honoured).
    #[arg(short = 'D', long, default_value = "false")]
    duration: bool,
}

fn main() -> Result<(), std::io::Error> {
    let mut cli = Cli::parse();

    // `--song` is 1-based for humans (song 1 = first sub-song), but
    // every internal index — `XmrsPlayer::new`, `Module::duration`,
    // the timeline `song` field — is 0-based. Validate the 0 case
    // (no song 0 exists in 1-based numbering) and convert once here,
    // so the whole downstream path keeps using a plain 0-based index.
    if cli.song == 0 {
        eprintln!(
            "error: --song is counted from 1 (song 1 = first sub-song); \
             0 is not a valid song number. Use -s 1 for the first song."
        );
        std::process::exit(2);
    }
    cli.song -= 1;

    // Term::stdout().clear_screen().unwrap();
    println!("--===~ XmRs Player Example ~===--");
    println!("(c) 2023-2026 Sébastien Béchet\n");
    println!("Because demo scene can't die :)\n");

    if let Some(filename) = cli.filename {
        println!("opening {}", filename);
        let contents = std::fs::read(filename.trim())?;
        let is_tfmx = contents.len() >= 10 && (
            &contents[..9] == b"TFMX-SONG" ||
            &contents[..9] == b"TFMX_SONG" ||
            &contents[..9] == b"TFMX_1.5"
        ) || filename.trim().ends_with(".tfx") || filename.trim().ends_with(".TFX")
          || std::path::Path::new(filename.trim()).file_name().and_then(|s| s.to_str()).map_or(false, |s| s.starts_with("mdat.") || s.starts_with("MDAT."));

        let module_res = if is_tfmx {
            Module::load_tfmx_path(filename.trim(), cli.song)
        } else {
            Module::load(&contents)
        };

        match module_res {
            Ok(mut module) => {
                drop(contents); // cleanup memory
                if is_tfmx {
                    cli.song = 0;
                }
                                // David Whittaker `.dw` files carry no embedded
                                // song name — the importer tags them all with the
                                // same generic "David Whittaker (.dw)". Derive a
                                // per-file title from the filename stem so a folder
                                // of `.dw` rips is distinguishable in the player /
                                // any UI. Other formats keep their embedded name.
                if module.origin == Some(xmrs::tracker::format::ModuleFormat::Dw) {
                    if let Some(stem) = std::path::Path::new(filename.trim())
                        .file_stem()
                        .and_then(|s| s.to_str())
                    {
                        module.name = stem.to_string();
                    }
                }
                // `--duration` short-circuits before any audio
                // setup: we just want the theoretical length of
                // the chosen sub-song. Print as `MM:SS` and
                // exit — no playback, no device probing.
                if cli.duration {
                    let d = module.duration(cli.song);
                    println!("{}", format_mm_ss(d));
                    return Ok(());
                }
                // `--instrument N`: replace the song with a one-note
                // audition of instrument N (sound-effect banks carry
                // samples but no song). Splices a temporary timeline
                // into the owned module in place and plays song 0 of it;
                // `--loops` repeats the audition. No restore needed — the
                // CLI discards the module on exit.
                if let Some(instr) = cli.instrument {
                    let pitch = xmrs::core::pitch::Pitch::try_from(cli.note)
                        .unwrap_or(xmrs::core::pitch::Pitch::C4);
                    // ~4 s audition window per loop iteration, derived
                    // from the module's row grid (seconds/row = 2.5 *
                    // tempo / bpm).
                    let sec_per_row = (2.5 * module.default_tempo.max(1) as f64)
                        / module.default_bpm.max(1) as f64;
                    let rows = (4.0 / sec_per_row).ceil().max(2.0) as usize;
                    if !xmrsplayer::sample_preview::install_instrument_audition(
                        &mut module,
                        instr,
                        pitch,
                        rows,
                    ) {
                        eprintln!(
                            "error: --instrument {instr} out of range \
                             (module has {} instruments, 0..={})",
                            module.instrument.len(),
                            module.instrument.len().saturating_sub(1)
                        );
                        return Ok(());
                    }
                    cli.song = 0;
                    println!(
                        "Auditioning instrument {instr} at {:?} (note {})",
                        pitch, cli.note
                    );
                }

                // Song count = highest `song` index in the timeline
                // map + 1 (DW multi-sub-song modules expose several;
                // most formats have just one).
                let song_count = module
                    .timeline_map
                    .entries
                    .iter()
                    .map(|e| e.song as usize)
                    .max()
                    .map(|m| m + 1)
                    .unwrap_or(1);
                // Speed / BPM of the SELECTED sub-song, not the module
                // defaults: `default_tempo`/`default_bpm` snapshot the
                // primary (song 0) values, so for `-s N>0` they'd print
                // the wrong tempo (e.g. a DW sub-song at speed 4 shown as
                // the primary's 6). Read the first row of the chosen
                // song's timeline instead — the same `speed_at_row` /
                // `bpm_at_row` the sequencer applies on its first row.
                let (song_speed, song_bpm) = module
                    .timeline_map
                    .entries
                    .iter()
                    .filter(|e| e.song as usize == cli.song && e.loop_iter == 0)
                    .min_by_key(|e| e.tick)
                    .map(|e| (e.speed_at_row as usize, e.bpm_at_row as usize))
                    .unwrap_or((module.default_tempo, module.default_bpm));
                println!(
                    "Playing {} ! (song {}/{}, speed {}, bpm {})",
                    module.name,
                    cli.song + 1,
                    song_count,
                    song_speed,
                    song_bpm,
                );

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
                // The importer's own words, then the exit code — a file it
                // cannot read is a failure, and a script that pipes a folder
                // through the player deserves to hear about it.
                eprintln!("error: cannot import {filename}: {e:?}");
                std::process::exit(2);
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
    // `cpal::StreamConfig::sample_rate()` already yields a
    // primitive `u32` Hz value in this cpal version — pass it
    // straight through to the (now Q-typed) player ctor.
    let sample_rate_hz: u32 = sample_rate;

    let player = Arc::new(Mutex::new(XmrsPlayer::new(module, sample_rate_hz, song)));

    // Keep a clone alive past the consumption paths below — we
    // need to read `generated_samples()` once playback is done
    // (either the WAV writer is exhausted or the user quits the
    // interactive loop) so we can compare actual elapsed time
    // against the duration estimator's prediction.
    let player_summary = Arc::clone(&player);

    // Latched by `ExitOnSongEnd::on_song_end` from the cpal
    // callback thread; read by the interactive event loop
    // below. When set, the loop exits — the binary returns to
    // `main` without waiting for the listener to press a key.
    let song_ended = Arc::new(AtomicBool::new(false));

    {
        let mut player_lock = player.lock().unwrap();
        // Q4.12 Q-format amplification — convert the f32 CLI
        // arg at the boundary.
        player_lock.set_amplification(Amplification::from_raw_q4_12(
            ((amplification * 4096.0)
                .round()
                .clamp(i16::MIN as f32, i16::MAX as f32)) as i16,
        ));
        if debug {
            tprintln!("Debug on");
            tprintln!("Module format: {:?}", module.origin);
            // In 0.10+ the inline `debug(bool)` toggle has been replaced by a
            // standalone observer — register it explicitly.
            player_lock.add_observer(Box::new(DebugObserver::new()));
        }
        // Auto-exit observer — registered for both consumption
        // paths so the same flag drives the interactive loop's
        // exit condition. (The WAV path doesn't need it, since
        // `write_wave` runs the iterator to natural exhaustion,
        // but registering unconditionally keeps the setup
        // symmetrical and the observer's overhead is one
        // atomic store on the song-end event.)
        player_lock.add_observer(Box::new(ExitOnSongEnd {
            flag: Arc::clone(&song_ended),
        }));
        if ch != 0 {
            player_lock.mute_all(true);
            player_lock.set_mute_channel((ch - 1).into(), false);
        }
        player_lock.set_max_loop_count(loops);
        player_lock.goto(position, 0, speed);
    }

    if let Some(output) = output {
        tprintln!("writing {}...", output);
        write_wave(player, output.as_str()).unwrap();
    } else {
        let player_clone = Arc::clone(&player);
        let stream = device
            .build_output_stream(
                &config.config(),
                move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                    let mut player_lock = player_clone.lock().unwrap();
                    // The player produces `i16` PCM (full
                    // ±32767 range). cpal in this stream
                    // configuration wants `f32` in [-1, 1] —
                    // convert at the demo boundary.
                    data.iter_mut()
                        .zip(player_lock.by_ref()) // itère sur les deux en parallèle
                        .for_each(|(sample, value)| {
                            *sample = value as f32 / i16::MAX as f32;
                        });
                },
                |_: cpal::StreamError| {},
                None,
            )
            .expect("failed to build output stream");

        stream.play().expect("failed to play stream");

        // Keyboard input is blocking on the main thread by
        // default — once we enter `read_key()` we lose the
        // ability to react to song-end notifications coming
        // from the cpal callback thread. Move input to its own
        // thread and bridge to the event loop through an mpsc
        // channel, so the loop can `recv_timeout` on it and
        // poll the `song_ended` flag between waits.
        //
        // The reader thread is detached: when the binary exits,
        // the OS reclaims it. We don't try to wake it up — it
        // sits in a blocking stdin read until the process dies.
        let (tx, rx) = mpsc::channel::<Key>();
        thread::spawn(move || {
            let stdout = Term::stdout();
            while let Ok(key) = stdout.read_key() {
                if tx.send(key).is_err() {
                    // Receiver dropped — main thread exited the
                    // event loop. Nothing more to do.
                    break;
                }
            }
        });

        tprintln!(
            "Enter and i keys for info, Space for pause, left or right arrow to move, escape key to exit..."
        );
        let mut playing = true;
        // The interactive loop now exits on any of three
        // signals: an explicit user quit (Escape/q), the
        // `song_ended` observer flag (set when the player's
        // sequencer emits `SongEnd`), or the keyboard channel
        // disconnecting (which would mean the reader thread
        // crashed). Polling cadence: 50 ms — short enough that
        // the song-end exit feels instantaneous to a
        // listener, long enough that we don't burn CPU on
        // empty loops.
        'event_loop: loop {
            if song_ended.load(Ordering::Relaxed) {
                tprintln!("Song finished.");
                break 'event_loop;
            }
            let character = match rx.recv_timeout(Duration::from_millis(50)) {
                Ok(k) => k,
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => break 'event_loop,
            };
            match character {
                Key::Enter => {
                    let ti = player.lock().unwrap().get_current_table_index();
                    let p = player.lock().unwrap().get_current_pattern();
                    tprintln!("current table index:{:02x}, current pattern:{:02x}", ti, p);
                }
                Key::Escape | Key::Char('q') => {
                    tprintln!("Have a nice day!");
                    break 'event_loop;
                }
                Key::ArrowLeft => {
                    let i = player.lock().unwrap().get_current_table_index();
                    if i != 0 {
                        player.lock().unwrap().goto(i - 1, 0, 0);
                    }
                }
                Key::ArrowRight => {
                    let len = module.song_length(song);
                    let i = player.lock().unwrap().get_current_table_index();
                    if i + 1 < len {
                        player.lock().unwrap().goto(i + 1, 0, 0);
                    }
                }
                Key::Char(' ') => {
                    if playing {
                        tprintln!("Pause, press space to continue");
                        player.lock().unwrap().pause(true);
                        playing = false;
                        {
                            let player_lock = player.lock().unwrap();
                            let ti = player_lock.get_current_table_index();
                            let p = player_lock.get_current_pattern();
                            let row = player_lock.get_current_row();
                            tprintln!("Pattern [{:02X}]={:02X}, Row {:02X}", ti, p, row);
                        }
                    } else {
                        tprintln!("Playing");
                        player.lock().unwrap().pause(false);
                        playing = true;
                    }
                }
                Key::Char('i') => {
                    let player_lock = player.lock().unwrap();
                    tprintln!(
                        "name:{}\ncomment:{}",
                        player_lock.module.name,
                        player_lock.module.comment
                    );
                    tprintln!(
                        "speed={}, generated samples:{}, loop count:{}",
                        player_lock.get_tempo(),
                        player_lock.generated_samples(),
                        player_lock.get_loop_count()
                    );
                    // Live duration probe: what has been
                    // played so far vs the estimator's
                    // prediction for the whole song. Same
                    // computation as the end-of-playback
                    // summary at the tail of `play_music`,
                    // but the listener can ask for it at
                    // any moment.
                    let actual_frames = player_lock.generated_samples();
                    let sr = player_lock.get_sample_rate();
                    let actual = Duration::from_secs(actual_frames / sr as u64);
                    let theoretical = module.duration(song);
                    tprintln!(
                        "Duration: {} / {}",
                        format_mm_ss(actual),
                        format_mm_ss(theoretical),
                    );
                    for (i, instr) in player_lock.module.instrument.iter().enumerate() {
                        if !instr.name.is_empty() {
                            tprintln!("instrument {:2}: {}", i, instr.name);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    // ---- Duration summary --------------------------------------------
    //
    // `generated_samples()` is the number of stereo frames the
    // player actually produced (paused frames do not count — see
    // `XmrsPlayer::sample` early-return). Divide by the
    // sample-rate (Hz, already in frames-per-second) to recover
    // wall-clock seconds of music heard.
    //
    // Theoretical duration comes from the row-scheduler walker
    // in `xmrs::duration` — it honours every flow-control effect
    // the same playback engine does, so on a healthy module the
    // two numbers should agree closely whenever the song was
    // played to completion (WAV path, or an interactive listener
    // that let it run out). When the listener quit early, the
    // played value is naturally smaller and the ratio reveals
    // how far through the song they got.
    let (actual_frames, sample_rate_hz_actual) = {
        let lock = player_summary.lock().unwrap();
        (lock.generated_samples(), lock.get_sample_rate())
    };
    let actual = Duration::from_secs(actual_frames / sample_rate_hz_actual as u64);
    let theoretical = module.duration(song);
    tprintln!(
        "Duration: {} / {}",
        format_mm_ss(actual),
        format_mm_ss(theoretical),
    );

    // Reset the tty to a cooked state for the caller's shell.
    // See `restore_terminal` doc for why this is necessary.
    restore_terminal();
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
        // Player iterator yields `i16` directly — write straight
        // to the wav file with no float round-trip.
        writer.write_sample(sample)?;
    }

    writer.finalize()?;
    Ok(())
}
