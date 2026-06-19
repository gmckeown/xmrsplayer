//! Generate the compact OPL golden fixture `tests/fixtures_corpus/opl_min.s3m`.
//!
//! The full `and_this_is_ultrasound.s3m` (≈1 MB) is too large to bundle, so
//! this builds a small representative S3M that still exercises the whole FM
//! path: two AdLib voices (one FM-with-feedback, one additive), several note
//! triggers, a sustain, and note-cuts that drive the release phase — enough
//! surface for the golden hash to catch any drift in the operator, envelope
//! or LFO. Run once to (re)create the fixture:
//!
//! ```text
//! cargo run --release --example gen_opl_fixture -- ~/Music/s3m/and_this_is_ultrasound.s3m
//! ```

use std::path::PathBuf;

fn main() {
    let src_path = std::env::args().nth(1).unwrap_or_else(|| {
        format!(
            "{}/Music/s3m/and_this_is_ultrasound.s3m",
            std::env::var("HOME").unwrap_or_default()
        )
    });
    let src = std::fs::read(&src_path).expect("read source OPL module");
    let opl = find_opl_instruments(&src);
    assert!(
        opl.len() >= 2,
        "need ≥2 OPL instruments, found {}",
        opl.len()
    );

    // Pick two contrasting patches: the first feedback-FM voice and the first
    // additive (connection-bit) voice, falling back to the first two.
    // SCRI 80-byte record: 12 OPL bytes at 0x10..0x1C; D0A (offset 0x1A) is
    // the feedback/connection byte, bit0 = connection (1 = additive).
    let is_additive = |rec: &[u8]| rec[0x1a] & 1 == 1;
    let fm = opl
        .iter()
        .find(|(_, r, _)| !is_additive(r))
        .unwrap_or(&opl[0]);
    let add = opl
        .iter()
        .find(|(_, r, _)| is_additive(r))
        .unwrap_or(&opl[1]);
    let recs = [fm.1.clone(), add.1.clone()];

    let s3m = build_opl_s3m(&recs);
    let out = fixture_path();
    std::fs::write(&out, &s3m).expect("write fixture");
    println!("wrote {} ({} bytes)", out.display(), s3m.len());
}

fn fixture_path() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    p.push("tests/fixtures_corpus/opl_min.s3m");
    p
}

/// Find every SCRI (AdLib melody) instrument: `(1-based index, 80-byte record, name)`.
fn find_opl_instruments(d: &[u8]) -> Vec<(usize, Vec<u8>, String)> {
    let ordnum = u16::from_le_bytes([d[0x20], d[0x21]]) as usize;
    let insnum = u16::from_le_bytes([d[0x22], d[0x23]]) as usize;
    let ipp = 0x60 + ordnum;
    let mut out = Vec::new();
    for i in 0..insnum {
        let pp = u16::from_le_bytes([d[ipp + 2 * i], d[ipp + 2 * i + 1]]) as usize;
        let off = pp * 16;
        if off + 80 > d.len() {
            continue;
        }
        if d[off] == 2 {
            let rec = d[off..off + 80].to_vec();
            let name = String::from_utf8_lossy(&d[off + 0x30..off + 0x4c])
                .trim_matches(|c: char| c == '\0' || c.is_whitespace())
                .to_string();
            out.push((i + 1, rec, name));
        }
    }
    out
}

