//! Pan laws and the interaural cues of the head model (spec §4.4). Constants were tuned by ear
//! in the Wrath of the Righteous mod; every one of them is a parameter here.

/// Lateral fraction −1..1 of a source at (`right`, `forward`) in the listener's frame.
/// Within `pan_width` of the listener, pan tracks absolute sideways offset; beyond it, pure
/// bearing. `exponent < 1` expands the response near the centre (a sharp null at zero, so a
/// cursor can be lined up on a source by ear) and leaves the extremes unchanged.
pub fn lateral(right: f32, forward: f32, pan_width: f32, exponent: f32) -> f32 {
    let dist = (right * right + forward * forward).sqrt();
    if dist <= 1e-4 {
        return 0.0;
    }
    let lat = (right / dist.max(pan_width.max(1e-4))).clamp(-1.0, 1.0);
    if lat == 0.0 { 0.0 } else { lat.signum() * lat.abs().powf(exponent.max(0.05)) }
}

/// Constant-power stereo gains with the interaural level difference capped at `ild_cap_db`
/// (a real head shadows the far ear by 8–20 dB, never to silence, and the far ear must keep
/// signal for the time cue to exist). Power-normalised, so loudness is constant across the arc;
/// centre is 0.707 / 0.707.
pub fn pan_gains(lat: f32, ild_cap_db: f32) -> (f32, f32) {
    let mag = lat.abs().clamp(0.0, 1.0);
    let far = 10f32.powf(-ild_cap_db.max(0.0) * mag / 20.0);
    let norm = 1.0 / (1.0 + far * far).sqrt();
    let (near, far) = (norm, far * norm);
    if lat >= 0.0 { (far, near) } else { (near, far) }
}

/// Plain constant-power pan for listener-relative sources (`pan` −1..1).
pub fn constant_power(pan: f32) -> (f32, f32) {
    let t = (pan.clamp(-1.0, 1.0) + 1.0) * 0.5 * std::f32::consts::FRAC_PI_2;
    (t.cos(), t.sin())
}

/// Interaural time difference in samples (positive = source on the right, left ear lags),
/// Woodworth spherical head: ∝ θ + sin θ with lat ≈ sin θ, normalised to `max_ms` at the side.
pub fn itd_samples(lat: f32, max_ms: f32, rate: f32) -> f32 {
    let s = lat.abs().clamp(0.0, 1.0);
    let theta = s.asin();
    let woodworth_max = std::f32::consts::FRAC_PI_2 + 1.0;
    (max_ms * 1e-3 * rate) * ((theta + s) / woodworth_max) * lat.signum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pan_gains_are_power_constant_and_capped() {
        for lat in [-1.0f32, -0.5, 0.0, 0.3, 1.0] {
            let (l, r) = pan_gains(lat, 12.0);
            assert!((l * l + r * r - 1.0).abs() < 1e-5, "power at {lat}");
        }
        let (l, r) = pan_gains(1.0, 12.0);
        assert!((20.0 * (r / l).log10() - 12.0).abs() < 1e-3, "hard right = exactly the cap");
        assert!(l > 0.2, "far ear keeps signal: {l}");
        let (l, r) = pan_gains(0.0, 12.0);
        assert!((l - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5 && (l - r).abs() < 1e-6);
        let (l, r) = pan_gains(-1.0, 12.0);
        assert!(l > r, "negative = left");
    }

    #[test]
    fn lateral_uses_absolute_offset_near_and_bearing_far() {
        assert_eq!(lateral(0.0, 0.0, 1.0, 1.0), 0.0);
        assert!((lateral(0.5, 0.0, 2.0, 1.0) - 0.25).abs() < 1e-6, "inside pan width: offset / width");
        assert!((lateral(10.0, 0.0, 2.0, 1.0) - 1.0).abs() < 1e-6, "far to the right: hard right");
        assert!((lateral(5.0, 5.0, 1.0, 1.0) - std::f32::consts::FRAC_1_SQRT_2).abs() < 1e-5, "45°: pure bearing");
        assert!(lateral(0.25, 0.0, 1.0, 0.82) > 0.25, "exponent < 1 expands the centre");
        assert!((lateral(1.0, 0.0, 1.0, 0.82) - 1.0).abs() < 1e-6, "extremes unchanged");
    }

    #[test]
    fn itd_is_monotonic_signed_and_bounded() {
        let rate = 48000.0;
        let max = itd_samples(1.0, 0.66, rate);
        assert!((max - 31.68).abs() < 0.1, "0.66 ms at 48 kHz ≈ 31.7 samples: {max}");
        let mut prev = 0.0;
        for i in 1..=10 {
            let v = itd_samples(i as f32 / 10.0, 0.66, rate);
            assert!(v > prev);
            prev = v;
        }
        assert_eq!(itd_samples(-0.5, 0.66, rate), -itd_samples(0.5, 0.66, rate));
        assert_eq!(itd_samples(0.0, 0.66, rate), 0.0);
    }
}
