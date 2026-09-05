//! RBJ "audio EQ cookbook" biquad, transposed direct form II, retunable in place with its
//! state preserved — so a shelf that moves with a source never clicks. At 0 dB a shelf or
//! peak reduces exactly to identity, so a "no cue" filter is genuinely transparent.

use std::f64::consts::PI;

#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    z1: f32,
    z2: f32,
}

impl Default for Biquad {
    fn default() -> Self {
        Biquad::identity()
    }
}

impl Biquad {
    pub const fn identity() -> Biquad {
        Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z1: 0.0, z2: 0.0 }
    }

    pub fn is_identity(&self) -> bool {
        (self.b0 - 1.0).abs() < 1e-6 && self.b1.abs() < 1e-6 && self.b2.abs() < 1e-6 && self.a1.abs() < 1e-6 && self.a2.abs() < 1e-6
    }

    pub fn reset_state(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    fn set(&mut self, b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) {
        self.b0 = (b0 / a0) as f32;
        self.b1 = (b1 / a0) as f32;
        self.b2 = (b2 / a0) as f32;
        self.a1 = (a1 / a0) as f32;
        self.a2 = (a2 / a0) as f32;
    }

    fn w0(rate: f32, hz: f32) -> f64 {
        let hz = (hz as f64).clamp(1.0, rate as f64 * 0.499);
        2.0 * PI * hz / rate as f64
    }

    pub fn set_lowpass(&mut self, rate: f32, hz: f32, q: f32) {
        let w0 = Self::w0(rate, hz);
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q.max(0.05) as f64);
        self.set((1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha);
    }

    pub fn set_highpass(&mut self, rate: f32, hz: f32, q: f32) {
        let w0 = Self::w0(rate, hz);
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q.max(0.05) as f64);
        self.set((1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0, 1.0 + alpha, -2.0 * cos, 1.0 - alpha);
    }

    /// Shelves and peaks at 0 dB become exact identity coefficients (state preserved), so a
    /// "no cue" filter is transparent to the sample.
    fn set_flat(&mut self) {
        self.b0 = 1.0;
        self.b1 = 0.0;
        self.b2 = 0.0;
        self.a1 = 0.0;
        self.a2 = 0.0;
    }

    pub fn set_peak(&mut self, rate: f32, hz: f32, db: f32, q: f32) {
        if db.abs() < 1e-4 {
            return self.set_flat();
        }
        let a = 10f64.powf(db as f64 / 40.0);
        let w0 = Self::w0(rate, hz);
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / (2.0 * q.max(0.05) as f64);
        self.set(1.0 + alpha * a, -2.0 * cos, 1.0 - alpha * a, 1.0 + alpha / a, -2.0 * cos, 1.0 - alpha / a);
    }

    /// Shelf slope S = 1 (the gentlest maximally-flat shelf).
    pub fn set_highshelf(&mut self, rate: f32, hz: f32, db: f32) {
        if db.abs() < 1e-4 {
            return self.set_flat();
        }
        let a = 10f64.powf(db as f64 / 40.0);
        let w0 = Self::w0(rate, hz);
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / 2.0 * 2f64.sqrt();
        let k = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) + (a - 1.0) * cos + k),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * cos),
            a * ((a + 1.0) + (a - 1.0) * cos - k),
            (a + 1.0) - (a - 1.0) * cos + k,
            2.0 * ((a - 1.0) - (a + 1.0) * cos),
            (a + 1.0) - (a - 1.0) * cos - k,
        );
    }

    pub fn set_lowshelf(&mut self, rate: f32, hz: f32, db: f32) {
        if db.abs() < 1e-4 {
            return self.set_flat();
        }
        let a = 10f64.powf(db as f64 / 40.0);
        let w0 = Self::w0(rate, hz);
        let (sin, cos) = (w0.sin(), w0.cos());
        let alpha = sin / 2.0 * 2f64.sqrt();
        let k = 2.0 * a.sqrt() * alpha;
        self.set(
            a * ((a + 1.0) - (a - 1.0) * cos + k),
            2.0 * a * ((a - 1.0) - (a + 1.0) * cos),
            a * ((a + 1.0) - (a - 1.0) * cos - k),
            (a + 1.0) + (a - 1.0) * cos + k,
            -2.0 * ((a - 1.0) + (a + 1.0) * cos),
            (a + 1.0) + (a - 1.0) * cos - k,
        );
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RMS of a filter's response to a sine at `hz`, after settling.
    fn rms_at(f: &mut Biquad, rate: f32, hz: f32) -> f32 {
        let n = 4096;
        let mut sum = 0.0;
        for i in 0..n * 2 {
            let x = (2.0 * std::f32::consts::PI * hz * i as f32 / rate).sin();
            let y = f.process(x);
            if i >= n {
                sum += y * y;
            }
        }
        (sum / n as f32).sqrt()
    }

    #[test]
    fn zero_db_shelves_and_peaks_are_identity() {
        let mut f = Biquad::identity();
        f.set_highshelf(48000.0, 3000.0, 0.0);
        assert!(f.is_identity(), "{f:?}");
        f.set_lowshelf(48000.0, 300.0, 0.0);
        assert!(f.is_identity());
        f.set_peak(48000.0, 1000.0, 0.0, 1.0);
        assert!(f.is_identity());
        for i in 0..10 {
            let x = i as f32 * 0.1;
            assert_eq!(f.process(x), x);
        }
    }

    #[test]
    fn highshelf_cuts_highs_and_leaves_lows() {
        let mut f = Biquad::identity();
        f.set_highshelf(48000.0, 3000.0, -10.0);
        let low = rms_at(&mut f, 48000.0, 200.0);
        f.reset_state();
        let high = rms_at(&mut f, 48000.0, 12000.0);
        let unity = std::f32::consts::FRAC_1_SQRT_2;
        assert!((low - unity).abs() < 0.05, "low band untouched: {low}");
        assert!(high < unity * 0.4, "high band ~-10 dB: {high}");
    }

    #[test]
    fn lowpass_attenuates_above_corner() {
        let mut f = Biquad::identity();
        f.set_lowpass(48000.0, 1000.0, 0.707);
        let pass = rms_at(&mut f, 48000.0, 100.0);
        f.reset_state();
        let stop = rms_at(&mut f, 48000.0, 8000.0);
        assert!(pass > 0.69 && stop < 0.03, "pass {pass}, stop {stop}");
    }
}
