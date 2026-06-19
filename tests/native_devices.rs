//! Native DSP `Device` variants — serde-contract (§0.3) and the
//! `prepare` runtime-reconstruction invariant.
//!
//! These devices keep their *parameters* in the serialised model and
//! their *transient DSP state* (`#[serde(skip)]`: biquad history, delay
//! buffers, envelopes, reverb banks) out of it, rebuilding that state in
//! [`Device::prepare`]. The contract is therefore twofold:
//!
//! 1. A `DeviceChain` holding these devices survives a `serde_json`
//!    round-trip with its parameters intact.
//! 2. Serialising *before* preparing, then deserialising and preparing,
//!    yields a device that renders **bit-identically** to one prepared
//!    from the original — i.e. the runtime state is fully reconstructed
//!    from the serialised parameters, nothing audible is lost in `skip`.

use xmrs::core::daw::device::{Device, DeviceChain, ModDelayMode, ReverbMode};
use xmrs::core::fixed::fixed::Q15;
use xmrs::core::fixed::units::Amp;

fn sample_devices() -> Vec<Device> {
    vec![
        Device::param_eq(2_500, 8, 6_000),
        Device::distortion(-3_000, Q15::from_ratio(55, 100), 2_200, 1_100, 6_500),
        Device::compressor(-18_000, 1_024, 1_500, 120_000, 3_000),
        Device::echo(Q15::HALF, Q15::from_ratio(43, 100), 137, 211, true),
        Device::mod_delay(
            ModDelayMode::Chorus,
            Q15::HALF,
            Q15::from_ratio(61, 100),
            Q15::from_ratio(9, 100),
            2_500,
            true,
            2_048,
            true,
        ),
        Device::reverb(
            ReverbMode::Freeverb,
            Q15::from_ratio(67, 100),
            Q15::from_ratio(37, 100),
            Q15::ONE,
            Q15::from_ratio(37, 100),
            Q15::from_ratio(24, 100),
        ),
        Device::i3dl2_reverb(-500, 1_800, 900, -1_500, 18, -1_200, 35),
        Device::gargle(120, false),
        Device::gargle(40, true),
    ]
}

/// Drive a deterministic stereo test signal (impulse + decaying tone)
/// through a prepared chain and collect the PCM.
fn render(chain: &mut DeviceChain, rate: u32, n: usize) -> Vec<(i16, i16)> {
    chain.prepare(rate);
    let mut out = Vec::with_capacity(n);
    let mut ph: u32 = 0;
    let inc = ((1_000u64 << 32) / rate as u64) as u32; // 1 kHz
    for i in 0..n {
        // impulse at 0, then a 1 kHz tone that decays.
        let env = (32_767i32 - (i as i32).min(32_767)) >> 4;
        let tone = ((ph >> 24) as i32 - 128) * env / 256;
        let s = if i == 0 { 30_000 } else { tone };
        let (l, r) = chain.process((
            Amp::from_q15_i16(s.clamp(-32_767, 32_767) as i16),
            Amp::from_q15_i16((s / 2).clamp(-32_767, 32_767) as i16),
        ));
        out.push((l.as_q15_i16(), r.as_q15_i16()));
        ph = ph.wrapping_add(inc);
    }
    out
}

#[test]
fn native_devices_survive_serde_round_trip() {
    let chain = DeviceChain {
        devices: sample_devices(),
        bypass: false,
    };
    let json = serde_json::to_string(&chain).expect("serialize DeviceChain");
    let back: DeviceChain = serde_json::from_str(&json).expect("deserialize DeviceChain");
    // Parameters are preserved (runtime `#[serde(skip)]` state is at its
    // Default in both — neither has been prepared — so full equality
    // holds here).
    assert_eq!(chain, back, "DeviceChain did not round-trip");
}

#[test]
fn prepare_reconstructs_runtime_state_bit_identically() {
    let rate = 48_000;
    let n = 6_000;

    // Reference: prepare the original chain and render.
    let mut original = DeviceChain {
        devices: sample_devices(),
        bypass: false,
    };
    let reference = render(&mut original, rate, n);

    // Round-trip the *un-prepared* chain, then prepare + render.
    let pristine = DeviceChain {
        devices: sample_devices(),
        bypass: false,
    };
    let json = serde_json::to_string(&pristine).unwrap();
    let mut restored: DeviceChain = serde_json::from_str(&json).unwrap();
    let after = render(&mut restored, rate, n);

    assert_eq!(
        reference, after,
        "a serde round-trip + prepare must render bit-identically (§0.3)"
    );
}

#[test]
fn empty_native_chain_is_inert() {
    // The universal tracker-import case: no inserts ⇒ pass-through even
    // after prepare (guards the bit-identity contract at the device API).
    let mut chain = DeviceChain::default();
    chain.prepare(48_000);
    assert!(!chain.is_active());
    let s = (Amp::from_q15_i16(20_000), Amp::from_q15_i16(-15_000));
    assert_eq!(chain.process(s), s);
}
