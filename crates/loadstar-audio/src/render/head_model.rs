//! The built-in renderer (spec §4.4): capped-ILD constant-power pan, Woodworth ITD on the far
//! ear, a laterality-scaled high-shelf head shadow on the far ear, a rear high-shelf cut for
//! front/back, and an optional elevation tilt. Every cue is a flag; every constant a parameter.

use super::{Placement, Renderer, SpatializerParams, VoiceRenderer, flags};
use crate::dsp::{Biquad, FracDelay, Ramp, pan};

pub struct HeadModel;

impl Renderer for HeadModel {
    fn name(&self) -> &'static str {
        "head-model"
    }

    fn make_voice(&self, rate: f32, _block: usize) -> Box<dyn VoiceRenderer> {
        Box::new(HeadVoice::new(rate))
    }
}

struct Shelf {
    filter: Biquad,
    db: f32,
    corner: f32,
}

impl Shelf {
    fn new() -> Shelf {
        Shelf {
            filter: Biquad::identity(),
            db: 0.0,
            corner: 0.0,
        }
    }

    /// Move halfway to the target once per block; retune only when it matters.
    fn approach(&mut self, rate: f32, corner: f32, target_db: f32, prime: bool) {
        let next = if prime {
            target_db
        } else {
            self.db + (target_db - self.db) * 0.5
        };
        if prime || (next - self.db).abs() > 0.05 || (corner - self.corner).abs() > 1.0 {
            self.db = if (next - target_db).abs() < 0.05 {
                target_db
            } else {
                next
            };
            self.corner = corner;
            self.filter.set_highshelf(rate, corner, self.db);
        }
    }
}

pub struct HeadVoice {
    rate: f32,
    gain_l: Ramp,
    gain_r: Ramp,
    itd: Ramp,
    rear: Shelf,
    shadow: Shelf,
    elevation: Shelf,
    delay: FracDelay,
    primed: bool,
}

impl HeadVoice {
    pub fn new(rate: f32) -> HeadVoice {
        HeadVoice {
            rate,
            gain_l: Ramp::new(0.0),
            gain_r: Ramp::new(0.0),
            itd: Ramp::new(0.0),
            rear: Shelf::new(),
            shadow: Shelf::new(),
            elevation: Shelf::new(),
            delay: FracDelay::new(),
            primed: false,
        }
    }

    /// The four cue targets for a placement: (gain_l, gain_r, itd_samples, shadow_db, rear_db, elev_db).
    fn targets(&self, p: &Placement, params: &SpatializerParams) -> (f32, f32, f32, f32, f32, f32) {
        if p.relative {
            let (l, r) = pan::constant_power(p.pan);
            return (l, r, 0.0, 0.0, 0.0, 0.0);
        }
        if p.inside {
            let (l, r) = pan::pan_gains(0.0, params.ild_cap_db);
            return (l, r, 0.0, 0.0, 0.0, 0.0);
        }
        let lat = pan::lateral(p.right, p.forward, p.pan_width, params.pan_exponent);
        let (l, r) = pan::pan_gains(lat, params.ild_cap_db);
        let itd = if params.has(flags::ITD) {
            pan::itd_samples(lat, params.itd_max_ms, self.rate)
        } else {
            0.0
        };
        let shadow = if params.has(flags::SHADOW) {
            -params.shadow_max_db * lat.abs()
        } else {
            0.0
        };
        let horizontal = (p.right * p.right + p.forward * p.forward).sqrt();
        let rear = if params.has(flags::REAR) && horizontal > 1e-4 {
            let forward_frac = (p.forward / horizontal).clamp(-1.0, 1.0);
            if forward_frac < 0.0 {
                params.rear_max_db * forward_frac
            } else {
                0.0
            }
        } else {
            0.0
        };
        let elev = if params.has(flags::ELEVATION) && p.distance > 1e-4 {
            params.elevation_max_db * (p.up / p.distance).clamp(-1.0, 1.0)
        } else {
            0.0
        };
        (l, r, itd, shadow, rear, elev)
    }
}

