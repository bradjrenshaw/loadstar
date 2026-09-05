/// A short fractional delay line for interaural time differences: up to 128 samples
/// (0.66 ms is 32 samples at 48 kHz, 64 at 96 kHz), linearly interpolated.
#[derive(Clone, Debug)]
pub struct FracDelay {
    ring: [f32; Self::SIZE],
    pos: usize,
}

impl Default for FracDelay {
    fn default() -> Self {
        FracDelay::new()
    }
}

impl FracDelay {
    pub const SIZE: usize = 128;
    const MASK: usize = Self::SIZE - 1;
    pub const MAX_DELAY: f32 = (Self::SIZE - 2) as f32;

    pub fn new() -> FracDelay {
        FracDelay { ring: [0.0; Self::SIZE], pos: 0 }
    }

    pub fn reset(&mut self) {
        self.ring = [0.0; Self::SIZE];
    }

    /// Push a sample, then read the sample `delay` samples ago (fractional). `delay = 0`
    /// returns the sample just pushed.
    #[inline]
    pub fn push_read(&mut self, x: f32, delay: f32) -> f32 {
        self.ring[self.pos & Self::MASK] = x;
        let d = delay.clamp(0.0, Self::MAX_DELAY);
        let whole = d as usize;
        let frac = d - whole as f32;
        let s0 = self.ring[(self.pos.wrapping_sub(whole)) & Self::MASK];
        let s1 = self.ring[(self.pos.wrapping_sub(whole + 1)) & Self::MASK];
        self.pos = self.pos.wrapping_add(1);
        s0 + (s1 - s0) * frac
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_by_whole_and_fractional_samples() {
        let mut d = FracDelay::new();
        let input = [1.0, 2.0, 3.0, 4.0, 5.0];
        let out: Vec<f32> = input.iter().map(|&x| d.push_read(x, 2.0)).collect();
        assert_eq!(out, vec![0.0, 0.0, 1.0, 2.0, 3.0]);
        let mut d = FracDelay::new();
        let out: Vec<f32> = input.iter().map(|&x| d.push_read(x, 0.5)).collect();
        assert_eq!(out[3], 3.5, "halfway between the last two samples");
    }
}
