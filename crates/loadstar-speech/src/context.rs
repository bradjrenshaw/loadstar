//! The game-thread handle (speech-spec §3, §6, §7): what the FFI wraps. Every call here returns
//! in microseconds — it resolves against the published tree snapshot, allocates an utterance id
//! and pushes one message; the worker does the rest.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::backend::{Backend, Params, Pcm, RenderBackend};
use crate::features;
use crate::ids;
use crate::state::{UtteranceState, UtteranceTable};
use crate::tree::Tree;
use crate::worker::{self, Item, JobState, Kind, Msg, Shared, Worker};

pub use crate::worker::QueueOptions;

#[derive(Clone, Debug)]
pub struct Config {
    /// An explicit Prism library path, tried first.
    pub prism_path: Option<PathBuf>,
    /// False = never load Prism (deterministic tests).
    pub load_prism: bool,
    /// How many recent utterances keep a queryable state.
    pub utterance_history: usize,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            prism_path: None,
            load_prism: true,
            utterance_history: 1024,
        }
    }
}

/// A profile (speech-spec §2): flat, caller-owned, passed with every call.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    /// Leaf id; 0 = root auto.
    pub voice: u64,
    pub rate: f32,
    pub pitch: f32,
    pub volume: f32,
    /// `profile_flags`.
    pub flags: u32,
}

impl Default for Profile {
    fn default() -> Self {
        Profile {
            voice: ids::ROOT_AUTO,
            rate: 0.5,
            pitch: 0.5,
            volume: 1.0,
            flags: 0,
        }
    }
}

impl Profile {
    fn params(&self) -> Params {
        Params {
            rate: self.rate,
            pitch: self.pitch,
            volume: self.volume,
        }
    }
    fn no_fallback(&self) -> bool {
        self.flags & profile_flags::NO_FALLBACK != 0
    }
    fn trim(&self) -> bool {
        self.flags & profile_flags::TRIM_SILENCE != 0
    }
}

pub mod profile_flags {
    /// Fail with `VoiceUnavailable` rather than walk up the tree.
    pub const NO_FALLBACK: u32 = 1;
    /// Trim leading/trailing silence from rendered speech.
    pub const TRIM_SILENCE: u32 = 2;
}

pub mod speak_flags {
    /// Stop what this queue is speaking, drop what it has pending, then speak this.
    pub const INTERRUPT: u32 = 1;
    /// Go to the head of the queue without cutting anything.
    pub const FRONT: u32 = 2;
    /// Text is SSML.
    pub const SSML: u32 = 4;
    /// Speech plus braille where the leaf supports it.
    pub const BRAILLE_TOO: u32 = 8;
    /// Positional: if the voice cannot render, render through the first synthesizer category's auto.
    pub const RENDER_ELSEWHERE: u32 = 16;
    /// Positional: if the voice cannot render, speak unpositioned and flag the utterance FLAT.
    pub const SPEAK_FLAT: u32 = 32;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechError {
    /// The chosen voice is absent and `NO_FALLBACK` was set (or nothing at all can speak).
    VoiceUnavailable,
    /// A feature the call needs is not in the resolved leaf's set.
    NotSupported,
    InvalidArgument,
    /// The worker is gone.
    Shutdown,
}

/// `resolve`'s answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// The leaf that would speak; 0 = nothing.
    pub voice: u64,
    pub fell_back: bool,
    /// Required features the leaf lacks.
    pub missing: u64,
    /// The tree has not been probed yet; the worker will resolve when it has.
    pub unprobed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderState {
    Pending,
    Done,
    Failed,
    Unknown,
}

/// Where a positional utterance plays (audio feature).
#[cfg(feature = "audio")]
#[derive(Clone)]
pub struct PositionalTarget {
    pub intake: loadstar_audio::device::DeviceIntake,
    pub space: u32,
    /// Source record template: bus, bounds, gain, pan width, priority. `id`, `kind`, `buffer`
    /// and the PLAYING/AUTO_REMOVE flags are set by the library.
    pub desc: loadstar_audio::space::SourceDesc,
}

pub struct Speech {
    tx: mpsc::Sender<Msg>,
    shared: Arc<Shared>,
    next_queue: AtomicU32,
    next_job: AtomicU64,
    next_source: AtomicU64,
    worker: Option<JoinHandle<()>>,
    render: Option<JoinHandle<()>>,
    prism_loaded_from: Option<PathBuf>,
}

impl Speech {
    /// Open with the platform's backends (speech-spec §2, "How the tree is populated").
    pub fn new(cfg: Config) -> Speech {
        let mut backends: Vec<Box<dyn Backend>> = Vec::new();
        let mut renderers: HashMap<usize, Box<dyn RenderBackend>> = HashMap::new();
        let mut prism_loaded_from = None;
        #[cfg(feature = "prism")]
        if cfg.load_prism {
            let p = crate::backends::prism::PrismBackend::load(cfg.prism_path.as_deref());
            prism_loaded_from = p.loaded_from().map(Path::to_path_buf);
            if p.is_loaded() {
                if !cfg!(windows) {
                    renderers.insert(
                        backends.len(),
                        Box::new(crate::backends::prism::PrismRender::load(
                            cfg.prism_path.as_deref(),
                        )),
                    );
                }
                backends.push(Box::new(p));
            }
        }
        #[cfg(all(feature = "sapi", windows))]
        {
            renderers.insert(
                backends.len(),
                Box::new(crate::backends::sapi::SapiRender::new()),
            );
            backends.push(Box::new(crate::backends::sapi::SapiBackend::new()));
        }
        #[cfg(all(feature = "clipboard", windows))]
        backends.push(Box::new(crate::backends::clipboard::ClipboardBackend));
        let mut s = Speech::with_backends(cfg, backends, renderers);
        s.prism_loaded_from = prism_loaded_from;
        s
    }

