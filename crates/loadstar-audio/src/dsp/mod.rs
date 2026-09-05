//! Pure DSP building blocks. Nothing here knows about spaces, sources or threads.

pub mod biquad;
pub mod delay;
pub mod generators;
pub mod pan;
pub mod ramp;
pub mod resample;

pub use biquad::Biquad;
pub use delay::FracDelay;
pub use generators::{Noise, NoiseColor, Oscillator, Waveform, click, tone};
pub use ramp::Ramp;

pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

pub fn gain_to_db(gain: f32) -> f32 {
    20.0 * gain.max(1e-9).log10()
}
