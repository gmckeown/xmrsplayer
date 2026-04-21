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
    t.mul_add(v - u, u)
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
