//! Streaming sources (audio-spec §3, speech-spec §6, roadmap "streaming positional speech"): a
//! voice that plays from a ring the producer fills while it plays, so speech is audible after
//! its first synthesized chunk instead of its last. One producer (the speech render thread, or a
//! mod through `la_stream_*`), one consumer (the voice on the audio thread). Mono at the mixer
//! rate — the producer resamples on its own thread ([`crate::dsp::resample::Streaming`]).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

/// Progress a producer and the worker can both read: frames written in, frames played out.
#[derive(Debug, Default)]
pub struct StreamProgress {
    pub written: AtomicU64,
    pub played: AtomicU64,
    pub done: AtomicBool,
    /// Times the voice ran out of samples while the producer was still going.
    pub underruns: AtomicU64,
}

impl StreamProgress {
    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Acquire)
    }
    pub fn played(&self) -> u64 {
        self.played.load(Ordering::Acquire)
    }
    pub fn is_done(&self) -> bool {
        self.done.load(Ordering::Acquire)
    }
    pub fn underruns(&self) -> u64 {
        self.underruns.load(Ordering::Acquire)
    }
    /// The producer finished and the consumer has played everything it wrote.
    pub fn drained(&self) -> bool {
        self.is_done() && self.played() >= self.written()
    }
}

/// The audio-thread end: pulled by the voice each period. Allocation-free.
pub struct StreamSource {
    consumer: rtrb::Consumer<f32>,
    progress: Arc<StreamProgress>,
    /// Frames to accumulate before the first sample plays (absorbs producer jitter).
    prefill: usize,
    started: bool,
    /// The ring ran dry mid-stream: silent until `rebuffer` frames are in again.
    starved: bool,
    /// Frames to accumulate after an underrun before resuming. More than `prefill`: a producer
    /// that fell behind once (a host whose main thread stalled) is likely still catching up,
    /// and resuming on a trickle would underrun again at once — a stutter instead of one gap.
    rebuffer: usize,
}

/// Frames an underrun's two edges are ramped over (about 2 ms): running dry stops the waveform
/// wherever it is, and resuming starts it wherever it is, and both click unless faded.
const EDGE: usize = 96;

pub enum Pull {
    /// `n` frames written into the output (the rest, if any, is silence held for the producer).
    Produced(usize),
    /// Nothing yet: still prefilling (silence, keep waiting).
    Prefilling,
    /// Producer finished and the ring is empty: the sequence is over.
    Finished,
}

impl StreamSource {
    /// Fill `out` from the ring. Silence (zeros) where the producer has not caught up.
    pub fn pull(&mut self, out: &mut [f32]) -> Pull {
        let available = self.consumer.slots();
        let done = self.progress.done.load(Ordering::Acquire);
        if !self.started {
            if available < self.prefill && !done {
                return Pull::Prefilling;
            }
            self.started = true;
        }
        if available == 0 {
            return if done {
                Pull::Finished
            } else {
                Pull::Produced(0) // starving: hold silence, the producer is behind
            };
        }
        if self.starved && available < self.rebuffer && !done {
            return Pull::Produced(0); // rebuffering after an underrun
        }
        let n = available.min(out.len());
        if let Ok(chunk) = self.consumer.read_chunk(n) {
            let (a, b) = chunk.as_slices();
            out[..a.len()].copy_from_slice(a);
            out[a.len()..a.len() + b.len()].copy_from_slice(b);
            chunk.commit_all();
        }
        self.progress.played.fetch_add(n as u64, Ordering::AcqRel);
        if self.starved {
            // Resuming after an underrun: ramp in.
            self.starved = false;
            let edge = n.min(EDGE);
            for (i, s) in out[..edge].iter_mut().enumerate() {
                *s *= i as f32 / edge as f32;
            }
        }
        if n < out.len() && !done {
            // The ring is empty and more is coming: an underrun. Ramp what there was down to
            // silence instead of stopping dead.
            self.starved = true;
            self.progress.underruns.fetch_add(1, Ordering::AcqRel);
            let edge = n.min(EDGE);
            for (i, s) in out[n - edge..n].iter_mut().enumerate() {
                *s *= 1.0 - (i + 1) as f32 / edge as f32;
            }
        }
        Pull::Produced(n)
    }
}

/// The producer end. `Send`; one owner.
pub struct StreamWriter {
    producer: rtrb::Producer<f32>,
    progress: Arc<StreamProgress>,
    rate: u32,
}

impl StreamWriter {
    /// Mixer rate the samples must already be at.
    pub fn rate(&self) -> u32 {
        self.rate
    }

    pub fn progress(&self) -> &Arc<StreamProgress> {
        &self.progress
    }

    /// The voice is gone (stopped, cancelled, space destroyed): nothing more will be read.
    pub fn abandoned(&self) -> bool {
        self.producer.is_abandoned()
    }

    /// Write what fits now; returns how many samples were taken.
    pub fn write(&mut self, samples: &[f32]) -> usize {
        let n = samples.len().min(self.producer.slots());
        if n == 0 {
            return 0;
        }
        if let Ok(chunk) = self.producer.write_chunk_uninit(n) {
            chunk.fill_from_iter(samples[..n].iter().copied());
            self.progress.written.fetch_add(n as u64, Ordering::AcqRel);
            n
        } else {
            0
        }
    }

