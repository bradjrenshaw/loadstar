//! Native AVSpeechSynthesizer (macOS) — category `avspeech` (speech-spec §5). Replaces Prism's
//! AVSpeech backend, which `dispatch_sync`s every call onto the main thread and so stalls the
//! worker whenever the host's main loop is busy (or deadlocks it when the host never runs one).
//!
//! Threading, measured on macOS (examples/avspeech_probe.rs): live speech runs entirely on the
//! calling thread — `speakUtterance` from the worker speaks with the main thread asleep. Render
//! does not: `writeUtterance:toBufferCallback:` delivers its buffers on the main queue, so a render
//! completes only while the host services it (a game's event loop does). The render thread waits
//! with a timeout, so a host that never does gets `Failed`, never a hang.

#![allow(unsafe_code)]

use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use objc2::rc::{Retained, autoreleasepool};
use objc2_avf_audio::{
    AVAudioBuffer, AVAudioPCMBuffer, AVSpeechBoundary, AVSpeechSynthesisVoice,
    AVSpeechSynthesizer, AVSpeechUtterance, AVSpeechUtteranceDefaultSpeechRate,
    AVSpeechUtteranceMaximumSpeechRate, AVSpeechUtteranceMinimumSpeechRate,
};
use objc2_foundation::NSString;

use crate::backend::{
    Backend, BackendError, LeafSpec, Params, Pcm, RenderBackend, Slot, StreamSink,
};
use crate::{features, ids, tree};

const LEAF_FEATURES: u64 = features::SPEAK
    | features::STOP
    | features::PAUSE
    | features::IS_SPEAKING
    | features::SET_RATE
    | features::SET_PITCH
    | features::SET_VOLUME
    | features::RENDER
    | features::MULTI_INSTANCE;

/// `isSpeaking` reads false for the first few milliseconds after `speakUtterance` returns (8 ms
/// measured), so an utterance is only finished once it has been seen speaking — or, if it never
/// is, after this long (empty or unspeakable text).
const START_GRACE: Duration = Duration::from_secs(2);

#[derive(Clone)]
struct VoiceInfo {
    selector: u64,
    identifier: String,
    name: String,
    language: String,
    is_default: bool,
}

fn enumerate() -> Vec<VoiceInfo> {
    autoreleasepool(|_| {
        // SAFETY: class methods with no preconditions; every object is retained by objc2.
        unsafe {
            let default = AVSpeechSynthesisVoice::voiceWithLanguage(Some(
                &AVSpeechSynthesisVoice::currentLanguageCode(),
            ))
            .map(|v| v.identifier().to_string());
            AVSpeechSynthesisVoice::speechVoices()
                .iter()
                .map(|v| {
                    let identifier = v.identifier().to_string();
                    VoiceInfo {
                        selector: ids::hash(&identifier),
                        is_default: default.as_deref() == Some(identifier.as_str()),
                        name: v.name().to_string(),
                        language: v.language().to_string(),
                        identifier,
                    }
                })
                .collect()
        }
    })
}

/// Normalized 0..1 (0.5 = default) onto AVSpeech's rate range, piecewise around its default.
fn map_rate(r: f32) -> f32 {
    // SAFETY: read-only framework constants.
    let (min, def, max) = unsafe {
        (
            AVSpeechUtteranceMinimumSpeechRate,
            AVSpeechUtteranceDefaultSpeechRate,
            AVSpeechUtteranceMaximumSpeechRate,
        )
    };
    let r = r.clamp(0.0, 1.0);
    if r <= 0.5 {
        min + (def - min) * (r / 0.5)
    } else {
        def + (max - def) * ((r - 0.5) / 0.5)
    }
}

/// Normalized 0..1 (0.5 = default) onto the 0.5..2.0 pitch multiplier, an octave each way.
fn map_pitch(p: f32) -> f32 {
    2f32.powf(p.clamp(0.0, 1.0) * 2.0 - 1.0)
}

