// Float math backend (only needed when `std` is disabled).
// Priority: std > libm > micromath.
#[cfg(all(not(feature = "std"), not(feature = "libm"), feature = "micromath"))]
#[allow(unused_imports)]
use micromath::F32Ext;
#[cfg(all(not(feature = "std"), feature = "libm"))]
#[allow(unused_imports)]
use num_traits::float::Float;

#[inline(always)]
pub fn lerp(u: f32, v: f32, t: f32) -> f32 {
    // t * (v - u) + u
    //
    // The earlier `t.mul_add(v - u, u)` was a portability landmine:
    // `f32::mul_add` is only fast when the target has hardware FMA
    // (x86 with `-C target-feature=+fma` / `target-cpu=native`, or
    // an ARMv8 / RISC-V scalable-FP target). Without that — i.e. on
    // every default `cargo build --release` for a generic x86-64
    // target — it falls back to a libc `fmaf` call, which costs a
    // function-call frame and an emulation that's *slower* than a
    // plain mul + add. Profiling on real-world IT modules attributed
    // around 6% of total runtime to `fmaf_with_fma` / `f32::mul_add`
    // because of this.
    //
    // Plain mul + add is at most one ULP off from the FMA result,
    // which is irrelevant for sample interpolation (we already
    // quantise back to f32 before mixing), and the compiler is free
    // to fuse it back into an FMA instruction when the target
    // *does* have hardware support. It's the right default for a
    // crate that wants to be fast without requiring users to set
    // `RUSTFLAGS=-C target-cpu=native`.
    t * (v - u) + u
}

#[allow(unused)]
#[inline(always)]
pub fn inverse_lerp(u: f32, v: f32, lerp: f32) -> f32 {
    (lerp - u) / (v - u)
}

#[inline(always)]
pub fn clamp_up_1f(value: &mut f32, limit: f32) {
    *value = value.min(limit);
}

#[inline(always)]
pub fn clamp_down_1f(value: &mut f32, limit: f32) {
    *value = value.max(limit);
}

#[inline(always)]
pub fn slide_towards(val: &mut f32, goal: f32, incr: f32) {
    if *val > goal {
        *val -= incr;
        clamp_down_1f(val, goal);
    } else if *val < goal {
        *val += incr;
        clamp_up_1f(val, goal);
    }
}