impl VoiceRenderer for HeadVoice {
    fn process(
        &mut self,
        mono: &[f32],
        placement: &Placement,
        params: &SpatializerParams,
        out: &mut [f32],
    ) {
        let frames = mono.len().min(out.len() / 2);
        if frames == 0 {
            return;
        }
        let (gl, gr, itd, shadow_db, rear_db, elev_db) = self.targets(placement, params);
        let prime = !self.primed;
        if prime {
            self.gain_l.set_now(gl);
            self.gain_r.set_now(gr);
            self.itd.set_now(itd);
            self.primed = true;
        } else {
            self.gain_l.set_target(gl);
            self.gain_r.set_target(gr);
            self.itd.set_target(itd);
        }
        self.rear
            .approach(self.rate, params.rear_corner_hz, rear_db, prime);
        self.shadow
            .approach(self.rate, params.shadow_corner_hz, shadow_db, prime);
        self.elevation.approach(self.rate, 4000.0, elev_db, prime);

        let (l0, ls) = self.gain_l.block(frames);
        let (r0, rs) = self.gain_r.block(frames);
        let (d0, ds) = self.itd.block(frames);

        for i in 0..frames {
            let gl = l0 + ls * i as f32;
            let gr = r0 + rs * i as f32;
            let itd = d0 + ds * i as f32;

            // Whole-source cues first (mono): rear darkening, elevation tilt.
            let m = self
                .elevation
                .filter
                .process(self.rear.filter.process(mono[i]));
            // Far ear: delayed, then head-shadowed. The near ear is the undelayed signal.
            let far = self
                .shadow
                .filter
                .process(self.delay.push_read(m, itd.abs()));
            let near = m;
            // Positive ITD = source on the right = the left ear is the far one. When the sign
            // flips mid-flight the roles swap at itd ≈ 0 with equal gains and ~0 dB shadow.
            let (left, right) = if itd >= 0.0 { (far, near) } else { (near, far) };
            out[2 * i] = left * gl;
            out[2 * i + 1] = right * gr;
        }
    }

