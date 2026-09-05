/// A parameter with a target it reaches linearly across one block. The audio thread calls
/// [`Ramp::block`] once per period and interpolates; the game thread only ever sets targets.
#[derive(Clone, Copy, Debug)]
pub struct Ramp {
    current: f32,
    target: f32,
}

impl Ramp {
    pub const fn new(value: f32) -> Ramp {
        Ramp { current: value, target: value }
    }

    pub fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    /// Jump both current and target (priming a fresh voice so it does not ramp up from silence).
    pub fn set_now(&mut self, value: f32) {
        self.current = value;
        self.target = value;
    }

    pub fn target(&self) -> f32 {
        self.target
    }

    pub fn current(&self) -> f32 {
        self.current
    }

    pub fn is_settled(&self) -> bool {
        (self.current - self.target).abs() < 1e-6
    }

    /// `(start, per_sample_step)` for a block of `frames`; afterwards the ramp has arrived at
    /// its target, so the next block starts there. Sample `i` of the block uses `start + step * i`.
    pub fn block(&mut self, frames: usize) -> (f32, f32) {
        let start = self.current;
        let step = if frames == 0 { 0.0 } else { (self.target - self.current) / frames as f32 };
        self.current = self.target;
        (start, step)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_linearly_and_arrives() {
        let mut r = Ramp::new(0.0);
        r.set_target(1.0);
        let (start, step) = r.block(4);
        assert_eq!(start, 0.0);
        assert_eq!(step, 0.25);
        assert_eq!(r.current(), 1.0);
        assert!(r.is_settled());
        let (start, step) = r.block(4);
        assert_eq!((start, step), (1.0, 0.0));
    }
}