    /// Write everything, waiting (sleeping, not spinning) while the ring is full — the producer
    /// is a synthesis thread running far ahead of real time, so waiting only throttles it.
    /// `false` if the consumer went away first.
    pub fn write_all(&mut self, mut samples: &[f32]) -> bool {
        while !samples.is_empty() {
            if self.abandoned() {
                return false;
            }
            let n = self.write(samples);
            samples = &samples[n..];
            if !samples.is_empty() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        true
    }

    /// No more samples will come: the voice ends once it has played what is in the ring.
    pub fn finish(&self) {
        self.progress.done.store(true, Ordering::Release);
    }
}

/// Make a stream pair. `capacity` frames of ring (the producer blocks beyond it), `prefill`
/// frames before the first sample plays.
pub fn stream_pair(capacity: usize, prefill: usize, rate: u32) -> (StreamSource, StreamWriter) {
    let (producer, consumer) = rtrb::RingBuffer::new(capacity.max(64));
    let progress = Arc::new(StreamProgress::default());
    (
        StreamSource {
            consumer,
            progress: Arc::clone(&progress),
            prefill: prefill.min(capacity / 2),
            started: false,
            starved: false,
            rebuffer: (prefill * 5).min(capacity / 2),
        },
        StreamWriter {
            producer,
            progress,
            rate,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefills_then_streams_then_finishes() {
        let (mut src, mut w) = stream_pair(1024, 8, 48000);
        let mut out = [1.0f32; 4];
        assert!(matches!(src.pull(&mut out), Pull::Prefilling));
        assert_eq!(w.write(&[0.5; 6]), 6);
        assert!(
            matches!(src.pull(&mut out), Pull::Prefilling),
            "6 < prefill 8"
        );
        assert_eq!(w.write(&[0.5; 4]), 4);
        assert!(matches!(src.pull(&mut out), Pull::Produced(4)));
        assert_eq!(out, [0.5; 4]);
        assert!(matches!(src.pull(&mut out), Pull::Produced(4)));
        assert!(
            matches!(src.pull(&mut out), Pull::Produced(2)),
            "partial: the rest is silence"
        );
        assert!(
            matches!(src.pull(&mut out), Pull::Produced(0)),
            "starving, not finished"
        );
        w.finish();
        assert!(matches!(src.pull(&mut out), Pull::Finished));
        assert!(w.progress().drained());
        assert_eq!((w.progress().written(), w.progress().played()), (10, 10));
    }

    #[test]
    fn write_all_sees_an_abandoned_consumer() {
        let (src, mut w) = stream_pair(64, 1, 48000);
        assert!(w.write_all(&[0.0; 32]));
        drop(src);
        assert!(
            !w.write_all(&[0.0; 64]),
            "ring full and nobody reading: gives up"
        );
    }

    #[test]
    fn an_underrun_fades_out_rebuffers_and_fades_back_in() {
        // Prefill 100, so the rebuffer after an underrun is 500.
        let (mut src, mut w) = stream_pair(8192, 100, 48000);
        let mut out = [0.0f32; 256];
        assert_eq!(w.write(&[1.0; 356]), 356);
        assert!(matches!(src.pull(&mut out), Pull::Produced(256)));
        assert_eq!(out, [1.0; 256], "a full block passes untouched");
        // 100 left for a 256 block and the producer is not done: an underrun.
        assert!(matches!(src.pull(&mut out), Pull::Produced(100)));
        assert_eq!(out[0], 1.0);
        assert!(
            out[98] < 0.05 && out[99] == 0.0,
            "ramped to silence: {} {}",
            out[98],
            out[99]
        );
        assert!(
            out[..100].windows(2).all(|p| p[1] <= p[0]),
            "monotonic fade"
        );
        assert_eq!(w.progress().underruns(), 1);
        // A trickle is not enough to resume on.
        assert_eq!(w.write(&[1.0; 300]), 300);
        assert!(
            matches!(src.pull(&mut out), Pull::Produced(0)),
            "rebuffering"
        );
        assert_eq!(w.write(&[1.0; 300]), 300);
        assert!(matches!(src.pull(&mut out), Pull::Produced(256)));
        assert_eq!(out[0], 0.0, "ramps back in from silence");
        assert!(out[..96].windows(2).all(|p| p[1] >= p[0]));
        assert_eq!(out[96..], [1.0; 160]);
        assert_eq!(w.progress().underruns(), 1, "one gap, counted once");
        // The end of the stream is not an underrun: what is left plays as it is.
        w.finish();
        assert!(matches!(src.pull(&mut out), Pull::Produced(256)));
        assert!(matches!(src.pull(&mut out), Pull::Produced(88)));
        assert_eq!(out[..88], [1.0; 88], "no fade at a real end");
        assert!(matches!(src.pull(&mut out), Pull::Finished));
        assert_eq!(w.progress().underruns(), 1);
        // A producer that finishes while the voice is rebuffering still plays out.
        let (mut src, mut w) = stream_pair(8192, 100, 48000);
        w.write(&[1.0; 120]);
        assert!(matches!(src.pull(&mut out), Pull::Produced(120)));
        w.write(&[1.0; 10]);
        assert!(matches!(src.pull(&mut out), Pull::Produced(0)));
        w.finish();
        assert!(matches!(src.pull(&mut out), Pull::Produced(10)));
        assert!(matches!(src.pull(&mut out), Pull::Finished));
    }
}