fn utterance(
    voices: &[VoiceInfo],
    selector: u64,
    text: &str,
    params: &Params,
) -> Result<Retained<AVSpeechUtterance>, BackendError> {
    let info = voices
        .iter()
        .find(|v| v.selector == selector)
        .ok_or(BackendError::Unavailable)?;
    // SAFETY: plain property setters on an utterance we own.
    unsafe {
        let voice = AVSpeechSynthesisVoice::voiceWithIdentifier(&NSString::from_str(
            &info.identifier,
        ))
        .ok_or(BackendError::Unavailable)?;
        let u = AVSpeechUtterance::speechUtteranceWithString(&NSString::from_str(text));
        u.setVoice(Some(&voice));
        u.setRate(map_rate(params.rate));
        u.setPitchMultiplier(map_pitch(params.pitch));
        u.setVolume(params.volume.clamp(0.0, 1.0));
        Ok(u)
    }
}

struct Instance {
    synth: Retained<AVSpeechSynthesizer>,
    selector: u64,
    busy: bool,
    seen_speaking: bool,
    started: Instant,
}

/// Live AVSpeech (worker thread). One synthesizer per concurrent utterance, reused per voice.
pub struct AvSpeechBackend {
    voices: Vec<VoiceInfo>,
    instances: Vec<Instance>,
}

// The synthesizers are created and used only on the worker thread that owns this struct; it is
// constructed empty on the game thread and moved there before any exist.
unsafe impl Send for AvSpeechBackend {}

impl AvSpeechBackend {
    pub fn new() -> AvSpeechBackend {
        AvSpeechBackend {
            voices: Vec::new(),
            instances: Vec::new(),
        }
    }

    fn free_instance(&mut self, selector: u64) -> usize {
        if let Some(i) = self
            .instances
            .iter()
            .position(|i| i.selector == selector && !i.busy)
        {
            return i;
        }
        // SAFETY: a fresh synthesizer; no preconditions.
        let synth = unsafe { AVSpeechSynthesizer::new() };
        self.instances.push(Instance {
            synth,
            selector,
            busy: false,
            seen_speaking: false,
            started: Instant::now(),
        });
        self.instances.len() - 1
    }
}

impl Default for AvSpeechBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for AvSpeechBackend {
    fn name(&self) -> &'static str {
        "avspeech"
    }

    fn probe(&mut self) -> Vec<LeafSpec> {
        self.voices = enumerate();
        self.voices
            .iter()
            .map(|v| LeafSpec {
                category: tree::AVSPEECH.into(),
                category_name: None,
                category_rank: None,
                name: v.name.clone(),
                language: v.language.clone(),
                native_id: v.identifier.clone(),
                dedupe: None,
                features: LEAF_FEATURES,
                selector: v.selector,
                is_default: v.is_default,
                available: true,
            })
            .collect()
    }

    fn speak(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        _interrupt: bool,
        _output: bool,
        _ssml: bool,
    ) -> Result<Slot, BackendError> {
        autoreleasepool(|_| {
            let u = utterance(&self.voices, selector, text, params)?;
            let idx = self.free_instance(selector);
            let inst = &mut self.instances[idx];
            // SAFETY: an idle synthesizer owned by this thread and a fully configured utterance.
            unsafe { inst.synth.speakUtterance(&u) };
            inst.busy = true;
            inst.seen_speaking = false;
            inst.started = Instant::now();
            Ok(idx as Slot)
        })
    }

    fn braille(&mut self, _selector: u64, _text: &str) -> Result<(), BackendError> {
        Err(BackendError::NotSupported)
    }

    fn stop(&mut self, slot: Option<Slot>) {
        let range: Vec<usize> = match slot {
            Some(s) => vec![s as usize],
            None => (0..self.instances.len()).collect(),
        };
        for i in range {
            if let Some(inst) = self.instances.get_mut(i) {
                // SAFETY: a synthesizer owned by this thread.
                unsafe { inst.synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
                inst.busy = false;
            }
        }
    }

    fn pause(&mut self, slot: Slot) -> Result<(), BackendError> {
        let inst = self
            .instances
            .get(slot as usize)
            .ok_or(BackendError::Unavailable)?;
        // SAFETY: a synthesizer owned by this thread.
        if unsafe { inst.synth.pauseSpeakingAtBoundary(AVSpeechBoundary::Immediate) } {
            Ok(())
        } else {
            Err(BackendError::Failed("pauseSpeakingAtBoundary".into()))
        }
    }

    fn resume(&mut self, slot: Slot) -> Result<(), BackendError> {
        let inst = self
            .instances
            .get(slot as usize)
            .ok_or(BackendError::Unavailable)?;
        // SAFETY: a synthesizer owned by this thread.
        if unsafe { inst.synth.continueSpeaking() } {
            Ok(())
        } else {
            Err(BackendError::Failed("continueSpeaking".into()))
        }
    }

    fn speaking(&mut self, slot: Slot) -> Option<bool> {
        let inst = self.instances.get_mut(slot as usize)?;
        // SAFETY: a synthesizer owned by this thread. isSpeaking stays true while paused.
        if unsafe { inst.synth.isSpeaking() } {
            inst.seen_speaking = true;
            return Some(true);
        }
        Some(!inst.seen_speaking && inst.started.elapsed() < START_GRACE)
    }

    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(10))
    }

    fn release(&mut self, slot: Slot) {
        if let Some(inst) = self.instances.get_mut(slot as usize) {
            inst.busy = false;
        }
    }
}

