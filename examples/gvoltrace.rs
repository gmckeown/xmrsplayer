//! Trace the engine's global_volume (and abs tick) once per second while
//! rendering, to map ticks→time and see a GlobalVolume slide's trajectory.
//! Usage: gvoltrace <module> [seconds] [rate]
use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let secs: f64 = a.get(2).and_then(|s| s.parse().ok()).unwrap_or(120.0);
    let rate: u32 = a.get(3).and_then(|s| s.parse().ok()).unwrap_or(48000);
    let bytes = std::fs::read(path).unwrap();
    let module = Module::load(&bytes).unwrap();
    // Direct lane probe: does the GlobalVolume Slide lane arm?
    {
        use xmrs::core::daw::automation::AutomationTarget;
        for (i, lane) in module
            .lanes_for(AutomationTarget::GlobalVolume, 0)
            .enumerate()
        {
            for t in [0u32, 11263, 11264, 11280, 12000, 12272, 20000, 60000] {
                let st = lane.slide_state_at(t);
                println!("PROBE lane#{i} slide_state_at({t}) = {:?}", st);
            }
        }
        // Max event tick across ALL lanes → the song's tick span.
        let mut maxt = 0u32;
        for lane in module.automation.iter() {
            for ev in lane.events_in(0, u32::MAX) {
                maxt = maxt.max(ev.tick());
            }
        }
        println!("PROBE max lane tick across all lanes = {maxt}");
    }
    let mut player = XmrsPlayer::new(&module, rate, 0);
    let n = (secs * rate as f64) as usize;
    let step = rate as usize; // one log line per second
    println!(" sec   gvol_frac   pos   row  song  tempo  bpm");
    let half = step / 2;
    for i in 0..n {
        let _ = player.sample(true);
        if i % half == 0 {
            let gv = player.global_volume();
            let q = gv.as_q15_i32();
            println!(
                "{:6.1}   {:.4}   {:3}   {:3}   {}   {:3}   {:3}",
                i as f64 / step as f64,
                q as f64 / 32768.0,
                player.get_current_table_index(),
                player.get_current_row(),
                player.current_song(),
                player.get_tempo(),
                player.get_bpm(),
            );
        }
    }
}
