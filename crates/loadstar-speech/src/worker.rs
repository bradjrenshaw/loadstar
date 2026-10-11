//! Queues and the worker thread (speech-spec §2, §3, §6, §8). The game thread only pushes
//! messages; this thread resolves, sequences, talks to backends, uploads rendered speech through
//! the audio intake and keeps the utterance table current. It never polls on a timer it does
//! not need: it parks on its inbox and wakes for a message, a gap expiry, an expected end, or a
//! backend whose state must be asked.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use crate::backend::{Backend, BackendError, Params, Pcm, RenderBackend, Slot};
use crate::features;
use crate::state::{UtteranceState, UtteranceTable};
use crate::tree::Tree;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QueueOptions {
    /// 1 = sequential; 0 = unlimited overlap (synthesizer leaves only).
    pub max_concurrent: u32,
    pub gap_ms: u32,
}

impl Default for QueueOptions {
    fn default() -> Self {
        QueueOptions {
            max_concurrent: 1,
            gap_ms: 0,
        }
    }
}

/// Where a positional utterance plays (audio feature).
#[cfg(feature = "audio")]
#[derive(Clone)]
pub struct Target {
    pub intake: loadstar_audio::device::DeviceIntake,
    pub space: u32,
    /// The mod's source record: bus, bounds, gain, pan width, priority. `id` is the allocated
    /// source id; the worker fills `buffer` and adds PLAYING | AUTO_REMOVE.
    pub desc: loadstar_audio::space::SourceDesc,
}

pub enum Kind {
    Speak {
        text: String,
        output: bool,
        braille_only: bool,
        ssml: bool,
    },
    Gap(Duration),
    #[cfg(feature = "audio")]
    Positional {
        text: String,
        ssml: bool,
        trim: bool,
        target: Target,
        pcm: Option<Pcm>,
        requested: bool,
    },
}

pub struct Item {
    pub id: u64,
    pub voice: u64,
    pub params: Params,
    pub no_fallback: bool,
    pub render_elsewhere: bool,
    pub speak_flat: bool,
    pub kind: Kind,
}

#[derive(Clone, Copy, Debug)]
pub enum RenderToken {
    Job(u64),
    Utterance(u64),
}

pub struct RenderReq {
    pub token: RenderToken,
    pub backend: usize,
    pub selector: u64,
    pub text: String,
    pub params: Params,
    pub ssml: bool,
    pub trim: bool,
}

pub enum Msg {
    Speak {
        queue: u32,
        item: Item,
        interrupt: bool,
        front: bool,
    },
    Stop(u32),
    Flush(u32),
    Pause(u32),
    Resume(u32),
    Cancel(u64),
    StopAll,
    QueueCreate {
        id: u32,
        opts: QueueOptions,
    },
    QueueSet {
        id: u32,
        opts: QueueOptions,
    },
    QueueDestroy(u32),
    Render {
        job: u64,
        voice: u64,
        no_fallback: bool,
        text: String,
        params: Params,
        ssml: bool,
        trim: bool,
    },
    Rendered {
        token: RenderToken,
        result: Result<Pcm, BackendError>,
    },
    Refresh,
    Shutdown,
}

pub enum JobState {
    Pending,
    Done(Box<Pcm>),
    Failed,
}

/// What the game thread and the worker share.
pub struct Shared {
    pub tree: RwLock<Arc<Tree>>,
    pub table: UtteranceTable,
    pub jobs: Mutex<HashMap<u64, JobState>>,
    pub generation: AtomicU64,
    /// The tree's shape changed since the mod last asked.
    pub changed: AtomicBool,
}

struct Active {
    id: u64,
    backend: Option<usize>,
    slot: Slot,
    expected_end: Option<Instant>,
    next_poll: Option<Instant>,
    #[cfg(feature = "audio")]
    positional: Option<(loadstar_audio::device::DeviceIntake, u32, u64)>,
}