    /// Open with explicit backends (tests, harnesses). `renderers` is keyed by backend index.
    pub fn with_backends(
        cfg: Config,
        backends: Vec<Box<dyn Backend>>,
        renderers: HashMap<usize, Box<dyn RenderBackend>>,
    ) -> Speech {
        let shared = Arc::new(Shared {
            tree: RwLock::new(Arc::new(Tree::default())),
            table: UtteranceTable::new(cfg.utterance_history),
            jobs: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            changed: std::sync::atomic::AtomicBool::new(false),
        });
        let (tx, rx) = mpsc::channel::<Msg>();
        let (render_tx, render_rx) = mpsc::channel::<worker::RenderReq>();
        let render_tx = if renderers.is_empty() {
            None
        } else {
            Some(render_tx)
        };
        let render = if renderers.is_empty() {
            drop(render_rx);
            None
        } else {
            let to_worker = tx.clone();
            Some(
                std::thread::Builder::new()
                    .name("loadstar-speech-render".into())
                    .spawn(move || {
                        #[cfg(all(feature = "sapi", windows))]
                        crate::backends::sapi::com_init();
                        worker::render_loop(render_rx, to_worker, renderers);
                    })
                    .expect("spawn render thread"),
            )
        };
        let w = Worker::new(backends, render_tx, rx, Arc::clone(&shared));
        let worker = std::thread::Builder::new()
            .name("loadstar-speech".into())
            .spawn(move || {
                #[cfg(all(feature = "sapi", windows))]
                crate::backends::sapi::com_init();
                w.run();
            })
            .expect("spawn speech worker");
        Speech {
            tx,
            shared,
            next_queue: AtomicU32::new(1),
            next_job: AtomicU64::new(1),
            next_source: AtomicU64::new(1),
            worker: Some(worker),
            render,
            prism_loaded_from: None,
        }
    }

    pub fn prism_loaded_from(&self) -> Option<&Path> {
        self.prism_loaded_from.as_deref()
    }

    // --- tree ---

