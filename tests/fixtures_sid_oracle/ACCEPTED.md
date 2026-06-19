# SID oracle-fidelity — ACCEPTED divergences (do not re-chase)

The `sid_oracle_fidelity_golden` golden freezes divergence counters vs the real
chip (`sidplay`). Not every non-zero counter is a bug. This file lists the
residuals **classified as non-fixable / non-bugs**, with proof, so no effort is
spent on them. Everything else = to be fixed (locate → Ghidra → fix → gate).

## Artefact families (cross-cutting)

- **Arpeggio / alternating effect off by 1 frame.** When an effect alternates a
  register value EVERY frame (octave-arp `frame&1`, fast vibrato), a 1-frame
  capture offset shows up as a **swap** (oracle=A/synth=B then oracle=B/synth=A).
  No integer shift aligns it (every shift yields ~50 % swap). **The VALUES are
  correct** (same A/B), only the dump phase differs. ⇒ accepted.
- **Init capture offset.** `sidplay` dumps one idle frame post-init as frame 0
  and the 1st `play()` as frame 1; our capture dumps the 1st `play()` as frame 0.
  Constant lead of 0..6 frames (tune-dependent), removed by the per-voice
  alignment — the **residual** on alternating effects remains (see above).
  ⇒ accepted.
- **Frame-skip (45.5 vs 50 Hz).** Fractional tempo ⇒ our synth emits the SID
  frames at a rate slightly ≠ 50 Hz. The `synth/oracle` scaling removes it to
  first order; for `drift>window` tunes (non-uniform drift > 8 frames over 16 s)
  a residual remains. ⇒ accepted (≠ value error; audio tempo OK).
- **Static table frames 0-5.** At the very start, the replayer's work-RAM is not
  yet populated; a few freq frames read off RAM=0 = irreducible static garbage
  (import limitation, not playback). ⇒ accepted.
- **Idle-parking.** A voice at rest: the oracle leaves `0x80`/`0x40` (gate-off,
  freq parking) in the registers, we leave `0x00`. **Audible: silence on both
  sides.** ⇒ accepted (register cosmetic).
- **Timing jitter at note transitions.** This is the DOMINANT RESIDUAL of the
  corpus once everything else is set aside. The frame-skip (45.5 vs 50 Hz) +
  sub-row timing make our gate-on/off and instrument switches land 1-2 frames
  off the oracle. Near every transition, the register diverges briefly even
  though **the VALUES are identical on both sides** (proof: the probe shows the
  same AD/SR on oracle AND synth, only the instant changes). A global integer
  shift does not fix it (the offset varies per-note). ⇒ accepted as long as the
  global timing model (frame-exact / timeline-lock) is not redone — it is a
  GLOBAL undertaking, not a per-tune register fix.

## CONCLUSION of the values campaign (2026-06-19)

After the monty fix (the only real audible VALUE bug: clipped envelope + missing
skydive) and the classification of every gate-ON divergence candidate
(`crazy_comets` v1=95, `spellbound` v2=40, `ace_2` v1=12, `thrust` v2, `sanxion`
v1…): **ALL** resolve to timing/transition jitter with **correct values**, or to
the artefacts above. **The SID register values are correct across the whole
corpus.** The residual fidelity gap is UNIFORMLY TIMING (frame-skip). Next useful
investment = the **player's global timing model** (≠ the per-tune
locate→Ghidra→fix loop, which is done on the values side).

## Per tune (module 0, golden 16 s)

| tune | accepted residual | proof |
|------|----------------|--------|
| `zoid#0` v0/v1 | `fHi` (octave-arp 1-frame phase) | probe: oracle 0x68/0x34 ↔ synth 0x34/0x68 swapped; 0x68:0x34 = 2:1 = exact octave; gate/AD/SR = 0 |
| `delta#1`..`#10` (v30) | `gate` ~1000-1500 | **AD/SR ≈ 6 = exact** (so SAME song, mapping good) but the gate bit **toggles every frame** (wave-alt v30, alternating `d404`) → 1-frame phase = ~50 % swap, NON-alignable by integer shift (see "alternating" family). It is the artefact, not a wrong note. |
| `last_v8#0` v0/v2 | `AD`/`SR` (398/248) | **idle-register residue**: probe split by gate = **0 diff gate-ON**, all in gate-OFF. The oracle keeps the previous note's AD/SR in the register of a voice at rest, we keep 0. No gate = no envelope = **inaudible**. Envelopes EXACT when the note plays. (Ghidra confirmed: decodes AD/SR correctly, kill-adsr `bit5&&lengthleft==0` like commando — no bug.) |

⚠️ **Test a sub-song = per-song**: `delta#i` = module `i` = sidplay `--song i+1`. A huge gate
WITH AD/SR≈exact = alternating/phase artefact (accepted); a huge gate WITH AD/SR
ALSO divergent = real problem (wrong note or mapping) → to investigate.

(to be completed as the campaign progresses — only add after probe proof + classification)

## Method (reminder)

1. `cargo run --release --example sid_oracle_probe -- <tune> [song] [secs] [voice]`
   → the probe prints FIRST the **AD/SR split by gate** (gateON = real audible
   bug, gateOFF = accepted idle-residue), then the per-register counters, then
   (4th arg = voice) a frame-by-frame trace.
2. **Look at the gate split FIRST.** If a divergence is ~100 % gate-OFF =
   idle-residue (lingering register of a voice at rest, inaudible) → accepted, STOP,
   **no Ghidra** (avoided implementing a useless "bit5" feature for last_v8).
   Otherwise classify: real bug (gate-ON diff) / alternating artefact /
   `drift>window`.
3. If real bug: Ghidra the replayer, fix, `cargo test --test cycle_exact_pcm
   -- --ignored sid_oracle_fidelity`; any `+metric` line = regression →
   Ghidra the regressed one.
4. If artefact: add it here, never touch it again.