    fn reset(&mut self) {
        self.primed = false;
        self.delay.reset();
        self.rear.filter.reset_state();
        self.shadow.filter.reset_state();
        self.elevation.filter.reset_state();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: f32 = 48000.0;
    const BLOCK: usize = 256;

    fn place(right: f32, forward: f32) -> Placement {
        Placement {
            right,
            up: 0.0,
            forward,
            distance: (right * right + forward * forward).sqrt(),
            inside: false,
            pan_width: 1.0,
            relative: false,
            pan: 0.0,
        }
    }

    fn render(voice: &mut HeadVoice, p: &Placement, hz: f32, blocks: usize) -> Vec<f32> {
        let params = SpatializerParams::default();
        let mut out = vec![0.0; BLOCK * 2];
        let mut all = Vec::new();
        let mut phase = 0.0f32;
        for _ in 0..blocks {
            let mono: Vec<f32> = (0..BLOCK)
                .map(|_| {
                    phase += hz / RATE;
                    (phase * std::f32::consts::TAU).sin()
                })
                .collect();
            voice.process(&mono, p, &params, &mut out);
            all.extend_from_slice(&out);
        }
        all
    }

    fn channel_rms(stereo: &[f32], channel: usize) -> f32 {
        let n = stereo.len() / 2;
        (stereo
            .iter()
            .skip(channel)
            .step_by(2)
            .map(|s| s * s)
            .sum::<f32>()
            / n as f32)
            .sqrt()
    }

    #[test]
    fn right_of_the_listener_is_louder_on_the_right_but_not_silent_on_the_left() {
        let mut v = HeadVoice::new(RATE);
        let out = render(&mut v, &place(5.0, 0.0), 440.0, 8);
        let (l, r) = (channel_rms(&out, 0), channel_rms(&out, 1));
        assert!(r > l * 2.0, "right louder: l={l} r={r}");
        assert!(l > 0.1, "far ear keeps signal (ILD cap): {l}");
    }

    #[test]
    fn left_mirrors_right_and_centre_is_equal() {
        let mut v = HeadVoice::new(RATE);
        let left = render(&mut v, &place(-5.0, 0.0), 440.0, 8);
        let mut v = HeadVoice::new(RATE);
        let right = render(&mut v, &place(5.0, 0.0), 440.0, 8);
        assert!((channel_rms(&left, 0) - channel_rms(&right, 1)).abs() < 1e-3);
        let mut v = HeadVoice::new(RATE);
        let centre = render(&mut v, &place(0.0, 5.0), 440.0, 8);
        assert!((channel_rms(&centre, 0) - channel_rms(&centre, 1)).abs() < 1e-3);
    }

    #[test]
    fn behind_is_darker_than_ahead() {
        let mut v = HeadVoice::new(RATE);
        let ahead = render(&mut v, &place(0.0, 5.0), 8000.0, 8);
        let mut v = HeadVoice::new(RATE);
        let behind = render(&mut v, &place(0.0, -5.0), 8000.0, 8);
        let (a, b) = (channel_rms(&ahead, 0), channel_rms(&behind, 0));
        assert!(
            b < a * 0.5,
            "8 kHz behind is at least ~6 dB down: ahead={a} behind={b}"
        );
        let mut v = HeadVoice::new(RATE);
        let ahead_low = render(&mut v, &place(0.0, 5.0), 200.0, 8);
        let mut v = HeadVoice::new(RATE);
        let behind_low = render(&mut v, &place(0.0, -5.0), 200.0, 8);
        assert!(
            (channel_rms(&ahead_low, 0) - channel_rms(&behind_low, 0)).abs() < 0.05,
            "lows pass the rear shelf"
        );
    }

    #[test]
    fn far_ear_is_delayed_when_itd_is_on() {
        let mut v = HeadVoice::new(RATE);
        let params = SpatializerParams::default();
        let mut mono = vec![0.0; BLOCK];
        mono[10] = 1.0;
        let mut out = vec![0.0; BLOCK * 2];
        v.process(&mono, &place(5.0, 0.0), &params, &mut out);
        let right_peak = (0..BLOCK)
            .max_by(|&a, &b| out[2 * a + 1].partial_cmp(&out[2 * b + 1]).unwrap())
            .unwrap();
        let left_peak = (0..BLOCK)
            .max_by(|&a, &b| out[2 * a].partial_cmp(&out[2 * b]).unwrap())
            .unwrap();
        assert_eq!(right_peak, 10, "near ear undelayed");
        assert!(
            (40..=43).contains(&left_peak),
            "far ear lags ~31.7 samples: {left_peak}"
        );
    }

    #[test]
    fn a_moving_source_never_jumps() {
        let mut v = HeadVoice::new(RATE);
        let params = SpatializerParams::default();
        let mut out = vec![0.0; BLOCK * 2];
        let mut prev = [0.0f32; 2];
        let mut phase = 0.0f32;
        for step in 0..60 {
            // Sweep from hard left, through ahead and behind, to hard right over 60 blocks.
            let angle = -std::f32::consts::PI + step as f32 * (2.0 * std::f32::consts::PI / 60.0);
            let p = place(5.0 * angle.sin(), 5.0 * angle.cos());
            let mono: Vec<f32> = (0..BLOCK)
                .map(|_| {
                    phase += 300.0 / RATE;
                    (phase * std::f32::consts::TAU).sin()
                })
                .collect();
            v.process(&mono, &p, &params, &mut out);
            for i in 0..BLOCK {
                for c in 0..2 {
                    let s = out[2 * i + c];
                    if step > 0 || i > 0 {
                        assert!(
                            (s - prev[c]).abs() < 0.08,
                            "jump at block {step} sample {i} ch {c}: {} -> {s}",
                            prev[c]
                        );
                    }
                    prev[c] = s;
                }
            }
        }
    }

    #[test]
    fn relative_and_inside_placements_are_centred_or_panned_plainly() {
        let mut v = HeadVoice::new(RATE);
        let inside = Placement {
            inside: true,
            ..place(3.0, 0.0)
        };
        let out = render(&mut v, &inside, 440.0, 4);
        assert!((channel_rms(&out, 0) - channel_rms(&out, 1)).abs() < 1e-3);
        let mut v = HeadVoice::new(RATE);
        let out = render(&mut v, &Placement::relative(1.0), 440.0, 4);
        assert!(
            channel_rms(&out, 0) < 1e-3 && channel_rms(&out, 1) > 0.5,
            "hard right, no cap for relative sources"
        );
    }
}
