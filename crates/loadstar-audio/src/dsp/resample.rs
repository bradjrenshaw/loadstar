//! Upload-time conversion: any channel count and rate → mono at the mixer rate.
//!
//! Band-limited (windowed-sinc) interpolation. Linear interpolation was here first and was
//! audible on speech: AVSpeech renders at 22.05 kHz, and stretching that to 48 kHz with straight
//! lines both dulls the top end and leaves images of the voice above 11 kHz — "gritty and dull"
//! by ear. The kernel here passes up to [`PASSBAND`] of the lower rate's Nyquist and stops what
//! lies beyond; [`Streaming`] does the same across chunk boundaries for sources that arrive while
//! they play, sample for sample identical to the whole-buffer path.

use std::f64::consts::PI;
use std::sync::Arc;

/// Zero crossings of the sinc on each side of the centre, at the kernel's cutoff.
const ZERO_CROSSINGS: f64 = 16.0;
/// Kernel table resolution: entries per input sample of distance.
const PHASES: usize = 512;
/// Kaiser window shape (~80 dB stopband with this length).
const KAISER_BETA: f64 = 8.0;
/// Fraction of the lower rate's Nyquist that passes; the rest is the transition band.
const PASSBAND: f64 = 0.92;

/// One-sided kernel: `table[j]` is the filter at distance `j / PHASES` input samples.
struct Kernel {
    /// Half the kernel's span, in input samples.
    half: f64,
    table: Vec<f32>,
}

impl Kernel {
    fn new(ratio: f64) -> Kernel {
        // Cutoff relative to the input's Nyquist: the lower of the two rates sets the band.
        let cutoff = PASSBAND * (1.0 / ratio).min(1.0);
        let half = ZERO_CROSSINGS / cutoff;
        let len = (half * PHASES as f64).ceil() as usize + 2;
        let i0_beta = bessel_i0(KAISER_BETA);
        let table = (0..len)
            .map(|j| {
                let d = j as f64 / PHASES as f64;
                if d >= half {
                    return 0.0;
                }
                let x = cutoff * d;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (PI * x).sin() / (PI * x)
                };
                let r = d / half;
                let window = bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / i0_beta;
                (cutoff * sinc * window) as f32
            })
            .collect();
        Kernel { half, table }
    }

    /// The kernel at `d` input samples from the centre (linear between table entries).
    #[inline]
    fn at(&self, d: f64) -> f32 {
        let p = d.abs() * PHASES as f64;
        let j = p as usize;
        if j + 1 >= self.table.len() {
            return 0.0;
        }
        let f = (p - j as f64) as f32;
        let a = self.table[j];
        a + (self.table[j + 1] - a) * f
    }
}

/// Modified Bessel function of the first kind, order 0 (for the Kaiser window).
fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
    while term > 1e-12 * sum {
        term *= (x / (2.0 * k)).powi(2);
        sum += term;
        k += 1.0;
    }
    sum
}

/// Interleaved `channels` at `from_rate` → mono at `to_rate`.
pub fn to_mono_at_rate(samples: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let mut s = Streaming::new(channels, from_rate, to_rate);
    let mut out = Vec::with_capacity(s.estimate(samples.len() / channels.max(1)));
    s.process(samples, &mut out);
    s.finish(&mut out);
    out
}

/// Chunk-continuous mono resampler: feed interleaved input of any channel count in pieces, get
/// mono output at the target rate with no seam between pieces, then [`Streaming::finish`] for
/// the last few output samples (the kernel looks ahead by half its span).
pub struct Streaming {
    channels: usize,
    /// Input frames per output frame.
    ratio: f64,
    /// `None` = same rate: downmix only.
    kernel: Option<Arc<Kernel>>,
    /// Mono input not yet consumed; `history[0]` is input frame `base`.
    history: Vec<f32>,
    base: u64,
    /// Input frames received so far.
    received: u64,
    /// Output frames produced so far; the next is at input position `produced * ratio`.
    produced: u64,
}

impl Streaming {
    pub fn new(channels: usize, from_rate: u32, to_rate: u32) -> Streaming {
        let ratio = if from_rate == 0 || to_rate == 0 {
            1.0
        } else {
            from_rate as f64 / to_rate as f64
        };
        Streaming {
            channels: channels.max(1),
            ratio,
            kernel: (ratio != 1.0).then(|| Arc::new(Kernel::new(ratio))),
            history: Vec::new(),
            base: 0,
            received: 0,
            produced: 0,
        }
    }

    /// Output frames for `input_frames` of input in total.
    fn estimate(&self, input_frames: usize) -> usize {
        ((input_frames as f64) / self.ratio).round() as usize
    }

