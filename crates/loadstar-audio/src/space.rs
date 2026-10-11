//! An audio space (spec §3): a listener, buses, and sources with geometry. This module is the
//! audio-thread side; commands arrive already built (chains, buses) so the period loop does
//! not allocate. Voices come from a per-space pool that is pre-warmed on the game thread and
//! grows only when more sources are alive at once than ever before; a retiring voice goes back
//! to the pool and hands anything it owned that must be freed (its buffer reference, any effect
//! nodes) to the garbage ring, so the callback never calls the allocator in steady state.

use std::sync::Arc;

use loadstar_core::geometry::{Bounds, Vec3};

use crate::chain::{Chain, DistanceParams};
use crate::commands::Garbage;
use crate::dsp::{Noise, NoiseColor, Oscillator, Ramp, Waveform};
use crate::render::{Placement, Renderer, SpatializerParams, VoiceRenderer};
use crate::stream::{Pull, StreamSource};

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SpaceMode {
    /// Pan by bearing in the XZ plane; elevation ignored.
    #[default]
    Planar = 0,
    Spatial = 1,
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SourceKind {
    #[default]
    Buffer = 0,
    Oscillator = 1,
    Noise = 2,
    /// Plays from a ring filled while it plays (`buffer` = the stream id). Plays once.
    Stream = 3,
}

impl SourceKind {
    pub fn from_u8(v: u8) -> SourceKind {
        match v {
            1 => SourceKind::Oscillator,
            2 => SourceKind::Noise,
            3 => SourceKind::Stream,
            _ => SourceKind::Buffer,
        }
    }
}

/// Source flags (spec §3).
pub mod flags {
    pub const PLAYING: u32 = 1 << 0;
    pub const RESTART: u32 = 1 << 1;
    pub const RELATIVE: u32 = 1 << 2;
    pub const AUTO_REMOVE: u32 = 1 << 3;
}

/// A source record (spec §3). All numeric; the id is the mod's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SourceDesc {
    pub id: u64,
    pub kind: SourceKind,
    /// Oscillator waveform or noise colour.
    pub waveform: u8,
    pub priority: u8,
    /// Plays per trigger; 0 = forever.
    pub repeat_count: u16,
    pub buffer: u32,
    pub bus: u32,
    pub flags: u32,
    pub bounds: Bounds,
    pub gain: f32,
    /// Buffer: playback rate. Oscillator: frequency in Hz.
    pub pitch: f32,
    /// Lateral crossover in the space's units; for RELATIVE sources, the pan (−1..1).
    pub pan_width: f32,
    pub repeat_gap_ms: f32,
}

impl SourceDesc {
    fn base(id: u64, kind: SourceKind, bounds: Bounds) -> SourceDesc {
        SourceDesc {
            id,
            kind,
            waveform: 0,
            priority: 0,
            repeat_count: 1,
            buffer: 0,
            bus: 0,
            flags: flags::PLAYING,
            bounds,
            gain: 1.0,
            pitch: 1.0,
            pan_width: 1.0,
            repeat_gap_ms: 0.0,
        }
    }

    pub fn buffer(id: u64, buffer: u32, bounds: Bounds) -> SourceDesc {
        SourceDesc {
            buffer,
            ..Self::base(id, SourceKind::Buffer, bounds)
        }
    }

    pub fn oscillator(id: u64, hz: f32, waveform: Waveform, bounds: Bounds) -> SourceDesc {
        SourceDesc {
            pitch: hz,
            waveform: waveform as u8,
            repeat_count: 0,
            ..Self::base(id, SourceKind::Oscillator, bounds)
        }
    }

    pub fn noise(id: u64, color: NoiseColor, bounds: Bounds) -> SourceDesc {
        SourceDesc {
            waveform: color as u8,
            repeat_count: 0,
            ..Self::base(id, SourceKind::Noise, bounds)
        }
    }

    pub fn looping(mut self) -> SourceDesc {
        self.repeat_count = 0;
        self
    }
    pub fn repeats(mut self, count: u16, gap_ms: f32) -> SourceDesc {
        self.repeat_count = count;
        self.repeat_gap_ms = gap_ms;
        self
    }
    pub fn relative(mut self, pan: f32) -> SourceDesc {
        self.flags |= flags::RELATIVE;
        self.pan_width = pan;
        self
    }
    pub fn on_bus(mut self, bus: u32) -> SourceDesc {
        self.bus = bus;
        self
    }
    pub fn with_gain(mut self, gain: f32) -> SourceDesc {
        self.gain = gain;
        self
    }
    pub fn with_flags(mut self, set: u32, clear: u32) -> SourceDesc {
        self.flags = (self.flags | set) & !clear;
        self
    }
    pub fn playing(&self) -> bool {
        self.flags & flags::PLAYING != 0
    }

    /// Equality for "did the record change" — the edge-triggered RESTART bit is not state.
    fn same_record(&self, other: &SourceDesc) -> bool {
        let a = SourceDesc {
            flags: self.flags & !flags::RESTART,
            ..*self
        };
        let b = SourceDesc {
            flags: other.flags & !flags::RESTART,
            ..*other
        };
        a == b
    }
}