/// What the buffer callback has delivered so far, shared with the waiting render thread.
#[derive(Default)]
struct Collected {
    pcm: Pcm,
    done: bool,
    /// The sink said stop.
    aborted: bool,
}

/// Interleaved f32 out of one AVAudioPCMBuffer (float or int16, interleaved or not).
fn samples_of(buf: &AVAudioPCMBuffer) -> (Vec<f32>, u32, u32) {
    // SAFETY: the channel pointers are valid for frameLength frames for the callback's duration.
    unsafe {
        let fmt = buf.format();
        let frames = buf.frameLength() as usize;
        let channels = fmt.channelCount().max(1) as usize;
        let rate = fmt.sampleRate() as u32;
        let interleaved = fmt.isInterleaved();
        let mut out = vec![0f32; frames * channels];
        let f = buf.floatChannelData();
        let i = buf.int16ChannelData();
        let read = |c: usize, n: usize| -> f32 {
            let (plane, idx) = if interleaved { (0, n * channels + c) } else { (c, n) };
            if !f.is_null() {
                *(*f.add(plane)).as_ptr().add(idx)
            } else if !i.is_null() {
                *(*i.add(plane)).as_ptr().add(idx) as f32 / 32768.0
            } else {
                0.0
            }
        };
        for n in 0..frames {
            for c in 0..channels {
                out[n * channels + c] = read(c, n);
            }
        }
        (out, channels as u32, rate)
    }
}

/// AVSpeech speak-to-memory (render thread): its own synthesizers, never the live ones.
pub struct AvSpeechRender {
    voices: Vec<VoiceInfo>,
    synths: std::collections::HashMap<u64, Retained<AVSpeechSynthesizer>>,
    probed: bool,
}

unsafe impl Send for AvSpeechRender {}

impl AvSpeechRender {
    pub fn new() -> AvSpeechRender {
        AvSpeechRender {
            voices: Vec::new(),
            synths: std::collections::HashMap::new(),
            probed: false,
        }
    }