struct Queue {
    opts: QueueOptions,
    pending: VecDeque<Item>,
    active: Vec<Active>,
    gap_until: Option<Instant>,
    paused: bool,
}

impl Queue {
    fn new(opts: QueueOptions) -> Queue {
        Queue {
            opts,
            pending: VecDeque::new(),
            active: Vec::new(),
            gap_until: None,
            paused: false,
        }
    }

    fn cap(&self) -> usize {
        if self.opts.max_concurrent == 0 {
            usize::MAX
        } else {
            self.opts.max_concurrent as usize
        }
    }
}

pub struct Worker {
    backends: Vec<Box<dyn Backend>>,
    names: Vec<&'static str>,
    render_tx: Option<mpsc::Sender<RenderReq>>,
    rx: mpsc::Receiver<Msg>,
    shared: Arc<Shared>,
    queues: HashMap<u32, Queue>,
    next_probe: Vec<Option<Instant>>,
    /// Utterance → queue, for Cancel and Rendered.
    owner: HashMap<u64, u32>,
}

impl Worker {
    pub fn new(
        backends: Vec<Box<dyn Backend>>,
        render_tx: Option<mpsc::Sender<RenderReq>>,
        rx: mpsc::Receiver<Msg>,
        shared: Arc<Shared>,
    ) -> Worker {
        let names = backends.iter().map(|b| b.name()).collect();
        let n = backends.len();
        let mut queues = HashMap::new();
        queues.insert(0, Queue::new(QueueOptions::default()));
        Worker {
            backends,
            names,
            render_tx,
            rx,
            shared,
            queues,
            next_probe: vec![None; n],
            owner: HashMap::new(),
        }
    }

    pub fn run(mut self) {
        self.probe();
        loop {
            // A deadline already in the past still goes through recv_timeout (with zero wait) so
            // messages are never starved by a busy service pass.
            let msg = match self.next_deadline() {
                Some(d) => match self
                    .rx
                    .recv_timeout(d.saturating_duration_since(Instant::now()))
                {
                    Ok(m) => Some(m),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return,
                },
                None => match self.rx.recv() {
                    Ok(m) => Some(m),
                    Err(_) => return,
                },
            };
            if let Some(m) = msg {
                if !self.handle(m) {
                    return;
                }
                while let Ok(m) = self.rx.try_recv() {
                    if !self.handle(m) {
                        return;
                    }
                }
            }
            self.service();
        }
    }

