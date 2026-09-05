//! The game-thread handle (spec §3, §6): the public API. Every call builds what it needs
//! (chains, buses, spaces, snapshots) on this thread and pushes a command through a
//! lock-free queue; displaced state comes back as garbage and is dropped here.

use std::sync::Arc;

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
        DeviceConfig { sample_rate: 0, period_frames: 0, master_gain: 1.0, voice_cap: 0 }
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
    Null { engine: Engine, inbox: rtrb::Consumer<Command> },
    #[cfg(windows)]
    Wasapi { stream: crate::backend::wasapi::WasapiStream, garbage: rtrb::Consumer<Garbage>, finished: rtrb::Consumer<(u32, u64)> },
}

pub struct Device {
    info: StreamInfo,
    commands: rtrb::Producer<Command>,
    finished: Vec<(u32, u64)>,
    spaces: Vec<Option<u32>>, // bus count per space id
    buffers: Vec<bool>,
    backend: Backend,
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
        Device { info, commands, finished: Vec::new(), spaces: Vec::new(), buffers: Vec::new(), backend: Backend::Null { engine, inbox } }
    }

    /// Open the default Windows render endpoint on its own audio thread (spec §2).
    #[cfg(windows)]
    pub fn open_wasapi(cfg: DeviceConfig) -> Result<Device, DeviceError> {
        use crate::backend::wasapi::{WasapiConfig, spawn};
        let (commands, inbox) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (garbage_tx, garbage_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let (finished_tx, finished_rx) = rtrb::RingBuffer::new(QUEUE_CAPACITY);
        let stream = spawn(
            WasapiConfig { period_frames: cfg.period_frames, master_gain: cfg.master_gain, voice_cap: cfg.voice_cap },
            inbox,
            garbage_tx,
            finished_tx,
        )
        .map_err(|e| DeviceError::Backend(e.0))?;
        let info = stream.info;
        Ok(Device {
            info,
            commands,
            finished: Vec::new(),
            spaces: Vec::new(),
            buffers: Vec::new(),
            backend: Backend::Wasapi { stream, garbage: garbage_rx, finished: finished_rx },
        })
    }

    pub fn info(&self) -> StreamInfo {
        self.info
    }

    /// Whether the low-latency (`IAudioClient3`) path is active. Always false for the null backend.
    pub fn low_latency(&self) -> bool {
        match &self.backend {
            Backend::Null { .. } => false,
            #[cfg(windows)]
            Backend::Wasapi { stream, .. } => stream.status().low_latency.load(std::sync::atomic::Ordering::Relaxed),
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
        self.commands.push(cmd).map_err(|_| DeviceError::QueueFull)
    }

    /// Drop whatever the audio thread handed back. Called on every API call; harmless to call more.
    pub fn collect_garbage(&mut self) {
        #[cfg(windows)]
        if let Backend::Wasapi { garbage, .. } = &mut self.backend {
            while garbage.pop().is_ok() {}
        }
    }

    // --- Spaces ---

    pub fn space_create(&mut self, mode: SpaceMode, gain: f32, unit_scale: f32) -> Result<u32, DeviceError> {
        self.space_create_with(mode, gain, unit_scale, Box::new(HeadModel))
    }

    pub fn space_create_with(&mut self, mode: SpaceMode, gain: f32, unit_scale: f32, renderer: Box<dyn Renderer>) -> Result<u32, DeviceError> {
        let id = self.spaces.iter().position(Option::is_none).unwrap_or(self.spaces.len()) as u32;
        let cfg = SpaceConfig { mode, gain, unit_scale: if unit_scale > 0.0 { unit_scale } else { 1.0 } };
        let space = Box::new(Space::new(cfg, self.rate(), self.block(), renderer));
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
        self.spaces.get(space as usize).copied().flatten().ok_or(DeviceError::UnknownSpace)
    }

    pub fn space_gain(&mut self, space: u32, gain: f32) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::SpaceGain { space, gain })
    }

    pub fn set_listener(&mut self, space: u32, position: Vec3, forward: Vec3, up: Vec3) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Listener { space, listener: Listener { position, forward, up } })
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
    pub fn buffer_create(&mut self, samples: &[f32], channels: usize, sample_rate: u32) -> Result<u32, DeviceError> {
        if samples.is_empty() || channels == 0 || sample_rate == 0 {
            return Err(DeviceError::InvalidArgument);
        }
        let mono = to_mono_at_rate(samples, channels, sample_rate, self.info.sample_rate);
        self.buffer_add(mono)
    }

    fn buffer_add(&mut self, mono: Vec<f32>) -> Result<u32, DeviceError> {
        let id = self.buffers.iter().position(|used| !used).unwrap_or(self.buffers.len()) as u32;
        let buffer = Arc::new(Buffer { samples: mono, rate: self.info.sample_rate });
        self.send(Command::BufferAdd { id, buffer })?;
        if self.buffers.len() <= id as usize {
            self.buffers.resize(id as usize + 1, false);
        }
        self.buffers[id as usize] = true;
        Ok(id)
    }

    pub fn buffer_click(&mut self, ms: f32) -> Result<u32, DeviceError> {
        let mono = click(self.info.sample_rate, ms.max(0.1));
        self.buffer_add(mono)
    }

    pub fn buffer_tone(&mut self, hz: f32, ms: f32, attack_ms: f32, release_ms: f32) -> Result<u32, DeviceError> {
        let mono = tone(self.info.sample_rate, hz.max(1.0), ms.max(0.1), attack_ms.max(0.0), release_ms.max(0.0));
        self.buffer_add(mono)
    }

    pub fn buffer_free(&mut self, buffer: u32) -> Result<(), DeviceError> {
        if !self.buffers.get(buffer as usize).copied().unwrap_or(false) {
            return Err(DeviceError::InvalidArgument);
        }
        self.send(Command::BufferFree(buffer))?;
        self.buffers[buffer as usize] = false;
        Ok(())
    }

    // --- Sources ---

    pub fn sync(&mut self, space: u32, descs: &[SourceDesc]) -> Result<(), DeviceError> {
        self.check_space(space)?;
        self.send(Command::Sync { space, descs: descs.to_vec() })
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

    pub fn set_chain(&mut self, target: ChainTarget, descs: &[EffectDesc]) -> Result<(), DeviceError> {
        let channels = match target {
            ChainTarget::Source(s, _, Stage::Pre) => {
                self.check_space(s)?;
                1
            }
            ChainTarget::Source(s, _, Stage::Post) | ChainTarget::Space(s) | ChainTarget::Bus(s, _) => {
                self.check_space(s)?;
                2
            }
            ChainTarget::Master => 2,
        };
        let chain = Box::new(Chain::new(descs, self.rate(), channels, self.block()));
        self.send(Command::SetChain { target, chain })
    }

    pub fn set_effect_param(&mut self, target: ChainTarget, index: usize, param: usize, value: f32) -> Result<(), DeviceError> {
        self.send(Command::SetEffectParam { target, index, param, value })
    }

    pub fn set_effect_enabled(&mut self, target: ChainTarget, index: usize, enabled: bool) -> Result<(), DeviceError> {
        self.send(Command::SetEffectEnabled { target, index, enabled })
    }

    pub fn master_gain(&mut self, gain: f32) -> Result<(), DeviceError> {
        self.send(Command::MasterGain(gain))
    }

    // --- Readback ---

    /// `(space, id)` pairs whose sequences ended since the last call.
    pub fn take_finished(&mut self) -> Vec<(u32, u64)> {
        #[cfg(windows)]
        if let Backend::Wasapi { finished, .. } = &mut self.backend {
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
                drop(engine.apply(cmd));
            }
            engine.process(out);
            self.finished.extend(engine.take_finished());
        }
    }

    /// Null backend only: inspect audio-thread state (tests, harnesses).
    pub fn engine(&self) -> Option<&Engine> {
        match &self.backend {
            Backend::Null { engine, .. } => Some(engine),
            #[cfg(windows)]
            Backend::Wasapi { .. } => None,
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
        d.set_listener(space, Vec3::default(), Vec3::new(0.0, 0.0, -1.0), Vec3::new(0.0, 1.0, 0.0)).unwrap();
        d.sync(space, &[SourceDesc::buffer(1, tone, Bounds::Point(Vec3::new(4.0, 0.0, 0.0))).looping()]).unwrap();
        let out = render(&mut d, 6);
        assert!(energy(&out, 1) > energy(&out, 0) * 2.0, "east → right");

        let walls = d.bus_create(space, 1.0).unwrap();
        d.set_chain(ChainTarget::Bus(space, walls), &[EffectDesc::lowpass(300.0, 0.707)]).unwrap();
        d.sync(space, &[SourceDesc::buffer(1, tone, Bounds::Point(Vec3::new(4.0, 0.0, 0.0))).looping().on_bus(walls)]).unwrap();
        let _ = render(&mut d, 4);
        let filtered = energy(&render(&mut d, 6), 1);
        assert!(filtered < energy(&out, 1) * 0.6, "440 Hz through a 300 Hz lowpass on the bus is quieter");

        d.fire(space, SourceDesc::buffer(77, tone, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))).repeats(1, 0.0)).unwrap();
        let _ = render(&mut d, 120);
        assert!(d.take_finished().contains(&(space, 77)));
        d.master_gain(0.0).unwrap();
        let _ = render(&mut d, 2);
        assert!(energy(&render(&mut d, 2), 0) < 1e-6);
    }

    #[test]
    fn errors_are_reported_not_ignored() {
        let mut d = device();
        assert_eq!(d.sync(9, &[]), Err(DeviceError::UnknownSpace));
        let s = d.space_create(SpaceMode::Spatial, 1.0, 1.0).unwrap();
        assert_eq!(d.bus_gain(s, 3, 1.0), Err(DeviceError::UnknownBus));
        assert_eq!(d.buffer_create(&[], 1, 48000), Err(DeviceError::InvalidArgument));
        assert_eq!(d.buffer_free(5), Err(DeviceError::InvalidArgument));
        let b = d.buffer_click(5.0).unwrap();
        assert_eq!(d.buffer_free(b), Ok(()));
        assert_eq!(d.buffer_click(5.0), Ok(b), "ids are reused");
        let desc = SourceDesc::buffer(1, b, Bounds::Point(Vec3::default())).with_flags(flags::RELATIVE, 0);
        assert_eq!(d.upsert(s, desc), Ok(()));
        d.space_destroy(s).unwrap();
        assert_eq!(d.upsert(s, desc), Err(DeviceError::UnknownSpace));
    }
}
