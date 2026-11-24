//! glibc compatibility. glibc 2.43 gave `acosf` and `atan2f` new symbol versions, so a binary
//! linked on a current system would refuse to start on older ones (Debian 13 has 2.41). Defining
//! them here keeps the binary on symbols every supported glibc has: they compute in f64 through
//! `acos`/`atan2`, whose versions never changed. `just check-glibc` verifies the result.

#[unsafe(no_mangle)]
pub extern "C" fn acosf(x: f32) -> f32 {
    f64::from(x).acos() as f32
}

#[unsafe(no_mangle)]
pub extern "C" fn atan2f(y: f32, x: f32) -> f32 {
    f64::from(y).atan2(f64::from(x)) as f32
}

#[cfg(test)]
mod tests {
    #[test]
    fn math_shims() {
        // std::hint::black_box keeps the calls from being constant-folded away.
        let half = std::hint::black_box(0.5f32);
        assert!((half.acos() - std::f32::consts::FRAC_PI_3).abs() < 1e-6);
        assert!((std::hint::black_box(1.0f32).atan2(std::hint::black_box(-1.0)) - 3.0 * std::f32::consts::FRAC_PI_4).abs() < 1e-6);
    }
}
