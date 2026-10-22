//! Procedural sources: a phase-continuous oscillator, coloured noise, and one-shot buffer
//! synthesis (click, tone) for the old wall-tone / footstep style cues.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Waveform {
    #[default]
    Sine = 0,
    Triangle = 1,
    Square = 2,
    Saw = 3,
}

impl Waveform {
    pub fn from_u8(v: u8) -> Waveform {
        match v {
            1 => Waveform::Triangle,
            2 => Waveform::Square,
            3 => Waveform::Saw,
            _ => Waveform::Sine,
        }
    }
}

/// Frequency changes are phase-continuous — a glissando, never a click.
#[derive(Clone, Copy, Debug, Default)]
pub struct Oscillator {
    phase: f64,
}

impl Oscillator {
    #[inline]
    pub fn next(&mut self, hz: f32, rate: f32, waveform: Waveform) -> f32 {
        let p = self.phase as f32;
        let out = match waveform {
            Waveform::Sine => (p * std::f32::consts::TAU).sin(),
            Waveform::Triangle => 1.0 - 4.0 * (p - 0.5).abs(),
            Waveform::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Waveform::Saw => 2.0 * p - 1.0,
        };
        self.phase += (hz.max(0.0) / rate.max(1.0)) as f64;
        if self.phase >= 1.0 {
            self.phase -= self.phase.floor();
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum NoiseColor {
    #[default]
    White = 0,
    Pink = 1,
    Brown = 2,
}

impl NoiseColor {
    pub fn from_u8(v: u8) -> NoiseColor {
        match v {
            1 => NoiseColor::Pink,
            2 => NoiseColor::Brown,
            _ => NoiseColor::White,
        }
    }
}

/// xorshift white noise; pink via Paul Kellet's economical filter; brown via a leaky integrator.
#[derive(Clone, Copy, Debug)]
pub struct Noise {
    state: u32,
    b: [f32; 7],
    brown: f32,
}

impl Default for Noise {
    fn default() -> Self {
        Noise::new(0x9E37_79B9)
    }
}

impl Noise {
    pub fn new(seed: u32) -> Noise {
        Noise {
            state: if seed == 0 { 1 } else { seed },
            b: [0.0; 7],
            brown: 0.0,
        }
    }

    #[inline]
    fn white(&mut self) -> f32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        (x as f32 / u32::MAX as f32) * 2.0 - 1.0
    }

    #[inline]
    pub fn next(&mut self, color: NoiseColor) -> f32 {
        let w = self.white();
        match color {
            NoiseColor::White => w,
            NoiseColor::Pink => {
                let b = &mut self.b;
                b[0] = 0.99886 * b[0] + w * 0.0555179;
                b[1] = 0.99332 * b[1] + w * 0.0750759;
                b[2] = 0.96900 * b[2] + w * 0.153852;
                b[3] = 0.86650 * b[3] + w * 0.3104856;
                b[4] = 0.55000 * b[4] + w * 0.5329522;
                b[5] = -0.7616 * b[5] - w * 0.0168980;
                let pink = b[0] + b[1] + b[2] + b[3] + b[4] + b[5] + b[6] + w * 0.5362;
                b[6] = w * 0.115926;
                pink * 0.11
            }
            NoiseColor::Brown => {
                self.brown = (self.brown + 0.02 * w) / 1.02;
                self.brown * 3.5
            }
        }
    }
}

/// A short click: a decaying burst, `ms` long. Mono, −1..1.
pub fn click(rate: u32, ms: f32) -> Vec<f32> {
    let n = ((rate as f32 * ms / 1000.0) as usize).max(1);
    let mut osc = Oscillator::default();
    (0..n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let env = (1.0 - t).powi(3);
            osc.next(1800.0 - 1200.0 * t, rate as f32, Waveform::Sine) * env
        })
        .collect()
}

/// A tone with linear attack and release. Mono, −1..1.
pub fn tone(rate: u32, hz: f32, ms: f32, attack_ms: f32, release_ms: f32) -> Vec<f32> {
    let n = ((rate as f32 * ms / 1000.0) as usize).max(1);
    let attack = (rate as f32 * attack_ms / 1000.0).max(1.0);
    let release = (rate as f32 * release_ms / 1000.0).max(1.0);
    let mut osc = Oscillator::default();
    (0..n)
        .map(|i| {
            let a = (i as f32 / attack).min(1.0);
            let r = ((n - i) as f32 / release).min(1.0);
            osc.next(hz, rate as f32, Waveform::Sine) * a.min(r)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oscillator_is_phase_continuous_across_frequency_changes() {
        let mut o = Oscillator::default();
        let mut prev = o.next(440.0, 48000.0, Waveform::Sine);
        for i in 1..2000 {
            let hz = if i < 1000 { 440.0 } else { 880.0 };
            let s = o.next(hz, 48000.0, Waveform::Sine);
            assert!((s - prev).abs() < 0.15, "jump at {i}: {prev} -> {s}");
            prev = s;
        }
    }

    #[test]
    fn noise_stays_in_range_and_colours_differ() {
        let mut n = Noise::default();
        let mut energy = [0.0f32; 3];
        for _ in 0..20000 {
            for (i, c) in [NoiseColor::White, NoiseColor::Pink, NoiseColor::Brown]
                .iter()
                .enumerate()
            {
                let s = n.next(*c);
                assert!(s.abs() <= 1.5, "{c:?} out of range: {s}");
                energy[i] += s * s;
            }
        }
        assert!(
            energy.iter().all(|&e| e > 1.0),
            "all colours produce signal: {energy:?}"
        );
    }

    #[test]
    fn synthesized_buffers_have_the_requested_length_and_fade_out() {
        let c = click(48000, 10.0);
        assert_eq!(c.len(), 480);
        assert!(c.last().unwrap().abs() < 0.01);
        let t = tone(48000, 440.0, 100.0, 5.0, 5.0);
        assert_eq!(t.len(), 4800);
        assert!(t[0].abs() < 0.01 && t[4799].abs() < 0.05);
        assert!(t.iter().any(|&s| s > 0.9));
    }
}
