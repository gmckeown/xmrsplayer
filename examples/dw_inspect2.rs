//! Ad-hoc extraction dump for a single `.dw` — compares the importer's
//! decoded layout/sub-songs/position-lists/tracks against the Ghidra
//! ground truth. Throwaway diagnostic for the C3 (bubble bobble) work.

use xmrs::tracker::import::dw::detect::detect;
use xmrs::tracker::import::dw::dw_module::DwModule;

fn main() -> Result<(), String> {
    let path = std::env::args().nth(1).ok_or("usage: dw_inspect2 <file.dw>")?;
    let data = std::fs::read(&path).map_err(|e| format!("read {}: {}", path, e))?;

    let layout = detect(&data).ok_or("detect returned None")?;
    println!("== LAYOUT ==");
    println!("start_offset       = {:#x} ({})", layout.start_offset, layout.start_offset);
    println!("sub_song_list_off  = {:?}", layout.sub_song_list_offset.map(|o| format!("{:#x}", o)));
    println!("sub_song_row_width = {}", layout.sub_song_row_width);
    println!("uses_32bit_ptrs    = {}", layout.uses_32bit_pointers);
    println!("dispatcher thresh  = {:x?}", layout.dispatcher.thresholds);
    println!("command_map present= {}", layout.command_map.is_some());
    println!("play_offset        = {:?}", layout.play_offset.map(|o| format!("{:#x}", o)));
    println!("volenv_table_off   = {:?}", layout.volume_envelope_table_offset.map(|o| format!("{:#x}", o)));
    println!("arp_table_off      = {:?}", layout.arpeggio_table_offset.map(|o| format!("{:#x}", o)));

    let dw = DwModule::load(&data).map_err(|e| format!("load: {:?}", e))?;
    println!("\n== SUB-SONGS ({}) selected={:?} ==", dw.sub_songs.len(), dw.selected_sub_song);
    for (i, s) in dw.sub_songs.iter().enumerate() {
        let offs: Vec<String> = s.channel_position_offsets.iter().map(|o| format!("{:#x}", o)).collect();
        println!("  song {}: speed={} delay={} chans={:?}", i, s.speed, s.delay_speed, offs);
    }

    println!("\n== POSITION LISTS (selected song) ==");
    for (ch, pl) in dw.position_lists.iter().enumerate() {
        let ents: Vec<String> = pl.entries.iter().map(|e| format!("{:#x}", e)).collect();
        println!("  ch{}: loop_to={:?} entries={:?}", ch, pl.loop_to, ents);
    }

    println!("\n== ARPEGGIOS ({}) / ENVELOPES ({}) ==", dw.arpeggios.len(), dw.volume_envelopes.len());
    for a in dw.arpeggios.iter() {
        println!("  arp[{}] offsets={:?}", a.index, a.offsets);
    }

    let module = dw.to_module();
    println!("\n== MODULE INSTRUMENTS ({}) ==", module.instrument.len());
    for (i, ins) in module.instrument.iter().enumerate() {
        use xmrs::prelude::InstrumentType;
        if let InstrumentType::Default(d) = &ins.instr_type {
            let pe = &d.voice.pitch_envelope;
            if pe.enabled {
                let sts: Vec<i32> = pe.point.iter()
                    .map(|p| (p.value.raw_q15().raw() as i32 - 16384) / 1024)
                    .collect();
                println!("  instr[{}] '{}' pitchenv enabled loop={}..{} semis={:?}",
                    i, ins.name, pe.loop_start_point, pe.loop_end_point, sts);
            }
            let ve = &d.voice.volume_envelope;
            if ve.enabled {
                let vols: Vec<u8> = ve.point.iter().map(|p| p.value.to_byte_64()).collect();
                println!("  instr[{}] '{}' VOLenv enabled sus={}..{} npts={} vols={:?}",
                    i, ins.name, ve.sustain_start_point, ve.sustain_end_point, ve.point.len(), vols);
            }
        }
    }
    // which instrument does each ch3 track use, song 1, around pattern 5
    use xmrs::prelude::{Track, InstrumentType as IT2};
    println!("== song1 ch3 clips (pattern 5 region, tick 960..1152) ==");
    for c in module.clips.iter().filter(|c| c.song == 1 && c.target_channel == 3) {
        if c.end_tick < 900 || c.position_tick > 1200 { continue; }
        if let Some(Track::Notes { instrument, name, .. }) = module.tracks.get(c.track as usize) {
            let arp = module.instrument.get(*instrument).and_then(|ins| {
                if let IT2::Default(d) = &ins.instr_type {
                    if d.voice.pitch_envelope.enabled {
                        Some(d.voice.pitch_envelope.point.iter()
                            .map(|p| (p.value.raw_q15().raw() as i32 - 16384) / 1024)
                            .collect::<Vec<i32>>())
                    } else { None }
                } else { None }
            });
            println!("  clip tick {}..{} -> instr {} arp={:?}", c.position_tick, c.end_tick, instrument, arp);
        }
    }

    println!("\n== TRACKS ({}) ==", dw.tracks.len());
    for t in dw.tracks.iter().take(12) {
        let head: Vec<String> = t.bytes.iter().take(20).map(|b| format!("{:02x}", b)).collect();
        println!("  @ {:#x} len={} bytes[0..20]={}", t.offset, t.bytes.len(), head.join(" "));
        println!("     events[0..8]={:?}", t.events.iter().take(8).collect::<Vec<_>>());
    }
    Ok(())
}
