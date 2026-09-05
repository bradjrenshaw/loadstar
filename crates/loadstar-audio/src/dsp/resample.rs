//! Upload-time conversion: any channel count and rate → mono at the mixer rate.
//! Linear interpolation is adequate for cue material; a windowed-sinc can replace it later
//! without touching callers.

/// Interleaved `channels` at `from_rate` → mono at `to_rate`.
pub fn to_mono_at_rate(samples: &[f32], channels: usize, from_rate: u32, to_rate: u32) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let mono: Vec<f32> = (0..frames)
        .map(|f| samples[f * channels..(f + 1) * channels].iter().sum::<f32>() / channels as f32)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn downmixes_and_resamples() {
        let stereo = [1.0, -1.0, 0.5, 0.5, 0.0, 1.0];
        assert_eq!(to_mono_at_rate(&stereo, 2, 48000, 48000), vec![0.0, 0.5, 0.5]);
        let up = to_mono_at_rate(&[0.0, 1.0, 0.0, 1.0], 1, 24000, 48000);
        assert_eq!(up.len(), 8);
        assert!((up[1] - 0.5).abs() < 1e-6, "interpolated midpoint");
        let down = to_mono_at_rate(&[0.0, 0.25, 0.5, 0.75], 1, 48000, 24000);
        assert_eq!(down, vec![0.0, 0.5]);
    }
}