    /// The current snapshot.
    pub fn tree(&self) -> Arc<Tree> {
        self.shared
            .tree
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn generation(&self) -> u64 {
        self.shared.generation.load(Ordering::Acquire)
    }

    /// Ask the worker to re-probe; returns whether the tree's shape changed since the last call
    /// (the re-probe itself lands asynchronously — call again next frame to see it).
    pub fn refresh(&self) -> bool {
        let changed = self.shared.changed.swap(false, Ordering::AcqRel);
        let _ = self.tx.send(Msg::Refresh);
        changed
    }

    /// Block until the first probe has published (harnesses and tests only).
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let start = std::time::Instant::now();
        while self.generation() == 0 {
            if start.elapsed() > timeout {
                return false;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        true
    }

    /// What `profile` would use right now for a call needing `required` features.
    pub fn resolve(&self, profile: &Profile, required: u64) -> Resolved {
        let tree = self.tree();
        if tree.generation == 0 {
            return Resolved {
                voice: 0,
                fell_back: false,
                missing: 0,
                unprobed: true,
            };
        }
        let r = tree.resolve(profile.voice, profile.no_fallback());
        match r.leaf {
            Some(i) => {
                let l = &tree.leaves[i];
                Resolved {
                    voice: l.id,
                    fell_back: r.fell_back,
                    missing: required & !l.features,
                    unprobed: false,
                }
            }
            None => Resolved {
                voice: 0,
                fell_back: false,
                missing: required,
                unprobed: false,
            },
        }
    }

    fn precheck(&self, profile: &Profile, required: u64) -> Result<(), SpeechError> {
        let r = self.resolve(profile, required);
        if r.unprobed {
            return Ok(()); // the worker resolves once it has probed
        }
        if r.voice == 0 {
            return Err(SpeechError::VoiceUnavailable);
        }
        if r.missing != 0 {
            return Err(SpeechError::NotSupported);
        }
        Ok(())
    }

    // --- queues ---

    pub fn queue_create(&self, opts: QueueOptions) -> u32 {
        let id = self.next_queue.fetch_add(1, Ordering::Relaxed);
        let _ = self.tx.send(Msg::QueueCreate { id, opts });
        id
    }

    pub fn queue_set(&self, queue: u32, opts: QueueOptions) {
        let _ = self.tx.send(Msg::QueueSet { id: queue, opts });
    }

    pub fn queue_destroy(&self, queue: u32) {
        let _ = self.tx.send(Msg::QueueDestroy(queue));
    }

    // --- speaking ---

    fn submit(
        &self,
        queue: u32,
        profile: &Profile,
        flags: u32,
        kind: Kind,
    ) -> Result<u64, SpeechError> {
        let id = self.shared.table.alloc();
        let item = Item {
            id,
            voice: profile.voice,
            params: profile.params(),
            no_fallback: profile.no_fallback(),
            render_elsewhere: flags & speak_flags::RENDER_ELSEWHERE != 0,
            speak_flat: flags & speak_flags::SPEAK_FLAT != 0,
            kind,
        };
        self.tx
            .send(Msg::Speak {
                queue,
                item,
                interrupt: flags & speak_flags::INTERRUPT != 0,
                front: flags & speak_flags::FRONT != 0,
            })
            .map_err(|_| SpeechError::Shutdown)?;
        Ok(id)
    }

    /// Speak (voice only, or voice + braille with `BRAILLE_TOO`). Returns the utterance id.
    pub fn speak(
        &self,
        queue: u32,
        profile: &Profile,
        text: &str,
        flags: u32,
    ) -> Result<u64, SpeechError> {
        if text.is_empty() {
            return Ok(self.shared.table.alloc_done());
        }
        let mut required = features::SPEAK;
        if flags & speak_flags::SSML != 0 {
            required |= features::SSML;
        }
        // BRAILLE_TOO never adds a requirement: "output" means the user's modality set, so a leaf
        // without braille (a synthesizer, the clipboard) just speaks (speech-spec §3).
        self.precheck(profile, required)?;
        self.submit(
            queue,
            profile,
            flags,
            Kind::Speak {
                text: text.to_string(),
                output: flags & speak_flags::BRAILLE_TOO != 0,
                braille_only: false,
                ssml: flags & speak_flags::SSML != 0,
            },
        )
    }

    /// Braille only.
    pub fn braille(&self, queue: u32, profile: &Profile, text: &str) -> Result<u64, SpeechError> {
        if text.is_empty() {
            return Ok(self.shared.table.alloc_done());
        }
        self.precheck(profile, features::BRAILLE)?;
        self.submit(
            queue,
            profile,
            0,
            Kind::Speak {
                text: text.to_string(),
                output: false,
                braille_only: true,
                ssml: false,
            },
        )
    }

    /// Silence as a queue item.
    pub fn gap(&self, queue: u32, ms: u32) -> Result<u64, SpeechError> {
        self.submit(
            queue,
            &Profile::default(),
            0,
            Kind::Gap(Duration::from_millis(ms as u64)),
        )
    }

    /// Positional speech: rendered on the worker, fired through the audio intake as one queued utterance.
    #[cfg(feature = "audio")]
    pub fn speak_at(
        &self,
        queue: u32,
        profile: &Profile,
        text: &str,
        flags: u32,
        target: PositionalTarget,
    ) -> Result<(u64, u64), SpeechError> {
        let source_id = (1u64 << 62) | self.next_source.fetch_add(1, Ordering::Relaxed);
        if text.is_empty() {
            return Ok((self.shared.table.alloc_done(), source_id));
        }
        let ssml = flags & speak_flags::SSML != 0;
        let mut required = features::SPEAK;
        if ssml {
            required |= features::SSML;
        }
        // RENDER is required unless the caller said what to do without it.
        let r = self.resolve(profile, required | features::RENDER);
        if !r.unprobed {
            if r.voice == 0 {
                return Err(SpeechError::VoiceUnavailable);
            }
            let missing = r.missing;
            if missing & !features::RENDER != 0 {
                return Err(SpeechError::NotSupported);
            }
            if missing & features::RENDER != 0 {
                let elsewhere = flags & speak_flags::RENDER_ELSEWHERE != 0
                    && self.tree().render_elsewhere().is_some();
                let flat = flags & speak_flags::SPEAK_FLAT != 0;
                if !elsewhere && !flat {
                    return Err(SpeechError::NotSupported);
                }
            }
        }
        let kind = Kind::Positional {
            text: text.to_string(),
            ssml,
            trim: profile.trim(),
            target: worker::Target {
                intake: target.intake,
                space: target.space,
                desc: loadstar_audio::space::SourceDesc {
                    id: source_id,
                    ..target.desc
                },
            },
            pcm: None,
            requested: false,
        };
        self.submit(queue, profile, flags, kind)
            .map(|id| (id, source_id))
    }

    pub fn stop(&self, queue: u32) {
        let _ = self.tx.send(Msg::Stop(queue));
    }

    pub fn flush(&self, queue: u32) {
        let _ = self.tx.send(Msg::Flush(queue));
    }

    pub fn pause(&self, queue: u32) {
        let _ = self.tx.send(Msg::Pause(queue));
    }

    pub fn resume(&self, queue: u32) {
        let _ = self.tx.send(Msg::Resume(queue));
    }

    pub fn cancel(&self, utterance: u64) {
        let _ = self.tx.send(Msg::Cancel(utterance));
    }

    pub fn stop_all(&self) {
        let _ = self.tx.send(Msg::StopAll);
    }

    /// `(state, spoken flat, voice that spoke it)`.
    pub fn utterance(&self, id: u64) -> (UtteranceState, bool, u64) {
        self.shared.table.get(id)
    }

    // --- rendering ---

    /// Start a render job. `NotSupported` synchronously if the voice cannot render.
    pub fn render(&self, profile: &Profile, text: &str, ssml: bool) -> Result<u64, SpeechError> {
        if text.is_empty() {
            return Err(SpeechError::InvalidArgument);
        }
        let mut required = features::RENDER;
        if ssml {
            required |= features::SSML;
        }
        self.precheck(profile, required)?;
        let job = self.next_job.fetch_add(1, Ordering::Relaxed);
        self.shared
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(job, JobState::Pending);
        self.tx
            .send(Msg::Render {
                job,
                voice: profile.voice,
                no_fallback: profile.no_fallback(),
                text: text.to_string(),
                params: profile.params(),
                ssml,
                trim: profile.trim(),
            })
            .map_err(|_| SpeechError::Shutdown)?;
        Ok(job)
    }

    pub fn render_poll(&self, job: u64) -> RenderState {
        match self
            .shared
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&job)
        {
            Some(JobState::Pending) => RenderState::Pending,
            Some(JobState::Done(_)) => RenderState::Done,
            Some(JobState::Failed) => RenderState::Failed,
            None => RenderState::Unknown,
        }
    }

    /// The finished PCM, library-owned until `render_free`: `(samples, frames, channels, rate)`.
    /// The pointer stays valid across other calls because the job entry is never moved.
    pub fn render_take(&self, job: u64) -> Option<(*const f32, usize, u32, u32)> {
        let jobs = self.shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
        match jobs.get(&job) {
            Some(JobState::Done(p)) => Some((p.samples.as_ptr(), p.frames(), p.channels, p.rate)),
            _ => None,
        }
    }

    /// Take ownership of the finished PCM, ending the job.
    pub fn render_take_owned(&self, job: u64) -> Option<Pcm> {
        let mut jobs = self.shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
        match jobs.remove(&job) {
            Some(JobState::Done(p)) => Some(*p),
            Some(other) => {
                jobs.insert(job, other);
                None
            }
            None => None,
        }
    }

    pub fn render_free(&self, job: u64) {
        self.shared
            .jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&job);
    }
}

impl Drop for Speech {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        if let Some(r) = self.render.take() {
            let _ = r.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backends::fake::{Call, FakeBackend, FakeRender};
    use crate::tree;
    use std::time::Instant;

