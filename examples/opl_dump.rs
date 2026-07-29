//! Throwaway harmonic-dump harness: render ONE OPL instrument at ONE note
//! through xmrs and Schism, write both as raw mono i16 so an external FFT
//! (numpy) can compare harmonic-by-harmonic. Used to dig into per-patch
//! timbre divergences `opl_compare` flags.
//!
//! Usage: cargo run --release --example opl_dump -- <module.s3m> <instr_1based> <semitone>
//! Writes /tmp/opl_xmrs.raw (i16 LE mono @44100) and /tmp/opl_schism.wav.

use std::process::Command;
use xmrs::core::module::Module;
use xmrsplayer::xmrsplayer::XmrsPlayer;

const RATE: u32 = 44100;
const SECS: f64 = 0.45;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let path = &a[1];
    let want: usize = a[2].parse().unwrap();
    let semi: u8 = a[3].parse().unwrap();
    let src = std::fs::read(path).unwrap();

    let mut rec = find_rec(&src, want).expect("instrument not OPL / not found");
    // Optional: override the 12 OPL bytes (record offset 0x10..0x1c) with a
    // 24-hex-char string, for synthetic-patch probing.
    if let Ok(hx) = std::env::var("OPL_PATCH_HEX") {
        let bytes: Vec<u8> = (0..hx.len() / 2)
            .map(|i| u8::from_str_radix(&hx[2 * i..2 * i + 2], 16).unwrap())
            .collect();
        assert_eq!(bytes.len(), 12, "OPL_PATCH_HEX must be 12 bytes");
        rec[0x10..0x1c].copy_from_slice(&bytes);
    }
    let s3m = build_minimal_s3m(&rec, semi);

    // xmrs
    let m = Module::load(&s3m).unwrap();
    let mut p = XmrsPlayer::new(&m, RATE, 0);
    let n = (RATE as f64 * SECS) as usize;
    let mut raw = Vec::with_capacity(n * 2);
    for _ in 0..n {
        let (l, r) = p.sample(true).unwrap_or((0, 0));
        let mono = ((l as i32 + r as i32) / 2) as i16;
        raw.extend_from_slice(&mono.to_le_bytes());
    }
    std::fs::write("/tmp/opl_xmrs.raw", &raw).unwrap();

    // schism
    let schism = schism_bin();
    let inp = "/tmp/opl_dump_in.s3m";
    std::fs::write(inp, &s3m).unwrap();
    let _ = std::fs::remove_file("/tmp/opl_schism.wav");
    let _ = Command::new(&schism)
        .arg("--headless")
        .arg("--diskwrite=/tmp/opl_schism.wav")
        .arg(inp)
        .output();
    eprintln!("wrote /tmp/opl_xmrs.raw and /tmp/opl_schism.wav (instr #{want}, semitone {semi})");
}

/// Path to a local Schism Tracker build, the reference this example compares
/// against. `$SCHISM_BIN` wins; otherwise a build under `$HOME/Documents`,
/// derived rather than written out so the default is not one machine's.
fn schism_bin() -> String {
    std::env::var("SCHISM_BIN").unwrap_or_else(|_| {
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("Documents/schismtracker-20251014/build_xmrs/schismtracker")
            .to_string_lossy()
            .into_owned()
    })
}

fn find_rec(d: &[u8], want: usize) -> Option<Vec<u8>> {
    let ordnum = u16::from_le_bytes([d[0x20], d[0x21]]) as usize;
    let insnum = u16::from_le_bytes([d[0x22], d[0x23]]) as usize;
    let ipp = 0x60 + ordnum;
    for i in 0..insnum {
        let pp = u16::from_le_bytes([d[ipp + 2 * i], d[ipp + 2 * i + 1]]) as usize;
        let off = pp * 16;
        if off + 80 > d.len() {
            continue;
        }
        if d[off] == 2 && i + 1 == want {
            return Some(d[off..off + 80].to_vec());
        }
    }
    None
}

fn build_minimal_s3m(rec: &[u8], semitone: u8) -> Vec<u8> {
    let mut hdr = vec![0u8; 96];
    hdr[0..7].copy_from_slice(b"OPLTEST");
    hdr[0x1c] = 0x1a;
    hdr[0x1d] = 16;
    hdr[0x20..0x22].copy_from_slice(&2u16.to_le_bytes());
    hdr[0x22..0x24].copy_from_slice(&1u16.to_le_bytes());
    hdr[0x24..0x26].copy_from_slice(&1u16.to_le_bytes());
    hdr[0x28..0x2a].copy_from_slice(&0x1320u16.to_le_bytes());
    hdr[0x2a..0x2c].copy_from_slice(&2u16.to_le_bytes());
    hdr[0x2c..0x30].copy_from_slice(b"SCRM");
    hdr[0x30] = 64;
    hdr[0x31] = 6;
    hdr[0x32] = 94;
    hdr[0x33] = 192;
    for i in 0..32 {
        hdr[0x40 + i] = 0xff;
    }
    hdr[0x40] = 0x10;
    let instr_off = 7 * 16;
    let mut pat_off = instr_off + 80;
    if pat_off % 16 != 0 {
        pat_off += 16 - (pat_off % 16);
    }
    let pat_para = pat_off / 16;
    let s3m_note = ((semitone / 12) << 4) | (semitone % 12);
    let mut pdata = vec![0x20u8, s3m_note, 1, 0x00];
    pdata.extend(std::iter::repeat_n(0u8, 63));
    let mut pat = (pdata.len() as u16).to_le_bytes().to_vec();
    pat.extend_from_slice(&pdata);
    let mut f = hdr;
    f.extend_from_slice(&[0, 0xff]);
    f.extend_from_slice(&(7u16).to_le_bytes());
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
