//! What a speech source implements (speech-spec §4, §5). The worker owns the [`Backend`]s; the
//! render thread owns the [`RenderBackend`]s. Both are plain traits so a test can substitute a
//! deterministic fake (`backends::fake`).

use std::time::Duration;

/// An instance handle within a backend: which synthesizer object is speaking a given utterance.
/// Single-channel backends (screen readers) always return 0.
pub type Slot = u32;

/// Per-utterance voice parameters, normalized 0..1 (0.5 = the voice's default for rate/pitch).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Params {
    pub rate: f32,
    pub pitch: f32,
    pub volume: f32,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            rate: 0.5,
            pitch: 0.5,
            volume: 1.0,
        }
    }
}

/// Rendered speech: interleaved f32.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pcm {
    pub samples: Vec<f32>,
    pub channels: u32,
    pub rate: u32,
}

impl Pcm {
    pub fn frames(&self) -> usize {
        if self.channels == 0 {
            0
        } else {
            self.samples.len() / self.channels as usize
        }
    }

    pub fn duration(&self) -> Duration {
        if self.rate == 0 {
            return Duration::ZERO;
        }
        Duration::from_secs_f64(self.frames() as f64 / self.rate as f64)
    }

    /// Mono at `rate`, linear interpolation. Runs on the worker, never on the game thread.
    pub fn mono_at(&self, rate: u32) -> Vec<f32> {
        let channels = self.channels.max(1) as usize;
        let frames = self.samples.len() / channels;
        let mono: Vec<f32> = (0..frames)
            .map(|f| {
                self.samples[f * channels..(f + 1) * channels]
                    .iter()
                    .sum::<f32>()
                    / channels as f32
            })
            .collect();
        if self.rate == rate || self.rate == 0 || rate == 0 || mono.len() < 2 {
            return mono;
        }
        let ratio = self.rate as f64 / rate as f64;
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

    /// Drop leading and trailing near-silence (below `threshold`), keeping a short tail.
    pub fn trim_silence(&mut self, threshold: f32) {
        let ch = self.channels.max(1) as usize;
        let frames = self.frames();
        let loud = |f: usize| {
            self.samples[f * ch..(f + 1) * ch]
                .iter()
                .any(|s| s.abs() > threshold)
        };
        let Some(first) = (0..frames).find(|&f| loud(f)) else {
            self.samples.clear();
            return;
        };
        let last = (0..frames).rev().find(|&f| loud(f)).unwrap_or(first);
        let keep_tail = (self.rate as usize / 20).min(frames - 1 - last); // 50 ms
        let end = (last + 1 + keep_tail) * ch;
        self.samples.drain(end..);
        self.samples.drain(..first * ch);
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BackendError {
    /// The backend or voice is not reachable right now (reader not running, COM object gone).
    Unavailable,
    /// The operation is not in this leaf's feature set.
    NotSupported,
    Failed(String),
}

/// A leaf as a backend reports it from `probe`.
#[derive(Clone, Debug, PartialEq)]
pub struct LeafSpec {
    /// Category key (`screen_readers`, `sapi5`, …). Unknown keys create a category.
    pub category: String,
    /// Display name and rank for a category the built-in table does not know.
    pub category_name: Option<String>,
    pub category_rank: Option<i32>,
    pub name: String,
    /// BCP-47 where the backend knows it, else empty.
    pub language: String,
    /// The backend's own stable identifier for this voice; hashed into the leaf id.
    pub native_id: String,
    /// Leaves with the same dedupe key collapse to the first one probed (native before Prism).
    pub dedupe: Option<String>,
    pub features: u64,
    /// What the backend wants back in `speak`/`render` to pick this voice.
    pub selector: u64,
    pub is_default: bool,
    pub available: bool,
}

/// A source of live speech. Lives on the worker thread.
pub trait Backend: Send {
    fn name(&self) -> &'static str;

    /// Enumerate (and re-check availability of) everything this backend can speak through.
    fn probe(&mut self) -> Vec<LeafSpec>;

    /// How often the worker should re-probe this backend on its own; None = only on request.
    fn reprobe_every(&self) -> Option<Duration> {
        None
    }

    /// Start speaking. `output` asks for braille too where supported. Returns the slot to poll.
    fn speak(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        interrupt: bool,
        output: bool,
        ssml: bool,
    ) -> Result<Slot, BackendError>;

    fn braille(&mut self, selector: u64, text: &str) -> Result<(), BackendError>;

    /// Cut one slot, or everything this backend is saying.
    fn stop(&mut self, slot: Option<Slot>);

    fn pause(&mut self, _slot: Slot) -> Result<(), BackendError> {
        Err(BackendError::NotSupported)
    }

    fn resume(&mut self, _slot: Slot) -> Result<(), BackendError> {
        Err(BackendError::NotSupported)
    }

    /// `Some(true)` still speaking, `Some(false)` finished, `None` = this backend cannot tell
    /// (the utterance is reported Done at hand-over).
    fn speaking(&mut self, slot: Slot) -> Option<bool>;

    /// How often `speaking` is worth asking while a slot is in flight; None = never.
    fn poll_interval(&self) -> Option<Duration> {
        None
    }

    /// The worker is done with a slot (finished or cancelled).
    fn release(&mut self, _slot: Slot) {}
}

/// Where streamed render output goes (speech-spec §6, streaming). Called from whatever thread the
/// synthesizer produces audio on, chunk by chunk, as soon as each exists.
pub trait StreamSink: Send + Sync {
    /// Interleaved f32 at the backend's native rate. `false` = stop, the consumer is gone.
    fn write(&self, samples: &[f32], channels: u32, rate: u32) -> bool;
    /// No more audio will come.
    fn finish(&self);
}

/// A source of rendered speech. Lives on the render thread with its own instances.
pub trait RenderBackend: Send {
    fn name(&self) -> &'static str;
    fn render(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        ssml: bool,
    ) -> Result<Pcm, BackendError>;

    /// Pay the first-use cost for a voice now (instance creation, first synthesis) so the first
    /// real utterance does not. Called on the render thread after a probe. Default: nothing.
    fn warm(&mut self, _selector: u64) {}

    /// Render into `sink` as audio is produced. Backends that can only render whole hand the
    /// finished buffer over in one write; the caller still calls `sink.finish()`.
    fn render_streaming(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        ssml: bool,
        sink: std::sync::Arc<dyn StreamSink>,
    ) -> Result<(), BackendError> {
        let pcm = self.render(selector, text, params, ssml)?;
        sink.write(&pcm.samples, pcm.channels, pcm.rate);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm_duration_mono_and_trim() {
        let mut p = Pcm {
            samples: vec![0.0, 0.0, 0.5, 0.5, 0.5, 0.0, 0.0, 0.0],
            channels: 1,
            rate: 8,
        };
        assert_eq!(p.duration(), Duration::from_secs(1));
        assert_eq!(p.mono_at(4).len(), 4);
        p.trim_silence(0.01);
        assert_eq!(p.samples, vec![0.5, 0.5, 0.5]);
        let stereo = Pcm {
            samples: vec![1.0, 0.0, 0.0, 1.0],
            channels: 2,
            rate: 10,
        };
        assert_eq!(stereo.mono_at(10), vec![0.5, 0.5]);
    }
}
