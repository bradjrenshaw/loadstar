//! The game-thread handle (spec §3, §6): the public API. Every call builds what it needs
//! (chains, buses, spaces, snapshots) on this thread and pushes a command through a
//! lock-free queue; displaced state comes back as garbage and is dropped here.

use std::sync::{Arc, Mutex};

use loadstar_core::geometry::Vec3;

use crate::backend::{NullBackend, StreamInfo};
use crate::chain::{Chain, EffectDesc};
use crate::commands::{ChainTarget, Command, Garbage};
use crate::dsp::{click, resample::to_mono_at_rate, tone};
use crate::engine::{Engine, EngineConfig};
use crate::render::{HeadModel, Renderer};
use crate::space::{Buffer, Bus, Listener, SourceDesc, Space, SpaceConfig, SpaceMode, Stage};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DeviceConfig {
    /// 0 = the device's mix rate.
    pub sample_rate: u32,
    /// 0 = the smallest period the backend offers.
    pub period_frames: usize,
    pub master_gain: f32,
    /// 0 = default (64).
    pub voice_cap: usize,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        DeviceConfig {
            sample_rate: 0,
            period_frames: 0,
            master_gain: 1.0,
            voice_cap: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// The command queue is full; the mixer is not draining (stalled thread or no pumping).
    QueueFull,
    UnknownSpace,
    UnknownBus,
    InvalidArgument,
    /// The backend could not open a device.
    Backend(String),
}

const QUEUE_CAPACITY: usize = 4096;

enum Backend {
    /// The engine lives here; [`Device::render`] pumps it.
    Null {
        engine: Engine,
        inbox: rtrb::Consumer<Command>,
    },
    #[cfg(windows)]
    Wasapi {
        stream: crate::backend::wasapi::WasapiStream,
        garbage: rtrb::Consumer<Garbage>,
        finished: rtrb::Consumer<(u32, u64)>,
    },
    #[cfg(feature = "miniaudio")]
    Miniaudio {
        stream: crate::backend::miniaudio::MiniaudioStream,
        garbage: rtrb::Consumer<Garbage>,
        finished: rtrb::Consumer<(u32, u64)>,
    },
}

pub struct Device {
    info: StreamInfo,
    /// The command producer, shared with in-process intakes (speech-spec §6): one uncontended
    /// lock per call on this thread; the ring underneath stays single-producer.
    commands: Arc<Mutex<rtrb::Producer<Command>>>,
    finished: Vec<(u32, u64)>,
    spaces: Vec<Option<u32>>, // bus count per space id
    /// Buffer id allocation, shared with intakes.
    buffers: Arc<Mutex<Vec<bool>>>,
    /// Voices pre-built per space on this thread (the voice cap), so the audio thread never
    /// allocates until more sources than that are alive at once.
    pool: usize,
    backend: Backend,
}

fn pool_size(cfg: &DeviceConfig) -> usize {
    if cfg.voice_cap == 0 {
        64
    } else {
        cfg.voice_cap
    }
}

/// A second way into the device's command queue for in-process crates whose work completes on
/// their own threads (the speech worker fires rendered utterances). Cloneable and `Send`; every
/// call takes the same lock the game thread's calls take. It never checks space ids (the engine
/// ignores unknown spaces) and never drains garbage — the owning [`Device`] does that.
#[derive(Clone)]
pub struct DeviceIntake {
    info: StreamInfo,
    commands: Arc<Mutex<rtrb::Producer<Command>>>,
    buffers: Arc<Mutex<Vec<bool>>>,
}

impl DeviceIntake {
    pub fn info(&self) -> StreamInfo {
        self.info
    }

    fn send(&self, cmd: Command) -> Result<(), DeviceError> {
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cmd)
            .map_err(|_| DeviceError::QueueFull)
    }

    /// Upload mono samples already at the mixer rate (the caller resampled on its own thread).
    pub fn buffer_create_mono(&self, mono: Vec<f32>) -> Result<u32, DeviceError> {
        if mono.is_empty() {
            return Err(DeviceError::InvalidArgument);
        }
        let id = {
            let mut used = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
            let id = used.iter().position(|u| !u).unwrap_or(used.len());
            if used.len() <= id {
                used.resize(id + 1, false);
            }
            used[id] = true;
            id as u32
        };
        let buffer = Arc::new(Buffer {
            samples: mono,
            rate: self.info.sample_rate,
        });
        match self.send(Command::BufferAdd { id, buffer }) {
            Ok(()) => Ok(id),
            Err(e) => {
                self.buffers.lock().unwrap_or_else(|e| e.into_inner())[id as usize] = false;
                Err(e)
            }
        }
    }

    /// Upload PCM at any rate/channel count (resampled here, on the calling thread).
    pub fn buffer_create(
        &self,
        samples: &[f32],
        channels: usize,
        sample_rate: u32,
    ) -> Result<u32, DeviceError> {
        if samples.is_empty() || channels == 0 || sample_rate == 0 {
            return Err(DeviceError::InvalidArgument);
        }
        self.buffer_create_mono(to_mono_at_rate(
            samples,
            channels,
            sample_rate,
            self.info.sample_rate,
        ))
    }

    pub fn buffer_free(&self, buffer: u32) -> Result<(), DeviceError> {
        {
            let mut used = self.buffers.lock().unwrap_or_else(|e| e.into_inner());
            if !used.get(buffer as usize).copied().unwrap_or(false) {
                return Err(DeviceError::InvalidArgument);
            }
            used[buffer as usize] = false;
        }
        self.send(Command::BufferFree(buffer))
    }

    pub fn fire(&self, space: u32, desc: SourceDesc) -> Result<(), DeviceError> {
        self.send(Command::Fire { space, desc })
    }

    pub fn upsert(&self, space: u32, desc: SourceDesc) -> Result<(), DeviceError> {
        self.send(Command::Upsert { space, desc })
    }

    pub fn stop(&self, space: u32, id: u64) -> Result<(), DeviceError> {
        self.send(Command::Stop { space, id })
    }

    pub fn remove(&self, space: u32, id: u64) -> Result<(), DeviceError> {
        self.send(Command::Remove { space, id })
    }
}

