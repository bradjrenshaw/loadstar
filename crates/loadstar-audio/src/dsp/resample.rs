//! Upload-time conversion: any channel count and rate → mono at the mixer rate.
//! Linear interpolation is adequate for cue material; a windowed-sinc can replace it later
//! without touching callers. [`Streaming`] does the same across chunk boundaries for sources
//! that arrive while they play.

/// Interleaved `channels` at `from_rate` → mono at `to_rate`.
pub fn to_mono_at_rate(samples: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let mono: Vec<f32> = (0..frames)
        .map(|f| {
            samples[f * channels..(f + 1) * channels]
                .iter()
                .sum::<f32>()
                / channels as f32
        })
        .collect();
    if from_rate == to_rate || from_rate == 0 || to_rate == 0 || mono.len() < 2 {
        return mono;
    }
    let ratio = from_rate as f64 / to_rate as f64;
    let out_len = ((mono.len() as f64) / ratio).round().max(1.0) as usize;
    (0..out_len)
        .map(|i| {
            let src = i as f64 * ratio;
            let idx = src as usize;
            let frac = (src - idx as f64) as f32;
            let a = mono[idx.min(mono.len() - 1)];
            let b = mono[(idx + 1).min(mono.len() - 1)];
            a + (b - a) * frac
        })
        .collect()
}

/// Chunk-continuous mono resampler: feed interleaved input of any channel count in pieces and
/// get mono output at the target rate with no seam between pieces.
pub struct Streaming {
    channels: usize,
    ratio: f64,
    /// Position of the next output sample in input-frame units, where frame −1 is `prev`.
    pos: f64,
    prev: f32,
    has_prev: bool,
}

impl Streaming {
    pub fn new(channels: usize, from_rate: u32, to_rate: u32) -> Streaming {
        Streaming {
            channels: channels.max(1),
            ratio: if from_rate == 0 || to_rate == 0 {
                1.0
            } else {
                from_rate as f64 / to_rate as f64
            },
            pos: 0.0,
            prev: 0.0,
            has_prev: false,
        }
    }

    /// Append the resampled mono for one input chunk to `out`.
    pub fn process(&mut self, input: &[f32], out: &mut Vec<f32>) {
        let ch = self.channels;
        let frames = input.len() / ch;
        if frames == 0 {
            return;
        }
        let mono =
            |f: usize| -> f32 { input[f * ch..(f + 1) * ch].iter().sum::<f32>() / ch as f32 };
        if !self.has_prev {
            self.prev = mono(0);
            self.has_prev = true;
        }
        let sample = |i: isize| -> f32 { if i < 0 { self.prev } else { mono(i as usize) } };
        // Output while both neighbours of the position exist (the right neighbour must be a real frame).
        while (self.pos.floor() as isize) < frames as isize - 1 {
            let idx = self.pos.floor() as isize;
            let frac = (self.pos - idx as f64) as f32;
            let a = sample(idx);
            let b = sample(idx + 1);
            out.push(a + (b - a) * frac);
            self.pos += self.ratio;
        }
        self.prev = mono(frames - 1);
        self.pos -= frames as f64; // frame `frames-1` becomes frame −1 for the next chunk
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_and_resamples() {
        let stereo = [1.0, -1.0, 0.5, 0.5, 0.0, 1.0];
        assert_eq!(
            to_mono_at_rate(&stereo, 2, 48000, 48000),
            vec![0.0, 0.5, 0.5]
        );
        let up = to_mono_at_rate(&[0.0, 1.0, 0.0, 1.0], 1, 24000, 48000);
        assert_eq!(up.len(), 8);
        assert!((up[1] - 0.5).abs() < 1e-6, "interpolated midpoint");
        let down = to_mono_at_rate(&[0.0, 0.25, 0.5, 0.75], 1, 48000, 24000);
        assert_eq!(down, vec![0.0, 0.5]);
    }

    #[test]
    fn streaming_matches_whole_buffer_regardless_of_chunking() {
        let input: Vec<f32> = (0..200).map(|i| ((i as f32) * 0.07).sin()).collect();
        let whole = to_mono_at_rate(&input, 1, 22050, 48000);
        let mut s = Streaming::new(1, 22050, 48000);
        let mut out = Vec::new();
        for chunk in input.chunks(37) {
            s.process(chunk, &mut out);
        }
        assert!(out.len() >= whole.len() - 3 && out.len() <= whole.len());
        for (i, (a, b)) in out.iter().zip(whole.iter()).enumerate() {
            assert!((a - b).abs() < 1e-5, "sample {i}: {a} vs {b}");
        }
        // Downsampling and stereo too.
        let mut s = Streaming::new(2, 48000, 24000);
        let mut out = Vec::new();
        for chunk in [1.0, 1.0, 0.0, 0.0, 1.0, 1.0, 0.0, 0.0].chunks(2) {
            s.process(chunk, &mut out);
        }
        assert_eq!(out, vec![1.0, 1.0]);
    }
}