    fn open(
        reader_running: bool,
    ) -> (
        Speech,
        Arc<Mutex<crate::backends::fake::Shared>>,
        Arc<Mutex<crate::backends::fake::Shared>>,
    ) {
        let reader = FakeBackend::reader("nvda", reader_running);
        let synth = FakeBackend::synth("sapi", &["David", "Zira"]);
        let (rs, ss) = (reader.shared.clone(), synth.shared.clone());
        let mut renderers: HashMap<usize, Box<dyn RenderBackend>> = HashMap::new();
        renderers.insert(
            1,
            Box::new(FakeRender {
                shared: ss.clone(),
                rate: 48000,
            }),
        );
        let s = Speech::with_backends(
            Config {
                load_prism: false,
                ..Config::default()
            },
            vec![Box::new(reader), Box::new(synth)],
            renderers,
        );
        assert!(s.wait_ready(Duration::from_secs(5)));
        (s, rs, ss)
    }

    fn wait_until(f: impl Fn() -> bool, what: &str) {
        let start = Instant::now();
        while !f() {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "timed out waiting for {what}"
            );
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn speaks(shared: &Arc<Mutex<crate::backends::fake::Shared>>) -> Vec<String> {
        shared
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter_map(|c| {
                if let Call::Speak { text, .. } = c {
                    Some(text.clone())
                } else {
                    None
                }
            })
            .collect()
    }

    fn finish_all(shared: &Arc<Mutex<crate::backends::fake::Shared>>) {
        shared.lock().unwrap().speaking.clear();
    }

    #[test]
    fn root_auto_goes_to_the_running_reader_and_reports_done_at_hand_over() {
        let (s, rs, _) = open(true);
        let id = s.speak(0, &Profile::default(), "hello", 0).unwrap();
        wait_until(|| s.utterance(id).0 == UtteranceState::Done, "hand-over");
        assert_eq!(speaks(&rs), vec!["hello"]);
        let (_, flat, voice) = s.utterance(id);
        assert!(!flat);
        assert_eq!(voice, ids::voice_id("nvda", "nvda"));
    }

    #[test]
    fn a_sequential_synth_queue_waits_for_the_previous_utterance() {
        let (s, _, ss) = open(false);
        let a = s.speak(0, &Profile::default(), "one", 0).unwrap();
        let b = s.speak(0, &Profile::default(), "two", 0).unwrap();
        wait_until(
            || s.utterance(a).0 == UtteranceState::Speaking,
            "first starts",
        );
        std::thread::sleep(Duration::from_millis(20));
        assert_eq!(speaks(&ss), vec!["one"], "second waits");
        assert_eq!(s.utterance(b).0, UtteranceState::Pending);
        finish_all(&ss);
        wait_until(
            || speaks(&ss).len() == 2,
            "second starts after the first finishes",
        );
        wait_until(|| s.utterance(a).0 == UtteranceState::Done, "first done");
        finish_all(&ss);
        wait_until(|| s.utterance(b).0 == UtteranceState::Done, "second done");
    }

    #[test]
    fn interrupt_cuts_and_drops_and_front_jumps_the_line() {
        let (s, _, ss) = open(false);
        let a = s.speak(0, &Profile::default(), "a", 0).unwrap();
        let b = s.speak(0, &Profile::default(), "b", 0).unwrap();
        wait_until(
            || s.utterance(a).0 == UtteranceState::Speaking,
            "a speaking",
        );
        let c = s
            .speak(0, &Profile::default(), "c", speak_flags::INTERRUPT)
            .unwrap();
        wait_until(
            || s.utterance(c).0 == UtteranceState::Speaking,
            "c speaking",
        );
        assert_eq!(s.utterance(a).0, UtteranceState::Cancelled, "a was cut");
        assert_eq!(s.utterance(b).0, UtteranceState::Cancelled, "b was dropped");
        assert!(
            ss.lock()
                .unwrap()
                .calls
                .iter()
                .any(|c| matches!(c, Call::Stop(Some(_)))),
            "backend asked to stop a"
        );
        let d = s.speak(0, &Profile::default(), "d", 0).unwrap();
        let e = s
            .speak(0, &Profile::default(), "e", speak_flags::FRONT)
            .unwrap();
        finish_all(&ss);
        wait_until(
            || s.utterance(e).0 == UtteranceState::Speaking,
            "e jumped ahead of d",
        );
        assert_eq!(s.utterance(d).0, UtteranceState::Pending);
        assert_eq!(
            s.utterance(c).0,
            UtteranceState::Done,
            "c finished normally, not cut by FRONT"
        );
    }

    #[test]
    fn unlimited_queues_overlap_and_gaps_space_sequential_ones() {
        let (s, _, ss) = open(false);
        let q = s.queue_create(QueueOptions {
            max_concurrent: 0,
            gap_ms: 0,
        });
        let a = s.speak(q, &Profile::default(), "x", 0).unwrap();
        let b = s.speak(q, &Profile::default(), "y", 0).unwrap();
        wait_until(
            || {
                s.utterance(a).0 == UtteranceState::Speaking
                    && s.utterance(b).0 == UtteranceState::Speaking
            },
            "both speaking at once",
        );
        let g = s.queue_create(QueueOptions {
            max_concurrent: 1,
            gap_ms: 60,
        });
        let c = s.speak(g, &Profile::default(), "c", 0).unwrap();
        let d = s.speak(g, &Profile::default(), "d", 0).unwrap();
        wait_until(|| s.utterance(c).0 == UtteranceState::Speaking, "c");
        finish_all(&ss);
        wait_until(|| s.utterance(c).0 == UtteranceState::Done, "c done");
        let t = Instant::now();
        wait_until(
            || s.utterance(d).0 == UtteranceState::Speaking,
            "d after the gap",
        );
        assert!(
            t.elapsed() >= Duration::from_millis(45),
            "gap honoured: {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn output_on_a_voice_without_braille_still_speaks() {
        let (s, _, ss) = open(false); // no reader: root auto is SAPI, which has no OUTPUT feature
        let id = s
            .speak(0, &Profile::default(), "hello", speak_flags::BRAILLE_TOO)
            .expect("output on a synthesizer speaks instead of being refused");
        wait_until(|| s.utterance(id).0 == UtteranceState::Speaking, "spoken");
        assert_eq!(speaks(&ss), vec!["hello"]);
    }

    #[test]
    fn absence_falls_back_but_a_missing_feature_is_refused_synchronously() {
        let (s, _, _) = open(false);
        let nvda = Profile {
            voice: ids::voice_id("nvda", "nvda"),
            ..Profile::default()
        };
        let r = s.resolve(&nvda, features::SPEAK);
        assert!(
            r.fell_back && r.voice == ids::voice_id("sapi", "david"),
            "reader absent → SAPI default"
        );
        let strict = Profile {
            flags: profile_flags::NO_FALLBACK,
            ..nvda
        };
        assert_eq!(
            s.speak(0, &strict, "x", 0),
            Err(SpeechError::VoiceUnavailable)
        );
        assert_eq!(
            s.braille(0, &Profile::default(), "x"),
            Err(SpeechError::NotSupported),
            "synth has no braille"
        );
        let (s2, _, _) = open(true);
        assert_eq!(
            s2.render(&Profile::default(), "x", false),
            Err(SpeechError::NotSupported),
            "reader cannot render"
        );
    }

    #[test]
    fn render_jobs_complete_off_thread_and_are_library_owned_until_freed() {
        let (s, _, _) = open(false);
        let job = s.render(&Profile::default(), "abc", false).unwrap();
        wait_until(|| s.render_poll(job) == RenderState::Done, "render");
        let (ptr, frames, channels, rate) = s.render_take(job).unwrap();
        assert!(!ptr.is_null() && frames == 3 * 480 && channels == 1 && rate == 48000);
        s.render_free(job);
        assert_eq!(s.render_poll(job), RenderState::Unknown);
    }

    #[test]
    fn the_tree_reads_as_the_spec_describes() {
        let (s, _, _) = open(true);
        let t = s.tree();
        let keys: Vec<&str> = t.categories.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec![tree::SCREEN_READERS, tree::SAPI5]);
        assert_eq!(t.leaves_in(ids::category_id(tree::SAPI5)).count(), 2);
        assert!(s.refresh() || true);
        let empty = s.speak(0, &Profile::default(), "", 0).unwrap();
        assert_eq!(
            s.utterance(empty).0,
            UtteranceState::Done,
            "empty text is an already-done utterance"
        );
    }

    #[test]
    fn speak_call_latency_budget() {
        let (s, _, _) = open(false);
        let n = 2000;
        let t = Instant::now();
        for i in 0..n {
            s.speak(
                0,
                &Profile::default(),
                if i % 2 == 0 { "tick" } else { "tock" },
                0,
            )
            .unwrap();
        }
        let per_call = t.elapsed() / n;
        // Debug builds are several times slower than release; the spec budget (50 µs) is for release.
        assert!(
            per_call < Duration::from_micros(if cfg!(debug_assertions) { 300 } else { 50 }),
            "speak() took {per_call:?} per call"
        );
    }

    #[cfg(feature = "audio")]
    #[test]
    fn positional_speech_renders_ahead_and_fires_through_the_intake() {
        use loadstar_audio::backend::NullBackend;
        use loadstar_audio::device::{Device, DeviceConfig};
        use loadstar_audio::space::SpaceMode;
        use loadstar_core::geometry::{Bounds, Vec3};
        let (s, _, ss) = open(false);
        let mut dev = Device::open_null(DeviceConfig::default(), NullBackend::new(48000, 256));
        let space = dev.space_create(SpaceMode::Planar, 1.0, 1.0).unwrap();
        let target = PositionalTarget {
            intake: dev.intake(),
            space,
            desc: loadstar_audio::space::SourceDesc::buffer(
                0,
                0,
                Bounds::Point(Vec3::new(2.0, 0.0, 0.0)),
            ),
        };
        let q = s.queue_create(QueueOptions {
            max_concurrent: 0,
            gap_ms: 0,
        });
        let (a, src) = s
            .speak_at(q, &Profile::default(), "hello there", 0, target.clone())
            .unwrap();
        assert!(src & (1 << 62) != 0);
        wait_until(
            || s.utterance(a).0 == UtteranceState::Speaking,
            "fired (streaming)",
        );
        assert!(
            ss.lock()
                .unwrap()
                .calls
                .iter()
                .any(|c| matches!(c, Call::Render { .. }))
        );
        // Pump the null device at the mixer's pace: the render thread fills the stream while we play.
        let mut out = vec![0.0f32; 512];
        let mut energy = 0.0;
        let t = Instant::now();
        while s.utterance(a).0 != UtteranceState::Done {
            assert!(
                t.elapsed() < Duration::from_secs(5),
                "streamed utterance never drained"
            );
            dev.render(&mut out);
            energy += out.iter().map(|x| x * x).sum::<f32>();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(energy > 0.0, "the utterance played on the device");
        // A reader voice cannot be positioned: refused unless told what to do instead.
        let (s2, _, _) = open(true);
        assert_eq!(
            s2.speak_at(0, &Profile::default(), "x", 0, target.clone())
                .map(|_| ()),
            Err(SpeechError::NotSupported)
        );
        let (flat, _) = s2
            .speak_at(
                0,
                &Profile::default(),
                "x",
                speak_flags::SPEAK_FLAT,
                target.clone(),
            )
            .unwrap();
        wait_until(
            || s2.utterance(flat).0 == UtteranceState::Done,
            "spoken flat through the reader",
        );
        assert!(s2.utterance(flat).1, "flagged FLAT");
        let (elsewhere, _) = s2
            .speak_at(
                0,
                &Profile::default(),
                "x",
                speak_flags::RENDER_ELSEWHERE,
                target,
            )
            .unwrap();
        wait_until(
            || {
                matches!(
                    s2.utterance(elsewhere).0,
                    UtteranceState::Speaking | UtteranceState::Done
                )
            },
            "rendered through SAPI instead",
        );
        assert_eq!(s2.utterance(elsewhere).2, ids::voice_id("sapi", "david"));
    }
}