/// Build a 2-AdLib-channel S3M, 2 instruments, one 32-row pattern exercising
/// trigger / sustain / note-cut(release) / retrigger on both voices.
fn build_opl_s3m(recs: &[Vec<u8>]) -> Vec<u8> {
    const ROWS: usize = 32;
    let note = |oct: u8, semi: u8| (oct << 4) | semi; // S3M packed note
    const CUT: u8 = 254; // S3M note-cut (^^) → key-off / release

    // (row, channel, note, instr) events. instr is 1-based.
    let events: &[(usize, u8, u8, u8)] = &[
        (0, 0, note(4, 0), 1),  // ch0 C-4, FM voice
        (0, 1, note(4, 7), 2),  // ch1 G-4, additive voice
        (8, 0, CUT, 1),         // ch0 release
        (12, 1, CUT, 2),        // ch1 release
        (16, 0, note(5, 0), 1), // ch0 retrigger an octave up
        (16, 1, note(3, 9), 2), // ch1 retrigger low
        (28, 0, CUT, 1),
        (28, 1, CUT, 2),
    ];

    // ---- pattern body (packed S3M rows) ----
    let mut pdata: Vec<u8> = Vec::new();
    for row in 0..ROWS {
        for &(_, ch, n, ins) in events.iter().filter(|e| e.0 == row) {
            pdata.push(0x20 | ch); // note + instrument present on this channel
            pdata.push(n);
            pdata.push(ins);
        }
        pdata.push(0x00); // end of row
    }
    let mut pat = (pdata.len() as u16).to_le_bytes().to_vec();
    pat.extend_from_slice(&pdata);

    // ---- header ----
    let mut hdr = vec![0u8; 96];
    hdr[0..7].copy_from_slice(b"OPLGOLD");
    hdr[0x1c] = 0x1a;
    hdr[0x1d] = 16;
    hdr[0x20..0x22].copy_from_slice(&2u16.to_le_bytes()); // ordnum
    hdr[0x22..0x24].copy_from_slice(&(recs.len() as u16).to_le_bytes()); // insnum
    hdr[0x24..0x26].copy_from_slice(&1u16.to_le_bytes()); // patnum
    hdr[0x28..0x2a].copy_from_slice(&0x1320u16.to_le_bytes()); // cwtv
    hdr[0x2a..0x2c].copy_from_slice(&2u16.to_le_bytes()); // ffi = unsigned
    hdr[0x2c..0x30].copy_from_slice(b"SCRM");
    hdr[0x30] = 64; // global volume
    hdr[0x31] = 6; // speed
    hdr[0x32] = 94; // tempo
    hdr[0x33] = 192; // master volume
    for i in 0..32 {
        hdr[0x40 + i] = 0xff; // all channels off…
    }
    hdr[0x40] = 0x10; // …ch0 = AdLib melody voice 1
    hdr[0x41] = 0x11; // …ch1 = AdLib melody voice 2

    // ---- layout: header | orders | instr-parapointers | pattern-parapointer
    //              | instrument records | pattern ----
    let paraptr_block = 0x60usize; // after 96-byte header
    let orders_len = 2usize; // [0, 0xFF]
    let ins_pp_at = paraptr_block + orders_len;
    let pat_pp_at = ins_pp_at + 2 * recs.len();

    // Instrument records start on the next 16-byte paragraph after the pointers.
    let mut cur = pat_pp_at + 2;
    if !cur.is_multiple_of(16) {
        cur += 16 - (cur % 16);
    }
    let mut ins_paras = Vec::new();
    for _ in recs {
        ins_paras.push((cur / 16) as u16);
        cur += 80;
    }
    if !cur.is_multiple_of(16) {
        cur += 16 - (cur % 16);
    }
    let pat_para = (cur / 16) as u16;

    // ---- assemble ----
    let mut f = hdr;
    f.extend_from_slice(&[0, 0xff]); // orders
    for p in &ins_paras {
        f.extend_from_slice(&p.to_le_bytes());
    }
    f.extend_from_slice(&pat_para.to_le_bytes());
    for (i, rec) in recs.iter().enumerate() {
        let off = ins_paras[i] as usize * 16;
        while f.len() < off {
            f.push(0);
        }
        f.extend_from_slice(rec);
    }
    let pat_off = pat_para as usize * 16;
    while f.len() < pat_off {
        f.push(0);
    }
    f.extend_from_slice(&pat);
    f
}