    /// Append the resampled mono for one input chunk to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let mono = input
            .chunks_exact(ch)
            .map(|f| f.iter().sum::<f32>() / ch as f32);
        let frames = input.len() / ch;
        self.received += frames as u64;
        if self.kernel.is_none() {
            out.extend(mono);
            self.produced += frames as u64;
            return;
        }
        self.history.extend(mono);
        self.run(out, false);
    }

    /// No more input: produce the remaining output, up to the input's length at the new rate.
    pub fn finish(&mut self, out: &mut Vec<f32>) {
        if self.kernel.is_some() {
            self.run(out, true);
        }
    }

    fn run(&mut self, out: &mut Vec<f32>, finishing: bool) {
        let kernel = Arc::clone(self.kernel.as_ref().unwrap());
        let total = self.estimate(self.received as usize) as u64;
        loop {
            let pos = self.produced as f64 * self.ratio;
            if finishing {
                if self.produced >= total {
                    break;
                }
            } else if pos + kernel.half > (self.received as f64) - 1.0 {
                break; // needs input that has not arrived yet
            }
            let first = (pos - kernel.half).ceil().max(0.0) as u64;
            let last = ((pos + kernel.half).floor() as u64).min(self.received.saturating_sub(1));
            let (mut acc, mut weight) = (0.0f32, 0.0f32);
            for i in first..=last {
                let w = kernel.at(pos - i as f64);
                acc += self.history[(i - self.base) as usize] * w;
                weight += w;
            }
            // Normalised per position: exact unity gain whatever the phase, and at the two
            // ends of the input over just the taps that exist.
            out.push(if weight > 1e-6 { acc / weight } else { 0.0 });
            self.produced += 1;
        }
        // Keep only what the next output can still reach.
        let keep_from = ((self.produced as f64 * self.ratio) - kernel.half)
            .floor()
            .max(0.0) as u64;
        if keep_from > self.base {
            let drop = ((keep_from - self.base) as usize).min(self.history.len());
            self.history.drain(..drop);
            self.base += drop as u64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Magnitude of `x`'s component at `freq` (single DFT bin).
    fn level(x: &[f32], freq: f64, rate: f64) -> f64 {
        let (mut re, mut im) = (0.0, 0.0);
        for (n, s) in x.iter().enumerate() {
            let ph = 2.0 * PI * freq * n as f64 / rate;
            re += *s as f64 * ph.cos();
            im -= *s as f64 * ph.sin();
        }
        (re * re + im * im).sqrt() / x.len() as f64
    }

    fn sine(freq: f64, rate: f64, n: usize) -> Vec<f32> {
        (0..n)
            .map(|i| (2.0 * PI * freq * i as f64 / rate).sin() as f32 * 0.5)
            .collect()
    }

    #[test]
    fn same_rate_only_downmixes() {
        let stereo = [1.0, -1.0, 0.5, 0.5, 0.0, 1.0];
        assert_eq!(
            to_mono_at_rate(&stereo, 2, 48000, 48000),
            vec![0.0, 0.5, 0.5]
        );
    }

    #[test]
    fn lengths_follow_the_rate_ratio() {
        assert_eq!(to_mono_at_rate(&[0.0; 4], 1, 24000, 48000).len(), 8);
        assert_eq!(to_mono_at_rate(&[0.0; 4], 1, 48000, 24000).len(), 2);
        assert_eq!(to_mono_at_rate(&[0.0; 2205], 1, 22050, 48000).len(), 4800);
    }

    #[test]
    fn a_tone_comes_through_clean() {
        // 1 kHz at 22.05 kHz -> 48 kHz: the samples land on the ideal waveform.
        let input = sine(1000.0, 22050.0, 2205);
        let out = to_mono_at_rate(&input, 1, 22050, 48000);
        let ideal = sine(1000.0, 48000.0, out.len());
        let err = out[200..out.len() - 200]
            .iter()
            .zip(&ideal[200..])
            .fold(0.0f32, |m, (a, b)| m.max((a - b).abs()));
        assert!(err < 2e-3, "max error {err}");
    }

    #[test]
    fn upsampling_leaves_no_images() {
        // A 5 kHz tone at 22.05 kHz has its first image at 17.05 kHz once upsampled. Linear
        // interpolation leaves it ~25 dB down (audible grit); the sinc kernel removes it.
        let input = sine(5000.0, 22050.0, 4410);
        let out = to_mono_at_rate(&input, 1, 22050, 48000);
        let body = &out[400..out.len() - 400];
        let tone = level(body, 5000.0, 48000.0);
        let image = level(body, 22050.0 - 5000.0, 48000.0);
        let db = 20.0 * (image / tone).log10();
        assert!(db < -60.0, "image at {db:.1} dB");
        assert!(
            (tone - 0.25).abs() < 0.01,
            "the tone keeps its level: {tone}"
        );
    }

    #[test]
    fn downsampling_does_not_alias() {
        // 20 kHz at 48 kHz is above the 11.025 kHz Nyquist of 22.05 kHz: it must be filtered
        // out, not folded down to 2.05 kHz.
        let input = sine(20000.0, 48000.0, 9600);
        let out = to_mono_at_rate(&input, 1, 48000, 22050);
        let body = &out[400..out.len() - 400];
        let alias = level(body, 22050.0 - 20000.0, 22050.0);
        assert!(alias < 0.25 * 1e-3, "alias level {alias}");
    }

    #[test]
    fn unity_gain_at_dc() {
        let out = to_mono_at_rate(&[0.5; 1000], 1, 22050, 44100);
        assert!(
            out[100..out.len() - 100]
                .iter()
                .all(|x| (x - 0.5).abs() < 1e-4)
        );
    }

    #[test]
    fn streaming_matches_whole_buffer_regardless_of_chunking() {
        for (channels, from, to) in [(1, 22050, 48000), (2, 48000, 22050), (1, 44100, 48000)] {
            let input: Vec<f32> = (0..2000 * channels)
                .map(|i| ((i as f32) * 0.037).sin() * 0.5)
                .collect();
            let whole = to_mono_at_rate(&input, channels, from, to);
            for chunk in [1, 37, 512] {
                let mut s = Streaming::new(channels, from, to);
                let mut out = Vec::new();
                for c in input.chunks(chunk * channels) {
                    s.process(c, &mut out);
                }
                s.finish(&mut out);
                assert_eq!(out.len(), whole.len(), "{from}->{to} chunk {chunk}");
                for (i, (a, b)) in out.iter().zip(&whole).enumerate() {
                    assert!(
                        (a - b).abs() < 1e-6,
                        "{from}->{to} chunk {chunk} sample {i}"
                    );
                }
            }
        }
    }
}
