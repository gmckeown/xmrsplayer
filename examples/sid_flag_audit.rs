//! Audit pour l'unification des deux drivers SID. Pour chaque tune : le driver
//! choisi (Coupled si un instrument a sync/ring/filtre, sinon PerInstrument) et
//! quels effets "coupled-only" ses instruments posent. Un effet coupled-only posé
//! par un tune PerInstrument = effet LATENT (absent aujourd'hui) que l'unification
//! activerait → point d'arbitrage / risque de régression golden.
use xmrs::core::instr_robsid::{ArpMode, VibratoMode};
use xmrs::prelude::InstrumentType;
use xmrs::tracker::import::sid::sid_module::SidModule;

type SidTune = (&'static str, fn() -> SidModule);

fn main() {
    let tunes: &[SidTune] = &[
        ("commando", SidModule::get_sid_commando),
        ("monty", SidModule::get_sid_monty_on_the_run),
        ("crazy_comets", SidModule::get_sid_crazy_comets),
        ("last_v8", SidModule::get_sid_last_v8),
        ("thing_on_a_spring", SidModule::get_sid_thing_on_a_spring),
        ("delta", SidModule::get_sid_delta),
        ("sanxion", SidModule::get_sid_sanxion_song_1),
        ("lightforce", SidModule::get_sid_lightforce),
        ("spellbound", SidModule::get_sid_spellbound),
        ("ace_2", SidModule::get_sid_ace_2),
        ("human_race", SidModule::get_sid_human_race),
        (
            "international_karate",
            SidModule::get_sid_international_karate,
        ),
        ("thrust", SidModule::get_sid_thrust),
        ("zoids", SidModule::get_sid_zoid),
    ];
    println!(
        "{:<22} {:<13} flags coupled-only posés (count instr)",
        "tune", "driver"
    );
    for (name, f) in tunes {
        let sm = f();
        let modules = sm.to_modules();
        let module = &modules[0];
        // Driver: Coupled si un instrument a sync/ring OU pilote le filtre.
        let mut uses_coupled = false;
        // Compteurs d'effets coupled-only.
        let mut interp = 0;
        let mut arp2 = 0;
        let mut wave_alt = 0;
        let mut alt_arp = 0;
        let mut two_phase = 0;
        let mut filter = 0;
        // Effets de base (partagés) — pour mémoire.
        let mut drum = 0;
        let mut skydive = 0;
        let mut arp_oct = 0;
        let mut vib = 0;
        for instr in &module.instrument {
            if let InstrumentType::RobSid(irs) = &instr.instr_type {
                let v = &irs.voice;
                if v.ctrl_sync || v.ctrl_rm {
                    uses_coupled = true;
                }
                let fx = &irs.fx;
                if fx.filter.enable {
                    uses_coupled = true;
                    filter += 1;
                }
                if matches!(fx.vibrato, VibratoMode::Semitone { .. }) {
                    interp += 1;
                }
                if matches!(fx.arpeggio, ArpMode::TwoNoteFixed(_)) {
                    arp2 += 1;
                }
                if !fx.wave_alt.alt_shape.is_silent() {
                    wave_alt += 1;
                }
                // The v15 two-note drop is a 2-step `Cycle` riding the skydive climb.
                if matches!(
                    fx.arpeggio,
                    ArpMode::Cycle {
                        len: 2,
                        follow_climb: true,
                        ..
                    }
                ) {
                    alt_arp += 1;
                }
                if !fx.two_phase.attack_shape.is_silent() {
                    two_phase += 1;
                }
                if fx.drum.enable {
                    drum += 1;
                }
                if fx.skydive.enable {
                    skydive += 1;
                }
                if matches!(fx.arpeggio, ArpMode::Octave) {
                    arp_oct += 1;
                }
                if matches!(
                    fx.vibrato,
                    VibratoMode::Upward { .. } | VibratoMode::Bipolar { .. }
                ) {
                    vib += 1;
                }
            }
        }
        let mut co = Vec::new();
        if interp > 0 {
            co.push(format!("interp×{}", interp));
        }
        if arp2 > 0 {
            co.push(format!("arp2×{}", arp2));
        }
        if wave_alt > 0 {
            co.push(format!("wavealt×{}", wave_alt));
        }
        if alt_arp > 0 {
            co.push(format!("altarp×{}", alt_arp));
        }
        if two_phase > 0 {
            co.push(format!("2phase×{}", two_phase));
        }
        if filter > 0 {
            co.push(format!("filter×{}", filter));
        }
        let driver = if uses_coupled { "COUPLED" } else { "PerInstr" };
        let co_str = if co.is_empty() {
            "—".into()
        } else {
            co.join(" ")
        };
        println!("{:<22} {:<13} {}", name, driver, co_str);
        // Ligne secondaire : effets de base (pour le contexte).
        println!(
            "{:<22} {:<13} base: drum×{} skydive×{} arpOct×{} vib×{}",
            "", "", drum, skydive, arp_oct, vib
        );
    }
    println!("\n*** RISQUE = un flag coupled-only sur une ligne 'PerInstr' (effet latent activé par l'unification) ***");
}