impl Device {
    /// Open with the null backend: no hardware, periods rendered on demand via [`Device::render`].
    pub fn open_null(cfg: DeviceConfig, backend: NullBackend) -> Device {
        let info = backend.info;
        let (commands, inbox) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let engine = Engine::new(EngineConfig {
            sample_rate: info.sample_rate,
            block_frames: info.period_frames,
            master_gain: cfg.master_gain,
            voice_cap: cfg.voice_cap,
        });
        Device {
            info,
            commands: Arc::new(Mutex::new(commands)),
            finished: Vec::new(),
            spaces: Vec::new(),
            buffers: Arc::default(),
            pool: pool_size(&cfg),
            backend: Backend::Null { engine, inbox },
        }
    }

    /// Open the default Windows render endpoint on its own audio thread (spec §2).
    #[cfg(windows)]
    pub fn open_wasapi(cfg: DeviceConfig) -> Result<Device, DeviceError> {
        use crate::backend::wasapi::{WasapiConfig, spawn};
        let (commands, inbox) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (finished_tx, finished_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let stream = spawn(
            WasapiConfig {
                period_frames: cfg.period_frames,
                master_gain: cfg.master_gain,
                voice_cap: cfg.voice_cap,
            },
            inbox,
            garbage_tx,
            finished_tx,
        )
        .map_err(|e| DeviceError::Backend(e.0))?;
        let info = stream.info;
        Ok(Device {
            info,
            commands: Arc::new(Mutex::new(commands)),
            finished: Vec::new(),
            spaces: Vec::new(),
            buffers: Arc::default(),
            pool: pool_size(&cfg),
            backend: Backend::Wasapi {
                stream,
                garbage: garbage_rx,
                finished: finished_rx,
            },
        })
    }

    /// Open the default output through miniaudio (WASAPI → DirectSound → WinMM on Windows;
    /// CoreAudio, ALSA, PulseAudio, JACK elsewhere). Handles device changes itself.
    #[cfg(feature = "miniaudio")]
    pub fn open_miniaudio(cfg: DeviceConfig) -> Result<Device, DeviceError> {
        use crate::backend::miniaudio::{MiniaudioConfig, open};
        let (commands, inbox) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (finished_tx, finished_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let stream = open(
            MiniaudioConfig {
                sample_rate: cfg.sample_rate,
                period_frames: cfg.period_frames,
                master_gain: cfg.master_gain,
                voice_cap: cfg.voice_cap,
            },
            inbox,
            garbage_tx,
            finished_tx,
        )
        .map_err(|e| DeviceError::Backend(e.0))?;
        let info = stream.info;
        Ok(Device {
            info,
            commands: Arc::new(Mutex::new(commands)),
            finished: Vec::new(),
            spaces: Vec::new(),
            buffers: Arc::default(),
            pool: pool_size(&cfg),
            backend: Backend::Miniaudio {
                stream,
                garbage: garbage_rx,
                finished: finished_rx,
            },
        })
    }

    /// The recommended way to open real hardware: miniaudio when compiled in (broadest device
    /// support), otherwise the direct WASAPI backend on Windows.
    pub fn open_default(cfg: DeviceConfig) -> Result<Device, DeviceError> {
        #[cfg(feature = "miniaudio")]
        {
            Device::open_miniaudio(cfg)
        }
        #[cfg(all(not(feature = "miniaudio"), windows))]
        {
            Device::open_wasapi(cfg)
        }
        #[cfg(all(not(feature = "miniaudio"), not(windows)))]
        {
            let _ = cfg;
            Err(DeviceError::Backend(
                "no audio backend compiled in for this platform".into(),
            ))
        }
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    /// Which device layer is driving the stream.
    pub fn backend_name(&self) -> &str {
        match &self.backend {
            Backend::Null { .. } => "null",
            #[cfg(windows)]
            Backend::Wasapi { .. } => "wasapi (direct)",
            #[cfg(feature = "miniaudio")]
            Backend::Miniaudio { stream, .. } => &stream.backend_name,
        }
    }

    /// Whether the low-latency (`IAudioClient3`) path is active. Always false for the null backend.
    pub fn low_latency(&self) -> bool {
        match &self.backend {
            Backend::Null { .. } => false,
            #[cfg(windows)]
            Backend::Wasapi { stream, .. } => stream
                .status()
                .low_latency
                .load(std::sync::atomic::Ordering::Relaxed),
            #[cfg(feature = "miniaudio")]
            Backend::Miniaudio { stream, .. } => stream
                .status()
                .low_latency
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    fn rate(&self) -> f32 {
        self.info.sample_rate as f32
    }

    fn block(&self) -> usize {
        self.info.period_frames
    }

    fn send(&mut self, cmd: Command) -> Result<(), DeviceError> {
        self.collect_garbage();
        self.commands
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(cmd)
            .map_err(|_| DeviceError::QueueFull)
    }

    /// A handle other in-process threads use to upload and fire (speech-spec §6).
    pub fn intake(&self) -> DeviceIntake {
        DeviceIntake {
            info: self.info,
            commands: Arc::clone(&self.commands),
            buffers: Arc::clone(&self.buffers),
        }
    }

    /// Drop whatever the audio thread handed back. Called on every API call; harmless to call more.
    pub fn collect_garbage(&mut self) {
        #[cfg(windows)]
        if let Backend::Wasapi { garbage, .. } = &mut self.backend {
            while garbage.pop().is_ok() {}
        }
        #[cfg(feature = "miniaudio")]
        if let Backend::Miniaudio { garbage, .. } = &mut self.backend {
            while garbage.pop().is_ok() {}
        }
    }

    // --- Spaces ---

    pub fn space_create(
        &mut self,
        mode: SpaceMode,
        gain: f32,
        unit_scale: f32,
    ) -> Result<u32, DeviceError> {
        self.space_create_with(mode, gain, unit_scale, Box::new(HeadModel))
    }

    pub fn space_create_with(
        &mut self,
        mode: SpaceMode,
        gain: f32,
        unit_scale: f32,
        renderer: Box<dyn Renderer>,
    ) -> Result<u32, DeviceError> {
        let id = self
            .spaces
            .iter()
            .position(Option::is_none)
            .unwrap_or(self.spaces.len()) as u32;
        let cfg = SpaceConfig {
            mode,
            gain,
            unit_scale: if unit_scale > 0.0 { unit_scale } else { 1.0 },
        };
        let space = Box::new(Space::with_pool(
            cfg,
            self.rate(),
            self.block(),
            renderer,
            self.pool,
        ));
        self.send(Command::SpaceCreate { id, space })?;
        if self.spaces.len() <= id as usize {
            self.spaces.resize(id as usize + 1, None);
        }
        self.spaces[id as usize] = Some(1);
        Ok(id)
    }

    pub fn space_destroy(&mut self, space: u32) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::SpaceDestroy(space))?;
        self.spaces[space as usize] = None;
        Ok(())
    }

    fn check_space(&self, space: u32) -> Result<u32, DeviceError> {
        self.spaces
            .get(space as usize)
            .copied()
            .flatten()
            .ok_or(DeviceError::UnknownSpace)
    }

    pub fn space_gain(&mut self, space: u32, gain: f32) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::SpaceGain { space, gain })
    }

    pub fn set_listener(
        &mut self,
        space: u32,
        position: Vec3,
        forward: Vec3,
        up: Vec3,
    ) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Listener {
            space,
            listener: Listener {
                position,
                forward,
                up,
            },
        })
    }

    /// Per-frame hook for renderers that simulate. The head model does nothing here.
    pub fn space_update(&mut self, space: u32, _dt: f32) -> Result<(), DeviceError> {
        self.check_space(space).map(|_| ())
    }

    // --- Buses ---

    pub fn bus_create(&mut self, space: u32, gain: f32) -> Result<u32, DeviceError> {
        let count = self.check_space(space)?;
        let bus = Box::new(Bus::new(gain, self.rate(), self.block()));
        self.send(Command::BusCreate { space, bus })?;
        self.spaces[space as usize] = Some(count + 1);
        Ok(count)
    }

    pub fn bus_gain(&mut self, space: u32, bus: u32, gain: f32) -> Result<(), DeviceError> {
        if bus >= self.check_space(space)? {
            return Err(DeviceError::UnknownBus);
        }
        self.send(Command::BusGain { space, bus, gain })
    }

    // --- Buffers ---

    /// Upload PCM (interleaved `channels` at `sample_rate`); converted to mono at the mixer rate.
    pub fn buffer_create(
        &mut self,
        samples: &[f32],
        channels: usize,
        sample_rate: u32,
    ) -> Result<u32, DeviceError> {
        if samples.is_empty() || channels == 0 || sample_rate == 0 {
            return Err(DeviceError::InvalidArgument);
        }
        let mono = to_mono_at_rate(samples, channels, sample_rate, self.info.sample_rate);
        self.buffer_add(mono)
    }

    fn buffer_add(&mut self, mono: Vec<f32>) -> Result<u32, DeviceError> {
        self.collect_garbage();
        self.intake().buffer_create_mono(mono)
    }

    pub fn buffer_click(&mut self, ms: f32) -> Result<u32, DeviceError> {
        let mono = click(self.info.sample_rate, ms.max(0.1));
        self.buffer_add(mono)
    }

    pub fn buffer_tone(
        &mut self,
        hz: f32,
        ms: f32,
        attack_ms: f32,
        release_ms: f32,
    ) -> Result<u32, DeviceError> {
        let mono = tone(
            self.info.sample_rate,
            hz.max(1.0),
            ms.max(0.1),
            attack_ms.max(0.0),
            release_ms.max(0.0),
        );
        self.buffer_add(mono)
    }

    pub fn buffer_free(&mut self, buffer: u32) -> Result<(), DeviceError> {
        self.collect_garbage();
        self.intake().buffer_free(buffer)
    }

    // --- Sources ---

    pub fn sync(&mut self, space: u32, descs: &[SourceDesc]) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Sync {
            space,
            descs: descs.to_vec(),
        })
    }

    pub fn upsert(&mut self, space: u32, desc: SourceDesc) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Upsert { space, desc })
    }

    pub fn remove(&mut self, space: u32, id: u64) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Remove { space, id })
    }

    pub fn fire(&mut self, space: u32, desc: SourceDesc) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Fire { space, desc })
    }

    pub fn stop(&mut self, space: u32, id: u64) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Stop { space, id })
    }

    // --- Effects ---

    pub fn set_chain(
        &mut self,
        target: ChainTarget,
        descs: &[EffectDesc],
    ) -> Result<(), DeviceError> {
        let channels = match target {
            ChainTarget::Source(s, _, Stage::Pre) => {
                self.check_space(s)?;
                1
            }
            ChainTarget::Source(s, _, Stage::Post)
            | ChainTarget::Space(s)
            | ChainTarget::Bus(s, _) => {
                self.check_space(s)?;
                2
            }
            ChainTarget::Master => 2,
        };
        let chain = Box::new(Chain::new(descs, self.rate(), channels, self.block()));
        self.send(Command::SetChain { target, chain })
    }

    pub fn set_effect_param(
        &mut self,
        target: ChainTarget,
        index: usize,
        param: usize,
        value: f32,
    ) -> Result<(), DeviceError> {
        self.send(Command::SetEffectParam {
            target,
            index,
            param,
            value,
        })
    }

    pub fn set_effect_enabled(
        &mut self,
        target: ChainTarget,
        index: usize,
        enabled: bool,
    ) -> Result<(), DeviceError> {
        self.send(Command::SetEffectEnabled {
            target,
            index,
            enabled,
        })
    }

    pub fn master_gain(&mut self, gain: f32) -> Result<(), DeviceError> {
        self.send(Command::MasterGain(gain))
    }

    // --- Readback ---

    /// Hand back items taken with [`Device::take_finished`] that the caller could not accept
    /// (a too-small buffer); they are reported again on the next call, in order.
    pub fn return_finished(&mut self, mut items: Vec<(u32, u64)>) {
        items.append(&mut self.finished);
        self.finished = items;
    }

    /// `(space, id)` pairs whose sequences ended since the last call.
    pub fn take_finished(&mut self) -> Vec<(u32, u64)> {
        #[cfg(windows)]
        if let Backend::Wasapi { finished, .. } = &mut self.backend {
            while let Ok(f) = finished.pop() {
                self.finished.push(f);
            }
        }
        #[cfg(feature = "miniaudio")]
        if let Backend::Miniaudio { finished, .. } = &mut self.backend {
            while let Ok(f) = finished.pop() {
                self.finished.push(f);
            }
        }
        std::mem::take(&mut self.finished)
    }

    // --- Null backend pumping ---

    /// Apply pending commands and render one period into `out` (interleaved stereo).
    /// Null backend only; a real backend does this on its own thread (this is then a no-op).
    pub fn render(&mut self, out: &mut [f32]) {
        if let Backend::Null { engine, inbox } = &mut self.backend {
            while let Ok(cmd) = inbox.pop() {
                drop(engine.apply(cmd)); // this IS the game thread: dropping here is the point
            }
            engine.process(out);
            engine.drain_garbage(|g| {
                drop(g);
                Ok(())
            });
            let finished = &mut self.finished;
            engine.drain_finished(|f| finished.push(f));
        }
    }

    /// Null backend only: inspect audio-thread state (tests, harnesses).
    pub fn engine(&self) -> Option<&Engine> {
        match &self.backend {
            Backend::Null { engine, .. } => Some(engine),
            #[cfg(windows)]
            Backend::Wasapi { .. } => None,
            #[cfg(feature = "miniaudio")]
            Backend::Miniaudio { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::space::flags;
    use loadstar_core::geometry::Bounds;

    fn device() -> Device {
        Device::open_null(DeviceConfig::default(), NullBackend::new(48000, 256))
    }

    fn energy(buf: &[f32], channel: usize) -> f32 {
        buf.iter().skip(channel).step_by(2).map(|s| s * s).sum()
    }

    fn render(d: &mut Device, blocks: usize) -> Vec<f32> {
        let mut all = Vec::new();
        for _ in 0..blocks {
            let mut out = vec![0.0; 512];
            d.render(&mut out);
            all.extend(out);
        }
        all
    }

    #[test]
    fn end_to_end_through_the_command_queue() {
        let mut d = device();
        let space = d.space_create(SpaceMode::Planar, 1.0, 1.0).unwrap();
        let tone = d.buffer_tone(440.0, 500.0, 5.0, 5.0).unwrap();
        d.set_listener(
            space,
            Vec3::default(),
            Vec3::new(0.0, 0.0, -1.0),
            Vec3::new(0.0, 1.0, 0.0),
        )
        .unwrap();
        d.sync(
            space,
            &[SourceDesc::buffer(1, tone, Bounds::Point(Vec3::new(4.0, 0.0, 0.0))).looping()],
        )
        .unwrap();
        let out = render(&mut d, 6);
        assert!(energy(&out, 1) > energy(&out, 0) * 2.0, "east → right");

        let walls = d.bus_create(space, 1.0).unwrap();
        d.set_chain(
            ChainTarget::Bus(space, walls),
            &[EffectDesc::lowpass(300.0, 0.707)],
        )
        .unwrap();
        d.sync(
            space,
            &[
                SourceDesc::buffer(1, tone, Bounds::Point(Vec3::new(4.0, 0.0, 0.0)))
                    .looping()
                    .on_bus(walls),
            ],
        )
        .unwrap();
        let _ = render(&mut d, 4);
        let filtered = energy(&render(&mut d, 6), 1);
        assert!(
            filtered < energy(&out, 1) * 0.6,
            "440 Hz through a 300 Hz lowpass on the bus is quieter"
        );

        d.fire(
            space,
            SourceDesc::buffer(77, tone, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))).repeats(1, 0.0),
        )
        .unwrap();
        let _ = render(&mut d, 120);
        assert!(d.take_finished().contains(&(space, 77)));
        d.master_gain(0.0).unwrap();
        let _ = render(&mut d, 2);
        assert!(energy(&render(&mut d, 2), 0) < 1e-6);
    }

    #[test]
    fn an_intake_uploads_and_fires_from_another_thread() {
        let mut d = device();
        let space = d.space_create(SpaceMode::Planar, 1.0, 1.0).unwrap();
        let intake = d.intake();
        let worker = std::thread::spawn(move || {
            let samples: Vec<f32> = (0..4800).map(|i| ((i as f32) * 0.05).sin() * 0.5).collect();
            let buf = intake.buffer_create(&samples, 1, 48000).unwrap();
            intake
                .fire(
                    space,
                    SourceDesc::buffer(42, buf, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))),
                )
                .unwrap();
            intake.buffer_free(buf).unwrap(); // the voice keeps its own reference
            buf
        });
        let buf = worker.join().unwrap();
        let out = render(&mut d, 4);
        assert!(
            energy(&out, 0) > 0.0,
            "fired from the worker thread, audible on the game thread's device"
        );
        assert_eq!(
            d.buffer_click(5.0),
            Ok(buf),
            "the id was released for reuse"
        );
        let _ = render(&mut d, 40);
        assert!(d.take_finished().contains(&(space, 42)));
    }

    #[test]
    fn errors_are_reported_not_ignored() {
        let mut d = device();
        assert_eq!(d.sync(9, &[]), Err(DeviceError::UnknownSpace));
        let s = d.space_create(SpaceMode::Spatial, 1.0, 1.0).unwrap();
        assert_eq!(d.bus_gain(s, 3, 1.0), Err(DeviceError::UnknownBus));
        assert_eq!(
            d.buffer_create(&[], 1, 48000),
            Err(DeviceError::InvalidArgument)
        );
        assert_eq!(d.buffer_free(5), Err(DeviceError::InvalidArgument));
        let b = d.buffer_click(5.0).unwrap();
        assert_eq!(d.buffer_free(b), Ok(()));
        assert_eq!(d.buffer_click(5.0), Ok(b), "ids are reused");
        let desc =
            SourceDesc::buffer(1, b, Bounds::Point(Vec3::default())).with_flags(flags::RELATIVE, 0);
        assert_eq!(d.upsert(s, desc), Ok(()));
        d.space_destroy(s).unwrap();
        assert_eq!(d.upsert(s, desc), Err(DeviceError::UnknownSpace));
    }
}