/// Mono samples at the mixer rate.
pub struct Buffer {
    pub samples: Vec<f32>,
    pub rate: u32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Listener {
    pub position: Vec3,
    pub forward: Vec3,
    pub up: Vec3,
}

impl Default for Listener {
    fn default() -> Self {
        Listener {
            position: Vec3::default(),
            forward: Vec3::new(0.0, 0.0, -1.0),
            up: Vec3::new(0.0, 1.0, 0.0),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpaceConfig {
    pub mode: SpaceMode,
    pub gain: f32,
    /// Units per metre.
    pub unit_scale: f32,
}

impl Default for SpaceConfig {
    fn default() -> Self {
        SpaceConfig {
            mode: SpaceMode::Planar,
            gain: 1.0,
            unit_scale: 1.0,
        }
    }
}

pub struct Bus {
    gain: Ramp,
    chain: Chain,
    buf: Vec<f32>,
}

impl Bus {
    pub fn new(gain: f32, rate: f32, block: usize) -> Bus {
        Bus {
            gain: Ramp::new(gain),
            chain: Chain::empty(rate, 2, block),
            buf: vec![0.0; block * 2],
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Playing,
    /// Frames of silence remaining between repeats.
    Gap(usize),
    /// Sequence complete (or never started); silent until retriggered.
    Idle,
}

/// Which chain a source-level operation targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    Pre,
    Post,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceState {
    pub playing: bool,
    pub position_frames: u64,
    /// `u16::MAX` for endless sources.
    pub plays_remaining: u16,
}

/// One playing (or pooled) source. Pool-recycled: `blank` allocates once, `arm` rebinds it to
/// a record without allocating, `disarm` strips what must be freed off the audio thread.
struct Voice {
    desc: SourceDesc,
    declared: bool,
    remove_when_done: bool,
    buffer: Option<Arc<Buffer>>,
    /// A streaming source this voice took from the registry (Stream kind).
    stream: Option<StreamSource>,
    pos: f64,
    osc: Oscillator,
    noise: Noise,
    plays_done: u16,
    phase: Phase,
    level: Ramp,
    /// Started from silence this period: the level primes to its target instead of ramping
    /// up from zero, so a short cue keeps its attack.
    fresh: bool,
    stopping: bool,
    /// Muted by the voice cap.
    stolen: bool,
    seen: bool,
    pre: Chain,
    post: Chain,
    render: Box<dyn VoiceRenderer>,
}

impl Voice {
    /// A pooled, idle voice. Allocates (chains, renderer state): game thread, or the one-time
    /// growth path when more voices are alive than the pool was warmed for.
    fn blank(rate: f32, block: usize, renderer: &dyn Renderer) -> Voice {
        Voice {
            desc: SourceDesc::base(0, SourceKind::Buffer, Bounds::Point(Vec3::default())),
            declared: false,
            remove_when_done: true,
            buffer: None,
            stream: None,
            pos: 0.0,
            osc: Oscillator::default(),
            noise: Noise::new(1),
            plays_done: 0,
            phase: Phase::Idle,
            level: Ramp::new(0.0),
            fresh: false,
            stopping: false,
            stolen: false,
            seen: true,
            pre: Chain::empty(rate, 1, block),
            post: Chain::empty(rate, 2, block),
            render: renderer.make_voice(rate, block),
        }
    }

    /// Bind a pooled voice to a record. No allocation.
    fn arm(
        &mut self,
        desc: SourceDesc,
        declared: bool,
        buffer: Option<Arc<Buffer>>,
        stream: Option<StreamSource>,
    ) {
        self.desc = desc;
        self.declared = declared;
        self.remove_when_done = !declared || desc.flags & flags::AUTO_REMOVE != 0;
        self.buffer = buffer;
        self.stream = stream;
        self.pos = 0.0;
        self.osc = Oscillator::default();
        self.noise = Noise::new(desc.id as u32 ^ 0x9E37_79B9);
        self.plays_done = 0;
        self.phase = Phase::Idle;
        self.level = Ramp::new(0.0);
        self.fresh = false;
        self.stopping = false;
        self.stolen = false;
        self.seen = true;
        if desc.playing() {
            self.start();
        }
    }

    fn active(&self) -> bool {
        self.phase != Phase::Idle && !self.stopping && !self.stolen
    }

    fn start(&mut self) {
        self.fresh = self.phase == Phase::Idle;
        self.pos = 0.0;
        self.plays_done = 0;
        self.phase = Phase::Playing;
        self.stopping = false;
    }

    fn stop(&mut self) {
        if self.phase != Phase::Idle {
            self.stopping = true;
            self.level.set_target(0.0);
        }
    }
}

pub struct Space {
    cfg: SpaceConfig,
    rate: f32,
    block: usize,
    gain: Ramp,
    chain: Chain,
    listener: Listener,
    buses: Vec<Bus>,
    voices: Vec<Voice>,
    /// Idle voices ready to be armed. Warmed on the game thread; grows only on a new peak.
    pool: Vec<Voice>,
    finished: Vec<u64>,
    /// What retiring voices gave up, for the engine to hand to the game thread.
    released: Vec<Garbage>,
    renderer: Box<dyn Renderer>,
    default_spatializer: SpatializerParams,
    mono: Vec<f32>,
    stereo: Vec<f32>,
    mix: Vec<f32>,
}

impl Space {
    /// A space with an empty pool: voices allocate on first use (tests; the device always
    /// warms the pool with [`Space::with_pool`]).
    pub fn new(cfg: SpaceConfig, rate: f32, block: usize, renderer: Box<dyn Renderer>) -> Space {
        Space::with_pool(cfg, rate, block, renderer, 0)
    }

    /// A space with `prewarm` voices ready, so the audio thread allocates nothing until more
    /// than that many sources are alive at once. Game thread.
    pub fn with_pool(
        cfg: SpaceConfig,
        rate: f32,
        block: usize,
        renderer: Box<dyn Renderer>,
        prewarm: usize,
    ) -> Space {
        let mut pool = Vec::with_capacity(prewarm);
        for _ in 0..prewarm {
            pool.push(Voice::blank(rate, block, renderer.as_ref()));
        }
        Space {
            cfg,
            rate,
            block,
            gain: Ramp::new(cfg.gain),
            chain: Chain::empty(rate, 2, block),
            listener: Listener::default(),
            buses: vec![Bus::new(1.0, rate, block)],
            voices: Vec::with_capacity(prewarm),
            pool,
            finished: Vec::with_capacity(prewarm.max(16)),
            released: Vec::with_capacity(prewarm.max(16) * 2),
            renderer,
            default_spatializer: SpatializerParams::default(),
            mono: vec![0.0; block],
            stereo: vec![0.0; block * 2],
            mix: vec![0.0; block * 2],
        }
    }

    pub fn config(&self) -> SpaceConfig {
        self.cfg
    }

    pub fn set_listener(&mut self, listener: Listener) {
        self.listener = listener;
    }

    pub fn set_gain(&mut self, gain: f32) {
        self.gain.set_target(gain);
    }

    pub fn chain_mut(&mut self) -> &mut Chain {
        &mut self.chain
    }

    pub fn set_default_spatializer(&mut self, params: SpatializerParams) {
        self.default_spatializer = params;
    }

    // --- Buses ---

    pub fn add_bus(&mut self, bus: Bus) -> u32 {
        self.buses.push(bus);
        self.buses.len() as u32 - 1
    }

    pub fn bus_count(&self) -> usize {
        self.buses.len()
    }

    pub fn set_bus_gain(&mut self, bus: u32, gain: f32) -> bool {
        match self.buses.get_mut(bus as usize) {
            Some(b) => {
                b.gain.set_target(gain);
                true
            }
            None => false,
        }
    }

    pub fn bus_chain_mut(&mut self, bus: u32) -> Option<&mut Chain> {
        self.buses.get_mut(bus as usize).map(|b| &mut b.chain)
    }

    // --- Sources ---

    fn find(&mut self, id: u64, declared: bool) -> Option<usize> {
        if id == 0 {
            return None;
        }
        self.voices
            .iter()
            .position(|v| v.desc.id == id && v.declared == declared)
    }

    fn create_voice(
        &mut self,
        desc: SourceDesc,
        declared: bool,
        buffers: &[Option<Arc<Buffer>>],
        streams: &mut [Option<StreamSource>],
    ) -> usize {
        let buffer = if desc.kind == SourceKind::Buffer {
            buffers.get(desc.buffer as usize).cloned().flatten()
        } else {
            None
        };
        let stream = if desc.kind == SourceKind::Stream {
            streams.get_mut(desc.buffer as usize).and_then(Option::take)
        } else {
            None
        };
        let mut voice = match self.pool.pop() {
            Some(v) => v,
            None => Voice::blank(self.rate, self.block, self.renderer.as_ref()),
        };
        voice.arm(desc, declared, buffer, stream);
        self.voices.push(voice);
        self.voices.len() - 1
    }

    /// Return voice `index` to the pool, routing its buffer reference and any effect nodes to
    /// the garbage ring so nothing is freed on this thread.
    fn retire(&mut self, index: usize) {
        let mut v = self.voices.swap_remove(index);
        if let Some(b) = v.buffer.take() {
            self.released.push(Garbage::Buffer(b));
        }
        if let Some(s) = v.stream.take() {
            self.released.push(Garbage::Stream(s));
        }
        if !v.pre.is_empty() {
            self.released.push(Garbage::Nodes(v.pre.take_nodes()));
        }
        if !v.post.is_empty() {
            self.released.push(Garbage::Nodes(v.post.take_nodes()));
        }
        v.pre.reset();
        v.post.reset();
        v.render.reset();
        v.phase = Phase::Idle;
        self.pool.push(v);
    }

    fn apply_desc(
        &mut self,
        index: usize,
        desc: SourceDesc,
        buffers: &[Option<Arc<Buffer>>],
        streams: &mut [Option<StreamSource>],
    ) {
        let voice = &mut self.voices[index];
        let old = voice.desc;
        let restart = desc.flags & flags::RESTART != 0;
        let started = !old.playing() && desc.playing();
        let stopped = old.playing() && !desc.playing();
        let buffer_changed = desc.kind != old.kind
            || ((desc.kind == SourceKind::Buffer || desc.kind == SourceKind::Stream)
                && desc.buffer != old.buffer);
        voice.desc = SourceDesc {
            flags: desc.flags & !flags::RESTART,
            ..desc
        };
        voice.remove_when_done = !voice.declared || desc.flags & flags::AUTO_REMOVE != 0;
        if buffer_changed {
            let next = if desc.kind == SourceKind::Buffer {
                buffers.get(desc.buffer as usize).cloned().flatten()
            } else {
                None
            };
            if let Some(old) = std::mem::replace(&mut voice.buffer, next) {
                self.released.push(Garbage::Buffer(old));
            }
            let next_stream = if desc.kind == SourceKind::Stream {
                streams.get_mut(desc.buffer as usize).and_then(Option::take)
            } else {
                None
            };
            if let Some(old) = std::mem::replace(&mut voice.stream, next_stream) {
                self.released.push(Garbage::Stream(old));
            }
        }
        if stopped {
            voice.stop();
        } else if restart || started || (buffer_changed && desc.playing()) {
            voice.start();
        }
    }

    /// Insert or update a declared source.
    pub fn upsert(&mut self, desc: SourceDesc, buffers: &[Option<Arc<Buffer>>]) {
        self.upsert_with(desc, buffers, &mut []);
    }

    /// `upsert` with the stream registry (Stream-kind sources take their consumer from it).
    pub fn upsert_with(
        &mut self,
        desc: SourceDesc,
        buffers: &[Option<Arc<Buffer>>],
        streams: &mut [Option<StreamSource>],
    ) {
        match self.find(desc.id, true) {
            Some(i) => {
                self.voices[i].seen = true;
                if !self.voices[i].desc.same_record(&desc) || desc.flags & flags::RESTART != 0 {
                    self.apply_desc(i, desc, buffers, streams);
                }
            }
            None => {
                self.create_voice(desc, true, buffers, streams);
            }
        }
    }

    /// Declarative snapshot: upsert all, stop-and-remove declared sources absent from it.
    pub fn sync(&mut self, descs: &[SourceDesc], buffers: &[Option<Arc<Buffer>>]) {
        self.sync_with(descs, buffers, &mut []);
    }

    /// `sync` with the stream registry.
    pub fn sync_with(
        &mut self,
        descs: &[SourceDesc],
        buffers: &[Option<Arc<Buffer>>],
        streams: &mut [Option<StreamSource>],
    ) {
        for v in &mut self.voices {
            if v.declared {
                v.seen = false;
            }
        }
        for &d in descs {
            self.upsert_with(d, buffers, streams);
        }
        for v in &mut self.voices {
            if v.declared && !v.seen {
                v.remove_when_done = true;
                v.stop();
                if v.phase == Phase::Idle {
                    v.stolen = true; // idle and unseen: drop on the next period
                }
            }
        }
    }

    pub fn remove(&mut self, id: u64) -> bool {
        match self.find(id, true) {
            Some(i) => {
                let v = &mut self.voices[i];
                v.remove_when_done = true;
                v.stop();
                if v.phase == Phase::Idle {
                    v.stolen = true;
                }
                true
            }
            None => false,
        }
    }

    /// Play a sequence and remove the source when it drains. Id 0 is anonymous; a nonzero id
    /// retriggers an existing fired voice with that id instead of stacking a new one.
    pub fn fire(&mut self, desc: SourceDesc, buffers: &[Option<Arc<Buffer>>]) {
        self.fire_with(desc, buffers, &mut []);
    }

    /// `fire` with the stream registry.
    pub fn fire_with(
        &mut self,
        desc: SourceDesc,
        buffers: &[Option<Arc<Buffer>>],
        streams: &mut [Option<StreamSource>],
    ) {
        let desc = SourceDesc {
            flags: desc.flags | flags::PLAYING | flags::RESTART,
            ..desc
        };
        match self.find(desc.id, false) {
            Some(i) => self.apply_desc(i, desc, buffers, streams),
            None => {
                self.create_voice(desc, false, buffers, streams);
            }
        }
    }

    pub fn stop(&mut self, id: u64) -> bool {
        let mut any = false;
        for v in &mut self.voices {
            if v.desc.id == id && id != 0 {
                v.stop();
                any = true;
            }
        }
        any
    }

    pub fn source_chain_mut(&mut self, id: u64, stage: Stage) -> Option<&mut Chain> {
        let v = self
            .voices
            .iter_mut()
            .find(|v| v.desc.id == id && id != 0)?;
        Some(match stage {
            Stage::Pre => &mut v.pre,
            Stage::Post => &mut v.post,
        })
    }

    pub fn state(&self, id: u64) -> Option<SourceState> {
        let v = self.voices.iter().find(|v| v.desc.id == id && id != 0)?;
        Some(SourceState {
            playing: v.active(),
            position_frames: v.pos as u64,
            plays_remaining: if v.desc.repeat_count == 0 {
                u16::MAX
            } else {
                v.desc.repeat_count.saturating_sub(v.plays_done)
            },
        })
    }

    pub fn source_count(&self) -> usize {
        self.voices.len()
    }

    /// Idle voices waiting in the pool.
    pub fn pool_len(&self) -> usize {
        self.pool.len()
    }

    /// Ids (nonzero) whose sequences ended since the last call, in order. No allocation.
    pub fn drain_finished(&mut self, mut f: impl FnMut(u64)) {
        for id in self.finished.drain(..) {
            f(id);
        }
    }

    /// Ids (nonzero) whose sequences ended since the last call (tests; allocates).
    pub fn take_finished(&mut self) -> Vec<u64> {
        std::mem::take(&mut self.finished)
    }

    /// Hand released state to `sink` until it is empty or the sink refuses (ring full); a
    /// refused item is kept for the next call.
    pub fn drain_garbage(&mut self, mut sink: impl FnMut(Garbage) -> Result<(), Garbage>) {
        while let Some(g) = self.released.pop() {
            if let Err(g) = sink(g) {
                self.released.push(g);
                break;
            }
        }
    }

    /// `(index, priority, level)` of every audible voice, for the engine's voice cap.
    pub fn audible(&self) -> impl Iterator<Item = (usize, u8, f32)> + '_ {
        self.voices
            .iter()
            .enumerate()
            .filter(|(_, v)| v.active())
            .map(|(i, v)| (i, v.desc.priority, v.level.target()))
    }

    pub fn steal(&mut self, index: usize) {
        if let Some(v) = self.voices.get_mut(index) {
            v.stolen = true;
            v.level.set_target(0.0);
        }
    }

    pub fn unsteal_all(&mut self) {
        for v in &mut self.voices {
            v.stolen = false;
        }
    }

    // --- Mixing ---

    fn placement(&self, desc: &SourceDesc) -> Placement {
        if desc.flags & flags::RELATIVE != 0 {
            return Placement::relative(desc.pan_width);
        }
        let l = &self.listener;
        let nearest = desc.bounds.nearest_point(l.position);
        let inside = desc.bounds.contains(l.position);
        let mut d = nearest - l.position;
        let mut forward = l.forward;
        let mut up = l.up;
        if self.cfg.mode == SpaceMode::Planar {
            d.y = 0.0;
            forward.y = 0.0;
            up = Vec3::new(0.0, 1.0, 0.0);
        }
        let forward = normalize(forward, Vec3::new(0.0, 0.0, -1.0));
        let up = normalize(up, Vec3::new(0.0, 1.0, 0.0));
        let right = cross(forward, up);
        Placement {
            right: dot(d, right),
            up: dot(d, up),
            forward: dot(d, forward),
            distance: d.length(),
            inside,
            pan_width: desc.pan_width.max(1e-3),
            relative: false,
            pan: 0.0,
        }
    }

    /// Fill `mono` for one voice, advancing its sequence. Returns false if it produced nothing.
    fn generate(v: &mut Voice, mono: &mut [f32], rate: f32, finished: &mut Vec<u64>) -> bool {
        mono.iter_mut().for_each(|s| *s = 0.0);
        if v.phase == Phase::Idle {
            return false;
        }
        match v.desc.kind {
            SourceKind::Oscillator => {
                let w = Waveform::from_u8(v.desc.waveform);
                for s in mono.iter_mut() {
                    *s = v.osc.next(v.desc.pitch, rate, w);
                }
                true
            }
            SourceKind::Noise => {
                let c = NoiseColor::from_u8(v.desc.waveform);
                for s in mono.iter_mut() {
                    *s = v.noise.next(c);
                }
                true
            }
            SourceKind::Stream => {
                let Some(s) = v.stream.as_mut() else {
                    v.phase = Phase::Idle;
                    return false;
                };
                match s.pull(mono) {
                    Pull::Prefilling => true, // silent but alive
                    Pull::Produced(n) => {
                        v.pos += n as f64;
                        true
                    }
                    Pull::Finished => {
                        v.phase = Phase::Idle;
                        if v.desc.id != 0 {
                            finished.push(v.desc.id);
                        }
                        false
                    }
                }
            }
            SourceKind::Buffer => {
                let Some(buf) = v.buffer.as_ref() else {
                    v.phase = Phase::Idle;
                    return false;
                };
                let samples = &buf.samples;
                if samples.is_empty() {
                    v.phase = Phase::Idle;
                    return false;
                }
                let step = (v.desc.pitch.max(0.01) * buf.rate as f32 / rate) as f64;
                let gap_frames = (v.desc.repeat_gap_ms.max(0.0) / 1000.0 * rate) as usize;
                let mut any = false;
                for s in mono.iter_mut() {
                    match v.phase {
                        Phase::Playing => {
                            let i = v.pos as usize;
                            if i + 1 < samples.len() {
                                let frac = (v.pos - i as f64) as f32;
                                *s = samples[i] + (samples[i + 1] - samples[i]) * frac;
                                v.pos += step;
                                any = true;
                            } else {
                                v.plays_done = v.plays_done.saturating_add(1);
                                let more =
                                    v.desc.repeat_count == 0 || v.plays_done < v.desc.repeat_count;
                                if more {
                                    v.pos = 0.0;
                                    v.phase = if gap_frames > 0 {
                                        Phase::Gap(gap_frames)
                                    } else {
                                        Phase::Playing
                                    };
                                } else {
                                    // The record keeps the mod's declared PLAYING bit; "finished"
                                    // is the phase, so an unchanged re-send does not retrigger.
                                    v.phase = Phase::Idle;
                                    if v.desc.id != 0 {
                                        finished.push(v.desc.id);
                                    }
                                    break;
                                }
                            }
                        }
                        Phase::Gap(n) => {
                            v.phase = if n <= 1 {
                                Phase::Playing
                            } else {
                                Phase::Gap(n - 1)
                            };
                            any = true;
                        }
                        Phase::Idle => break,
                    }
                }
                any
            }
        }
    }

    /// Mix one period into `out` (interleaved stereo, `out.len() == 2 * frames`), accumulating.
    pub fn process(&mut self, out: &mut [f32]) {
        let frames = (out.len() / 2).min(self.block);
        if frames == 0 {
            return;
        }
        for bus in &mut self.buses {
            bus.buf[..frames * 2].iter_mut().for_each(|s| *s = 0.0);
        }

        // Voices → buses. A source without its own DISTANCE node falls back to the SPACE chain's
        // (so a mod can declare "no distance model, I own gain" once per space), then to the
        // implicit default.
        let space_distance = self.chain.distance();
        let mut i = 0;
        while i < self.voices.len() {
            let placement = self.placement(&self.voices[i].desc);
            let unit_scale = self.cfg.unit_scale;
            let default_spat = self.default_spatializer;
            let rate = self.rate;
            let v = &mut self.voices[i];

            let distance_gain = if placement.relative || placement.inside {
                1.0
            } else {
                v.pre
                    .distance()
                    .or(space_distance)
                    .unwrap_or_else(|| DistanceParams::default_for(unit_scale))
                    .gain(placement.distance)
            };
            if !v.stopping && !v.stolen {
                let target = v.desc.gain.max(0.0) * distance_gain;
                // A voice starting from silence lands on its level at once; only changes ramp.
                if v.fresh {
                    v.level.set_now(target)
                } else {
                    v.level.set_target(target)
                }
            }
            v.fresh = false;

            let mono = &mut self.mono[..frames];
            let produced = Self::generate(v, mono, rate, &mut self.finished);
            let (l0, ls) = v.level.block(frames);
            let silent = !produced || (l0 == 0.0 && ls == 0.0);
            if !silent {
                for (k, s) in mono.iter_mut().enumerate() {
                    *s *= l0 + ls * k as f32;
                }
                v.pre.process(mono);
                let stereo = &mut self.stereo[..frames * 2];
                let params = v
                    .pre
                    .spatializer()
                    .map(SpatializerParams::from_desc)
                    .unwrap_or(default_spat);
                v.render.process(mono, &placement, &params, stereo);
                v.post.process(stereo);
                let bus_index = (v.desc.bus as usize).min(self.buses.len() - 1);
                let bus = &mut self.buses[bus_index];
                for (o, s) in bus.buf[..frames * 2].iter_mut().zip(stereo.iter()) {
                    *o += *s;
                }
            }

            // Retire voices that are done and either stopping (ramp complete) or stolen while idle.
            let done_sequence = v.phase == Phase::Idle && v.remove_when_done;
            let ramped_out =
                (v.stopping || v.stolen) && v.level.is_settled() && v.level.target() == 0.0;
            if (done_sequence && (v.level.is_settled() || !produced))
                || (ramped_out && (v.remove_when_done || v.stolen && v.phase == Phase::Idle))
            {
                if v.stopping && v.declared && !v.remove_when_done {
                    v.phase = Phase::Idle;
                    v.stopping = false;
                    i += 1;
                    continue;
                }
                self.retire(i);
                continue;
            }
            if v.stopping && v.level.is_settled() && v.level.target() == 0.0 {
                v.phase = Phase::Idle;
                v.stopping = false;
            }
            i += 1;
        }

        // Buses → space mix.
        let mix = &mut self.mix[..frames * 2];
        mix.iter_mut().for_each(|s| *s = 0.0);
        for bus in &mut self.buses {
            let (g0, gs) = bus.gain.block(frames);
            let buf = &mut bus.buf[..frames * 2];
            for f in 0..frames {
                let g = g0 + gs * f as f32;
                buf[2 * f] *= g;
                buf[2 * f + 1] *= g;
            }
            bus.chain.process(buf);
            for (m, s) in mix.iter_mut().zip(buf.iter()) {
                *m += *s;
            }
        }

        // Space gain + chain → out (accumulate).
        let (g0, gs) = self.gain.block(frames);
        for f in 0..frames {
            let g = g0 + gs * f as f32;
            mix[2 * f] *= g;
            mix[2 * f + 1] *= g;
        }
        self.chain.process(mix);
        for (o, s) in out[..frames * 2].iter_mut().zip(mix.iter()) {
            *o += *s;
        }
    }
}

fn dot(a: Vec3, b: Vec3) -> f32 {
    a.x * b.x + a.y * b.y + a.z * b.z
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    Vec3::new(
        a.y * b.z - a.z * b.y,
        a.z * b.x - a.x * b.z,
        a.x * b.y - a.y * b.x,
    )
}

fn normalize(v: Vec3, fallback: Vec3) -> Vec3 {
    let len = v.length();
    if len < 1e-6 {
        fallback
    } else {
        Vec3::new(v.x / len, v.y / len, v.z / len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HeadModel;

    const RATE: f32 = 48000.0;
    const BLOCK: usize = 256;

    fn space() -> Space {
        Space::new(SpaceConfig::default(), RATE, BLOCK, Box::new(HeadModel))
    }

    fn buffers(frames: usize) -> Vec<Option<Arc<Buffer>>> {
        vec![Some(Arc::new(Buffer {
            samples: vec![0.5; frames],
            rate: 48000,
        }))]
    }

    fn point(x: f32, z: f32) -> Bounds {
        Bounds::Point(Vec3::new(x, 0.0, z))
    }

    fn render(space: &mut Space, blocks: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..blocks {
            let mut out = vec![0.0; BLOCK * 2];
            space.process(&mut out);
            all.extend(out);
        }
        all
    }

    fn energy(buf: &[f32], channel: usize) -> f32 {
        buf.iter().skip(channel).step_by(2).map(|s| s * s).sum()
    }

    #[test]
    fn a_source_to_the_east_is_heard_on_the_right_and_follows_the_listener() {
        let mut s = space();
        let b = buffers(48000);
        s.set_listener(Listener::default()); // at origin, facing north (-Z); east is +X = right
        s.sync(&[SourceDesc::buffer(1, 0, point(5.0, 0.0)).looping()], &b);
        let out = render(&mut s, 8);
        assert!(energy(&out, 1) > energy(&out, 0) * 3.0, "east is right");
        // Turn to face east: the source is now straight ahead → centred.
        s.set_listener(Listener {
            forward: Vec3::new(1.0, 0.0, 0.0),
            ..Listener::default()
        });
        let out = render(&mut s, 12);
        let tail = &out[out.len() / 2..];
        assert!(
            (energy(tail, 0) - energy(tail, 1)).abs() < energy(tail, 0) * 0.05,
            "ahead is centred"
        );
    }

    #[test]
    fn distance_attenuates_and_inside_bounds_is_full_gain() {
        let mut s = space();
        let b = buffers(48000);
        s.sync(&[SourceDesc::buffer(1, 0, point(0.0, -1.0)).looping()], &b);
        let near = energy(&render(&mut s, 6), 0) + energy(&render(&mut s, 0), 1);
        s.sync(&[SourceDesc::buffer(1, 0, point(0.0, -8.0)).looping()], &b);
        let _ = render(&mut s, 2);
        let far = energy(&render(&mut s, 6), 0);
        assert!(
            far < near * 0.1,
            "8 units away is much quieter: near {near}, far {far}"
        );
        let room = Bounds::Box {
            center: Vec3::new(0.0, 0.0, 0.0),
            extents: Vec3::new(3.0, 3.0, 3.0),
        };
        s.sync(&[SourceDesc::buffer(1, 0, room).looping()], &b);
        let _ = render(&mut s, 2);
        let inside = render(&mut s, 6);
        assert!(
            energy(&inside, 0) > far * 20.0
                && (energy(&inside, 0) - energy(&inside, 1)).abs() < 1e-3,
            "inside: loud and centred"
        );
    }

    #[test]
    fn one_shot_in_a_snapshot_plays_once_and_stays_finished() {
        let mut s = space();
        let b = buffers(BLOCK * 2);
        let desc = SourceDesc::buffer(7, 0, point(0.0, -2.0));
        s.sync(&[desc], &b);
        let first = energy(&render(&mut s, 4), 0);
        assert!(first > 0.0);
        assert_eq!(s.take_finished(), vec![7]);
        for _ in 0..3 {
            s.sync(&[desc], &b);
            assert_eq!(
                energy(&render(&mut s, 2), 0),
                0.0,
                "re-sending the same record does not retrigger"
            );
        }
        assert!(s.take_finished().is_empty());
        assert!(!s.state(7).unwrap().playing);
        s.sync(&[desc.with_flags(flags::RESTART, 0)], &b);
        assert!(energy(&render(&mut s, 1), 0) > 0.0, "RESTART retriggers");
        assert_eq!(s.source_count(), 1, "declared: stays registered");
    }

    #[test]
    fn fired_sources_repeat_with_gaps_and_remove_themselves() {
        let mut s = space();
        let b = buffers(BLOCK); // exactly one block long
        s.fire(
            SourceDesc::buffer(9, 0, point(0.0, -2.0)).repeats(3, BLOCK as f32 / 48.0),
            &b,
        ); // gap = 1 block
        let mut per_block = Vec::new();
        for _ in 0..8 {
            per_block.push(energy(&render(&mut s, 1), 0) > 1e-6);
        }
        // play, gap, play, gap, play, then silence
        assert_eq!(
            &per_block[..6],
            &[true, false, true, false, true, false],
            "{per_block:?}"
        );
        assert_eq!(s.take_finished(), vec![9]);
        assert_eq!(s.source_count(), 0, "fired: gone when done");
        // Anonymous fire is never reported and never stacks with anything.
        s.fire(SourceDesc::buffer(0, 0, point(0.0, -2.0)), &b);
        let _ = render(&mut s, 3);
        assert!(s.take_finished().is_empty());
        assert_eq!(s.source_count(), 0);
    }

    #[test]
    fn retired_voices_return_to_the_pool_and_release_their_buffer_as_garbage() {
        let mut s = Space::with_pool(SpaceConfig::default(), RATE, BLOCK, Box::new(HeadModel), 2);
        assert_eq!(s.pool_len(), 2);
        let b = buffers(BLOCK);
        s.fire(SourceDesc::buffer(1, 0, point(0.0, -2.0)), &b);
        s.fire(SourceDesc::buffer(2, 0, point(1.0, -2.0)), &b);
        assert_eq!(
            (s.source_count(), s.pool_len()),
            (2, 0),
            "both came from the pool"
        );
        assert_eq!(
            Arc::strong_count(b[0].as_ref().unwrap()),
            3,
            "two voices hold the buffer"
        );
        let _ = render(&mut s, 3);
        assert_eq!((s.source_count(), s.pool_len()), (0, 2), "both went back");
        let mut released = Vec::new();
        s.drain_garbage(|g| {
            released.push(g);
            Ok(())
        });
        assert_eq!(
            released.len(),
            2,
            "each retiring voice released its buffer reference"
        );
        assert!(released.iter().all(|g| matches!(g, Garbage::Buffer(_))));
        drop(released);
        assert_eq!(
            Arc::strong_count(b[0].as_ref().unwrap()),
            1,
            "no reference left on the audio side"
        );
        // A refused sink keeps the item for next time.
        s.fire(SourceDesc::buffer(3, 0, point(0.0, -2.0)), &b);
        let _ = render(&mut s, 3);
        s.drain_garbage(Err);
        let mut kept = 0;
        s.drain_garbage(|_| {
            kept += 1;
            Ok(())
        });
        assert_eq!(kept, 1);
    }

    #[test]
    fn a_fresh_voice_starts_at_full_level_and_a_stop_still_ramps() {
        let mut s = space();
        let b = buffers(BLOCK * 4);
        s.fire(SourceDesc::buffer(4, 0, point(0.0, -1.0)).looping(), &b);
        let mut out = vec![0.0; BLOCK * 2];
        s.process(&mut out);
        // Constant 0.5 input, centred: the very first sample is already at level (no fade-in).
        let first = out[0].abs() + out[1].abs();
        let last = out[BLOCK * 2 - 2].abs() + out[BLOCK * 2 - 1].abs();
        assert!(
            (first - last).abs() < last * 0.05,
            "no ramp across the first block: {first} vs {last}"
        );
        s.stop(4);
        let mut out = vec![0.0; BLOCK * 2];
        s.process(&mut out);
        assert!(
            out[0].abs() > out[BLOCK * 2 - 2].abs() * 10.0,
            "stopping ramps down over the block"
        );
    }

    #[test]
    fn sync_omission_removes_and_gain_zero_keeps_alive() {
        let mut s = space();
        let b = buffers(48000);
        s.sync(
            &[
                SourceDesc::buffer(1, 0, point(1.0, 0.0)).looping(),
                SourceDesc::buffer(2, 0, point(-1.0, 0.0)).looping(),
            ],
            &b,
        );
        let _ = render(&mut s, 2);
        assert_eq!(s.source_count(), 2);
        s.sync(
            &[SourceDesc::buffer(1, 0, point(1.0, 0.0))
                .looping()
                .with_gain(0.0)],
            &b,
        );
        let _ = render(&mut s, 4);
        assert_eq!(
            s.source_count(),
            1,
            "2 was omitted → removed after its ramp-out"
        );
        assert!(
            s.state(1).unwrap().playing,
            "gain 0 keeps the voice alive (mute-not-dispose)"
        );
        assert!(energy(&render(&mut s, 2), 0) < 1e-6);
        s.sync(
            &[SourceDesc::buffer(1, 0, point(1.0, 0.0))
                .looping()
                .with_gain(1.0)],
            &b,
        );
        assert!(
            energy(&render(&mut s, 2), 1) > 0.0,
            "and resumes seamlessly"
        );
    }

    #[test]
    fn oscillator_and_noise_sources_play_until_stopped() {
        let mut s = space();
        s.sync(
            &[
                SourceDesc::oscillator(1, 440.0, Waveform::Sine, point(0.0, -1.0)),
                SourceDesc::noise(2, NoiseColor::Pink, point(0.0, 1.0)),
            ],
            &[],
        );
        let out = render(&mut s, 4);
        assert!(energy(&out, 0) > 1.0 && energy(&out, 1) > 1.0);
        s.stop(1);
        s.stop(2);
        let _ = render(&mut s, 2);
        assert!(energy(&render(&mut s, 2), 0) < 1e-6);
        assert!(!s.state(1).unwrap().playing);
    }

    #[test]
    fn buses_and_space_gain_scale_the_mix() {
        let mut s = space();
        let b = buffers(48000);
        let quiet = s.add_bus(Bus::new(0.25, RATE, BLOCK));
        s.sync(
            &[SourceDesc::buffer(1, 0, point(0.0, -1.0))
                .looping()
                .on_bus(quiet)],
            &b,
        );
        let _ = render(&mut s, 2);
        let on_quiet_bus = energy(&render(&mut s, 4), 0);
        s.sync(&[SourceDesc::buffer(1, 0, point(0.0, -1.0)).looping()], &b);
        let _ = render(&mut s, 2);
        let on_bus_0 = energy(&render(&mut s, 4), 0);
        assert!(
            (on_quiet_bus / on_bus_0 - 0.0625).abs() < 0.01,
            "gain 0.25 → energy 1/16: {}",
            on_quiet_bus / on_bus_0
        );
        s.set_gain(0.5);
        let _ = render(&mut s, 2);
        let half = energy(&render(&mut s, 4), 0);
        assert!((half / on_bus_0 - 0.25).abs() < 0.01);
    }
}