    fn tree(&self) -> Arc<Tree> {
        self.shared
            .tree
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    // --- probing ---

    fn probe(&mut self) {
        let mut specs = Vec::new();
        let now = Instant::now();
        for (i, b) in self.backends.iter_mut().enumerate() {
            for s in b.probe() {
                specs.push((i, self.names[i], s));
            }
            self.next_probe[i] = b.reprobe_every().map(|d| now + d);
        }
        self.publish(specs);
    }

    /// A backend's own cadence came due (readers start and stop after boot): re-probe everything.
    /// Probing is cheap off the game thread and keeps ids stable, since they hash native ids.
    fn probe_due(&mut self, now: Instant) {
        if self.next_probe.iter().any(|t| t.is_some_and(|t| t <= now)) {
            self.probe();
        }
    }

    fn publish(&mut self, specs: Vec<(usize, &'static str, crate::backend::LeafSpec)>) {
        let generation = self.shared.generation.load(Ordering::Relaxed) + 1;
        let tree = Tree::build(generation, specs);
        self.install(tree);
    }

    fn install(&mut self, tree: Tree) {
        let changed = !self.tree().same_shape(&tree);
        self.shared
            .generation
            .store(tree.generation, Ordering::Release);
        *self.shared.tree.write().unwrap_or_else(|e| e.into_inner()) = Arc::new(tree);
        if changed {
            self.shared.changed.store(true, Ordering::Release);
        }
    }

    // --- timing ---

    fn next_deadline(&self) -> Option<Instant> {
        let mut d: Option<Instant> = None;
        let mut consider = |t: Option<Instant>| {
            if let Some(t) = t {
                d = Some(d.map_or(t, |x| x.min(t)));
            }
        };
        for q in self.queues.values() {
            consider(q.gap_until);
            for a in &q.active {
                consider(a.expected_end);
                consider(a.next_poll);
            }
        }
        for t in &self.next_probe {
            consider(*t);
        }
        d
    }

    // --- messages ---

    fn handle(&mut self, msg: Msg) -> bool {
        match msg {
            Msg::Speak {
                queue,
                item,
                interrupt,
                front,
            } => {
                let q = self
                    .queues
                    .entry(queue)
                    .or_insert_with(|| Queue::new(QueueOptions::default()));
                if interrupt {
                    Self::cut_actives(q, &mut self.backends, &self.shared.table);
                    Self::drop_pending(q, &self.shared.table);
                    q.gap_until = None;
                }
                self.owner.insert(item.id, queue);
                if front {
                    q.pending.push_front(item)
                } else {
                    q.pending.push_back(item)
                }
            }
            Msg::Stop(queue) => {
                if let Some(q) = self.queues.get_mut(&queue) {
                    Self::cut_actives(q, &mut self.backends, &self.shared.table);
                    q.gap_until = None;
                }
            }
            Msg::Flush(queue) => {
                if let Some(q) = self.queues.get_mut(&queue) {
                    Self::drop_pending(q, &self.shared.table);
                }
            }
            Msg::Pause(queue) => {
                if let Some(q) = self.queues.get_mut(&queue) {
                    q.paused = true;
                    for a in &q.active {
                        if let Some(b) = a.backend {
                            let _ = self.backends[b].pause(a.slot);
                        }
                    }
                }
            }
            Msg::Resume(queue) => {
                if let Some(q) = self.queues.get_mut(&queue) {
                    q.paused = false;
                    for a in &q.active {
                        if let Some(b) = a.backend {
                            let _ = self.backends[b].resume(a.slot);
                        }
                    }
                }
            }
            Msg::Cancel(id) => {
                if let Some(queue) = self.owner.get(&id).copied() {
                    if let Some(q) = self.queues.get_mut(&queue) {
                        if let Some(pos) = q.pending.iter().position(|i| i.id == id) {
                            q.pending.remove(pos);
                            self.shared.table.set(id, UtteranceState::Cancelled, false);
                        } else if let Some(pos) = q.active.iter().position(|a| a.id == id) {
                            let a = q.active.remove(pos);
                            Self::cut_one(&a, &mut self.backends);
                            self.shared.table.set(id, UtteranceState::Cancelled, false);
                        }
                    }
                }
            }
            Msg::StopAll => {
                for q in self.queues.values_mut() {
                    Self::cut_actives(q, &mut self.backends, &self.shared.table);
                    Self::drop_pending(q, &self.shared.table);
                    q.gap_until = None;
                }
                for b in &mut self.backends {
                    b.stop(None);
                }
            }
            Msg::QueueCreate { id, opts } => {
                self.queues
                    .entry(id)
                    .or_insert_with(|| Queue::new(opts))
                    .opts = opts;
            }
            Msg::QueueSet { id, opts } => {
                if let Some(q) = self.queues.get_mut(&id) {
                    q.opts = opts;
                }
            }
            Msg::QueueDestroy(id) => {
                if id != 0 {
                    if let Some(mut q) = self.queues.remove(&id) {
                        Self::cut_actives(&mut q, &mut self.backends, &self.shared.table);
                        Self::drop_pending(&mut q, &self.shared.table);
                    }
                }
            }
            Msg::Render {
                job,
                voice,
                no_fallback,
                text,
                params,
                ssml,
                trim,
            } => {
                let tree = self.tree();
                let leaf = tree
                    .resolve(voice, no_fallback)
                    .leaf
                    .map(|i| &tree.leaves[i])
                    .filter(|l| l.features & features::RENDER != 0);
                match (leaf, &self.render_tx) {
                    (Some(l), Some(tx)) => {
                        let _ = tx.send(RenderReq {
                            token: RenderToken::Job(job),
                            backend: l.backend,
                            selector: l.selector,
                            text,
                            params,
                            ssml,
                            trim,
                        });
                    }
                    _ => {
                        self.shared
                            .jobs
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(job, JobState::Failed);
                    }
                }
            }
            Msg::Rendered { token, result } => match token {
                RenderToken::Job(job) => {
                    let mut jobs = self.shared.jobs.lock().unwrap_or_else(|e| e.into_inner());
                    if jobs.contains_key(&job) {
                        jobs.insert(
                            job,
                            match result {
                                Ok(p) => JobState::Done(Box::new(p)),
                                Err(_) => JobState::Failed,
                            },
                        );
                    }
                }
                RenderToken::Utterance(id) => self.rendered_for(id, result),
            },
            Msg::Refresh => self.probe(),
            Msg::Shutdown => return false,
        }
        true
    }

    #[cfg(feature = "audio")]
    fn rendered_for(&mut self, id: u64, result: Result<Pcm, BackendError>) {
        let Some(queue) = self.owner.get(&id).copied() else {
            return;
        };
        let Some(q) = self.queues.get_mut(&queue) else {
            return;
        };
        let Some(pos) = q.pending.iter().position(|i| i.id == id) else {
            return;
        };
        match result {
            Ok(p) => {
                if let Kind::Positional { pcm, .. } = &mut q.pending[pos].kind {
                    *pcm = Some(p);
                }
            }
            Err(_) => {
                q.pending.remove(pos);
                self.shared.table.set(id, UtteranceState::Cancelled, false);
            }
        }
    }

    #[cfg(not(feature = "audio"))]
    fn rendered_for(&mut self, _id: u64, _result: Result<Pcm, BackendError>) {}

    fn cut_one(a: &Active, backends: &mut [Box<dyn Backend>]) {
        if let Some(b) = a.backend {
            backends[b].stop(Some(a.slot));
            backends[b].release(a.slot);
        }
        #[cfg(feature = "audio")]
        if let Some((intake, space, source)) = &a.positional {
            let _ = intake.stop(*space, *source);
        }
    }

    fn cut_actives(q: &mut Queue, backends: &mut [Box<dyn Backend>], table: &UtteranceTable) {
        for a in q.active.drain(..) {
            Self::cut_one(&a, backends);
            table.set(a.id, UtteranceState::Cancelled, false);
        }
    }

    fn drop_pending(q: &mut Queue, table: &UtteranceTable) {
        for i in q.pending.drain(..) {
            table.set(i.id, UtteranceState::Cancelled, false);
        }
    }

    // --- the per-wake service pass ---

    fn service(&mut self) {
        let now = Instant::now();
        self.probe_due(now);
        let tree = self.tree();
        let mut ids: Vec<u32> = self.queues.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            self.retire_finished(id, now);
            self.start_ready(id, now, &tree);
            self.render_ahead(id, &tree);
        }
    }

    fn retire_finished(&mut self, queue: u32, now: Instant) {
        let Some(q) = self.queues.get_mut(&queue) else {
            return;
        };
        let mut i = 0;
        let mut finished_any = false;
        while i < q.active.len() {
            let a = &mut q.active[i];
            let mut done = a.expected_end.is_some_and(|e| e <= now);
            if !done {
                if let (Some(b), Some(poll)) = (a.backend, a.next_poll) {
                    if poll <= now {
                        match self.backends[b].speaking(a.slot) {
                            Some(true) => {
                                a.next_poll = Some(
                                    now + self.backends[b]
                                        .poll_interval()
                                        .unwrap_or(Duration::from_millis(20)),
                                )
                            }
                            Some(false) | None => done = true,
                        }
                    }
                }
            }
            if done {
                let a = q.active.remove(i);
                if let Some(b) = a.backend {
                    self.backends[b].release(a.slot);
                }
                self.shared.table.set(a.id, UtteranceState::Done, false);
                self.owner.remove(&a.id);
                finished_any = true;
            } else {
                i += 1;
            }
        }
        if finished_any && q.cap() == 1 && q.opts.gap_ms > 0 && !q.pending.is_empty() {
            q.gap_until = Some(now + Duration::from_millis(q.opts.gap_ms as u64));
        }
    }

    fn start_ready(&mut self, queue: u32, now: Instant, tree: &Tree) {
        loop {
            let Some(q) = self.queues.get(&queue) else {
                return;
            };
            if q.paused || q.pending.is_empty() || q.active.len() >= q.cap() {
                return;
            }
            if q.gap_until.is_some_and(|g| g > now) {
                return;
            }
            if q.gap_until.is_some() {
                self.queues.get_mut(&queue).unwrap().gap_until = None; // elapsed: never a stale deadline
            }
            let q = self.queues.get(&queue).unwrap();
            let sequential = q.cap() == 1;
            // Which pending item to try: the head on a sequential queue; the first startable one otherwise.
            let mut pick = None;
            for (pos, item) in q.pending.iter().enumerate() {
                let ready = match &item.kind {
                    #[cfg(feature = "audio")]
                    // Waiting for PCM only if a render is actually coming; otherwise start() decides
                    // (SPEAK_FLAT speaks through the reader, anything else is refused).
                    Kind::Positional { pcm, .. } => pcm.is_some() || !self.will_render(item, tree),
                    _ => true,
                };
                if ready && self.channel_free(q, item, tree) {
                    pick = Some(pos);
                    break;
                }
                if sequential {
                    break;
                }
            }
            let Some(pos) = pick else { return };
            let item = self
                .queues
                .get_mut(&queue)
                .unwrap()
                .pending
                .remove(pos)
                .unwrap();
            let started = self.start(queue, item, now, tree);
            if !started && sequential {
                // Cancelled at start (unavailable voice): fall through to the next head.
                continue;
            }
        }
    }

    /// Will a render be requested for this positional item (its leaf renders, or RENDER_ELSEWHERE
    /// has somewhere to go)? If not, the item must not wait for PCM that never comes.
    #[cfg(feature = "audio")]
    fn will_render(&self, item: &Item, tree: &Tree) -> bool {
        match tree.resolve(item.voice, item.no_fallback).leaf {
            Some(i) if tree.leaves[i].features & features::RENDER != 0 => true,
            Some(_) => item.render_elsewhere && tree.render_elsewhere().is_some(),
            None => false,
        }
    }

    /// A leaf without MULTI_INSTANCE is one physical channel: never two of its utterances at once.
    fn channel_free(&self, q: &Queue, item: &Item, tree: &Tree) -> bool {
        let Some(i) = tree.resolve(item.voice, item.no_fallback).leaf else {
            return true;
        };
        let leaf = &tree.leaves[i];
        if leaf.features & features::MULTI_INSTANCE != 0 {
            return true;
        }
        !q.active
            .iter()
            .any(|a| a.backend == Some(leaf.backend) && a.slot == 0)
    }

    /// Start one item. Returns false if it ended immediately (cancelled or done at hand-over).
    fn start(&mut self, queue: u32, item: Item, now: Instant, tree: &Tree) -> bool {
        let table = &self.shared.table;
        let resolved = tree.resolve(item.voice, item.no_fallback);
        let Some(mut li) = resolved.leaf else {
            table.set(item.id, UtteranceState::Cancelled, false);
            self.owner.remove(&item.id);
            return false;
        };
        match item.kind {
            Kind::Gap(d) => {
                table.set(item.id, UtteranceState::Speaking, false);
                self.queues.get_mut(&queue).unwrap().active.push(Active {
                    id: item.id,
                    backend: None,
                    slot: 0,
                    expected_end: Some(now + d),
                    next_poll: None,
                    #[cfg(feature = "audio")]
                    positional: None,
                });
                true
            }
            Kind::Speak {
                text,
                output,
                braille_only,
                ssml,
            } => {
                let leaf = &tree.leaves[li];
                table.set_voice(item.id, leaf.id);
                let b = leaf.backend;
                if braille_only {
                    let _ = self.backends[b].braille(leaf.selector, &text);
                    table.set(item.id, UtteranceState::Done, false);
                    self.owner.remove(&item.id);
                    return false;
                }
                match self.backends[b].speak(
                    leaf.selector,
                    &text,
                    &item.params,
                    false,
                    output,
                    ssml,
                ) {
                    Ok(slot) => {
                        table.set(item.id, UtteranceState::Speaking, item.speak_flat);
                        match self.backends[b].poll_interval() {
                            Some(interval) => {
                                self.queues.get_mut(&queue).unwrap().active.push(Active {
                                    id: item.id,
                                    backend: Some(b),
                                    slot,
                                    expected_end: None,
                                    next_poll: Some(now + interval),
                                    #[cfg(feature = "audio")]
                                    positional: None,
                                });
                                true
                            }
                            None => {
                                // The backend cannot say when it finishes: Done at hand-over.
                                self.backends[b].release(slot);
                                table.set(item.id, UtteranceState::Done, item.speak_flat);
                                self.owner.remove(&item.id);
                                let q = self.queues.get_mut(&queue).unwrap();
                                if q.cap() == 1 && q.opts.gap_ms > 0 {
                                    q.gap_until =
                                        Some(now + Duration::from_millis(q.opts.gap_ms as u64));
                                }
                                false
                            }
                        }
                    }
                    Err(_) => {
                        table.set(item.id, UtteranceState::Cancelled, false);
                        self.owner.remove(&item.id);
                        false
                    }
                }
            }
            #[cfg(feature = "audio")]
            Kind::Positional {
                text,
                ssml,
                trim,
                target,
                pcm,
                ..
            } => {
                if tree.leaves[li].features & features::RENDER == 0 {
                    if item.render_elsewhere {
                        match tree.render_elsewhere() {
                            Some(alt) => li = alt,
                            None => {
                                table.set(item.id, UtteranceState::Cancelled, false);
                                self.owner.remove(&item.id);
                                return false;
                            }
                        }
                    } else if item.speak_flat {
                        let flat = Item {
                            id: item.id,
                            voice: item.voice,
                            params: item.params,
                            no_fallback: item.no_fallback,
                            render_elsewhere: false,
                            speak_flat: true,
                            kind: Kind::Speak {
                                text,
                                output: false,
                                braille_only: false,
                                ssml,
                            },
                        };
                        return self.start(queue, flat, now, tree);
                    } else {
                        table.set(item.id, UtteranceState::Cancelled, false);
                        self.owner.remove(&item.id);
                        return false;
                    }
                }
                let leaf = &tree.leaves[li];
                table.set_voice(item.id, leaf.id);
                let Some(mut pcm) = pcm else {
                    // Not rendered yet (unlimited queue path requested it; sequential waits in start_ready).
                    table.set(item.id, UtteranceState::Cancelled, false);
                    self.owner.remove(&item.id);
                    return false;
                };
                if trim {
                    pcm.trim_silence(0.002);
                }
                let rate = target.intake.info().sample_rate;
                let mono = pcm.mono_at(rate);
                let duration = Duration::from_secs_f64(mono.len() as f64 / rate.max(1) as f64);
                let fired = target.intake.buffer_create_mono(mono).and_then(|buf| {
                    use loadstar_audio::space::{SourceKind, flags};
                    let mut desc = target.desc;
                    desc.kind = SourceKind::Buffer;
                    desc.buffer = buf;
                    desc.repeat_count = 1;
                    desc.flags |= flags::PLAYING | flags::AUTO_REMOVE;
                    let r = target.intake.fire(target.space, desc);
                    let _ = target.intake.buffer_free(buf);
                    r
                });
                match fired {
                    Ok(()) => {
                        table.set(item.id, UtteranceState::Speaking, false);
                        self.queues.get_mut(&queue).unwrap().active.push(Active {
                            id: item.id,
                            backend: None,
                            slot: 0,
                            expected_end: Some(now + duration),
                            next_poll: None,
                            positional: Some((target.intake.clone(), target.space, target.desc.id)),
                        });
                        true
                    }
                    Err(_) => {
                        table.set(item.id, UtteranceState::Cancelled, false);
                        self.owner.remove(&item.id);
                        false
                    }
                }
            }
        }
    }

    /// Request renders for pending positional items (all of them on an unlimited queue; the
    /// next two on a sequential one) so that reaching the head costs no render wait.
    #[cfg(feature = "audio")]
    fn render_ahead(&mut self, queue: u32, tree: &Tree) {
        let Some(tx) = self.render_tx.clone() else {
            return;
        };
        let Some(q) = self.queues.get_mut(&queue) else {
            return;
        };
        let limit = if q.cap() == 1 { 2 } else { usize::MAX };
        let mut requested = 0;
        let mut failed = Vec::new();
        for item in q.pending.iter_mut() {
            if requested >= limit {
                break;
            }
            let Kind::Positional {
                text,
                ssml,
                trim,
                pcm,
                requested: req,
                ..
            } = &mut item.kind
            else {
                continue;
            };
            if pcm.is_some() || *req {
                continue;
            }
            let Some(mut li) = tree.resolve(item.voice, item.no_fallback).leaf else {
                failed.push(item.id);
                continue;
            };
            if tree.leaves[li].features & features::RENDER == 0 {
                if item.render_elsewhere {
                    match tree.render_elsewhere() {
                        Some(alt) => li = alt,
                        None => {
                            failed.push(item.id);
                            continue;
                        }
                    }
                } else {
                    continue; // SPEAK_FLAT or a plain NotSupported: start() decides
                }
            }
            let leaf = &tree.leaves[li];
            *req = true;
            requested += 1;
            let _ = tx.send(RenderReq {
                token: RenderToken::Utterance(item.id),
                backend: leaf.backend,
                selector: leaf.selector,
                text: text.clone(),
                params: item.params,
                ssml: *ssml,
                trim: *trim,
            });
        }
        for id in failed {
            if let Some(pos) = q.pending.iter().position(|i| i.id == id) {
                q.pending.remove(pos);
                self.shared.table.set(id, UtteranceState::Cancelled, false);
            }
        }
    }

    #[cfg(not(feature = "audio"))]
    fn render_ahead(&mut self, _queue: u32, _tree: &Tree) {}
}

/// The render thread: blocking synthesis to PCM, far from both the game thread and the worker.
pub fn render_loop(
    rx: mpsc::Receiver<RenderReq>,
    tx: mpsc::Sender<Msg>,
    mut renderers: HashMap<usize, Box<dyn RenderBackend>>,
) {
    for req in rx {
        let result = match renderers.get_mut(&req.backend) {
            Some(r) => r
                .render(req.selector, &req.text, &req.params, req.ssml)
                .map(|mut p| {
                    if req.trim {
                        p.trim_silence(0.002);
                    }
                    p
                }),
            None => Err(BackendError::NotSupported),
        };
        if tx
            .send(Msg::Rendered {
                token: req.token,
                result,
            })
            .is_err()
        {
            break;
        }
    }
}