    /// Synthesize `text` to buffers; each chunk goes to `sink` (if any) and into the result.
    /// Waits at most a generous multiple of the text's spoken length for the main queue to
    /// deliver (see the module note).
    fn write(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        sink: Option<Arc<dyn StreamSink>>,
    ) -> Result<Pcm, BackendError> {
        if !self.probed {
            self.voices = enumerate();
            self.probed = true;
        }
        autoreleasepool(|_| {
            let u = utterance(&self.voices, selector, text, params)?;
            let synth = self
                .synths
                .entry(selector)
                // SAFETY: a fresh synthesizer; no preconditions.
                .or_insert_with(|| unsafe { AVSpeechSynthesizer::new() })
                .clone();
            let state = Arc::new((Mutex::new(Collected::default()), Condvar::new()));
            let cb_state = Arc::clone(&state);
            let block = RcBlock::new(move |buf: NonNull<AVAudioBuffer>| {
                // SAFETY: AVFoundation passes a live buffer for the duration of the call.
                let buf: &AVAudioBuffer = unsafe { buf.as_ref() };
                let (lock, cv) = &*cb_state;
                let mut c = lock.lock().unwrap_or_else(|e| e.into_inner());
                if c.done {
                    return;
                }
                let Some(pcm) = buf.downcast_ref::<AVAudioPCMBuffer>() else {
                    return;
                };
                // SAFETY: a live PCM buffer.
                if unsafe { pcm.frameLength() } == 0 {
                    c.done = true; // the zero-length buffer marks the end
                    cv.notify_all();
                    return;
                }
                let (samples, channels, rate) = samples_of(pcm);
                if let Some(s) = &sink {
                    if !s.write(&samples, channels, rate) {
                        c.aborted = true;
                        c.done = true;
                        cv.notify_all();
                        return;
                    }
                }
                c.pcm.channels = channels;
                c.pcm.rate = rate;
                c.pcm.samples.extend_from_slice(&samples);
            });
            // SAFETY: the synthesizer copies the block; `state` outlives every invocation by Arc.
            unsafe { synth.writeUtterance_toBufferCallback(&u, &*block as *const _ as *mut _) };
            let budget = Duration::from_secs(5) + Duration::from_millis(150) * text.len() as u32;
            let (lock, cv) = &*state;
            let guard = lock.lock().unwrap_or_else(|e| e.into_inner());
            let (mut c, timeout) = cv
                .wait_timeout_while(guard, budget, |c| !c.done)
                .unwrap_or_else(|e| e.into_inner());
            if timeout.timed_out() || c.aborted {
                c.done = true; // late buffers are ignored
                // SAFETY: a synthesizer owned by this thread.
                unsafe { synth.stopSpeakingAtBoundary(AVSpeechBoundary::Immediate) };
                if timeout.timed_out() {
                    return Err(BackendError::Failed(
                        "AVSpeech render timed out (the host never serviced the main queue)"
                            .into(),
                    ));
                }
            }
            Ok(std::mem::take(&mut c.pcm))
        })
    }
}

impl Default for AvSpeechRender {
    fn default() -> Self {
        Self::new()
    }
}

impl RenderBackend for AvSpeechRender {
    fn name(&self) -> &'static str {
        "avspeech"
    }

    fn render(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        _ssml: bool,
    ) -> Result<Pcm, BackendError> {
        self.write(selector, text, params, None)
    }

    fn render_streaming(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        _ssml: bool,
        sink: Arc<dyn StreamSink>,
    ) -> Result<(), BackendError> {
        self.write(selector, text, params, Some(sink)).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_and_pitch_map_around_the_default() {
        // SAFETY: framework constants.
        let def = unsafe { AVSpeechUtteranceDefaultSpeechRate };
        assert!((map_rate(0.5) - def).abs() < 1e-6);
        assert!(map_rate(0.0) < def && map_rate(1.0) > def);
        assert!((map_pitch(0.5) - 1.0).abs() < 1e-6);
        assert!((map_pitch(0.0) - 0.5).abs() < 1e-6);
        assert!((map_pitch(1.0) - 2.0).abs() < 1e-6);
    }
}
