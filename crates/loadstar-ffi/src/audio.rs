//! The audio ABI (`la_*`, see `docs/audio-spec.md`). Same rules as the navigation surface:
//! flat data, status codes, opaque handles, no callbacks, no strings, every entry guarded.

use loadstar_audio::backend::NullBackend;
use loadstar_audio::chain::{EffectDesc, EffectKind};
use loadstar_audio::commands::ChainTarget;
use loadstar_audio::device::{Device, DeviceConfig, DeviceError};
use loadstar_audio::space::{SourceDesc, SourceKind, SpaceMode, Stage};

use crate::{BoundsDesc, Status, Vec3, guard};

/// Opaque: an open audio device and everything under it. Exported to C as `LsAudioDevice`.
pub struct AudioDevice(pub(crate) Device);

/// Mirrors `SpaceMode`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSpaceMode {
    Planar = 0,
    Spatial = 1,
}

/// `LaSourceDesc.kind`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceKind {
    Buffer = 0,
    Oscillator = 1,
    Noise = 2,
    /// Plays from a stream (`buffer` = the stream id from `la_stream_create`).
    Stream = 3,
}

/// `LaSourceDesc.flags` bits.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioSourceFlags {
    None = 0,
    Playing = 1,
    Restart = 2,
    Relative = 4,
    AutoRemove = 8,
}

/// `LaEffectDesc.kind` (parameters per kind are documented in `loadstar_audio::chain::EffectDesc`).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioEffectKind {
    Gain = 0,
    LowPass = 1,
    HighPass = 2,
    LowShelf = 3,
    HighShelf = 4,
    Peak = 5,
    Delay = 6,
    Limiter = 7,
    Distance = 8,
    Spatializer = 9,
}

/// `LaChainTarget.kind`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioChainKind {
    Master = 0,
    Space = 1,
    Bus = 2,
    SourcePre = 3,
    SourcePost = 4,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioDeviceConfig {
    /// 0 = the device's mix rate.
    pub sample_rate: u32,
    /// 0 = the smallest period the driver offers.
    pub period_frames: u32,
    pub master_gain: f32,
    /// 0 = default (64).
    pub voice_cap: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct AudioStreamInfo {
    pub sample_rate: u32,
    pub period_frames: u32,
    pub channels: u32,
    /// Nonzero when the `IAudioClient3` low-latency path is active.
    pub low_latency: u32,
}

/// A source record (audio spec §3), 96 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioSourceDesc {
    /// Mod-assigned; 0 = anonymous (fired sources only).
    pub id: u64,
    /// `AudioSourceKind`.
    pub kind: u8,
    /// Oscillator waveform (0 sine, 1 triangle, 2 square, 3 saw) or noise colour (0 white, 1 pink, 2 brown).
    pub waveform: u8,
    pub priority: u8,
    pub _reserved0: u8,
    /// Plays per trigger; 0 = forever.
    pub repeat_count: u16,
    pub _reserved1: u16,
    pub buffer: u32,
    pub bus: u32,
    /// Bitwise OR of `AudioSourceFlags`.
    pub flags: u32,
    pub bounds: BoundsDesc,
    pub gain: f32,
    /// Buffer: playback rate. Oscillator: frequency in Hz.
    pub pitch: f32,
    /// Lateral crossover in the space's units; for RELATIVE sources, the pan (−1..1).
    pub pan_width: f32,
    pub repeat_gap_ms: f32,
    pub _reserved2: [u32; 3],
}

impl AudioSourceDesc {
    pub(crate) fn to_core(&self) -> Option<SourceDesc> {
        Some(SourceDesc {
            id: self.id,
            kind: SourceKind::from_u8(self.kind),
            waveform: self.waveform,
            priority: self.priority,
            repeat_count: self.repeat_count,
            buffer: self.buffer,
            bus: self.bus,
            flags: self.flags,
            bounds: self.bounds.to_core()?,
            gain: self.gain,
            pitch: self.pitch,
            pan_width: self.pan_width,
            repeat_gap_ms: self.repeat_gap_ms,
        })
    }
}

/// An effect node (audio spec §4.2), 40 bytes. `flags` bit 0 = enabled; renderer cue bits above.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioEffectDesc {
    /// `AudioEffectKind`.
    pub kind: u32,
    pub flags: u32,
    pub p: [f32; 8],
}

impl AudioEffectDesc {
    fn to_core(&self) -> Option<EffectDesc> {
        Some(EffectDesc {
            kind: EffectKind::from_u32(self.kind)?,
            flags: self.flags,
            p: self.p,
        })
    }
}

/// Which chain an effect operation addresses, 24 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct AudioChainTarget {
    /// `AudioChainKind`.
    pub kind: u32,
    pub space: u32,
    pub bus: u32,
    pub _reserved: u32,
    pub source_id: u64,
}

impl AudioChainTarget {
    fn to_core(&self) -> Option<ChainTarget> {
        Some(match self.kind {
            0 => ChainTarget::Master,
            1 => ChainTarget::Space(self.space),
            2 => ChainTarget::Bus(self.space, self.bus),
            3 => ChainTarget::Source(self.space, self.source_id, Stage::Pre),
            4 => ChainTarget::Source(self.space, self.source_id, Stage::Post),
            _ => return None,
        })
    }
}

/// A finished one-shot: `(space, source id)`, 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioFinished {
    pub space: u32,
    pub _reserved: u32,
    pub id: u64,
}

fn status(r: Result<(), DeviceError>) -> Status {
    match r {
        Ok(()) => Status::Ok,
        Err(DeviceError::QueueFull) => Status::Busy,
        Err(DeviceError::Backend(_)) => Status::DeviceUnavailable,
        Err(_) => Status::InvalidArgument,
    }
}

fn with_device(dev: *mut AudioDevice, f: impl FnOnce(&mut Device) -> Status) -> Status {
    guard(|| {
        if dev.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live device handle from `la_device_open*`, used from one thread.
        f(&mut unsafe { &mut *dev }.0)
    })
}

/// Write a `u32` result through an out-pointer.
fn out_u32(out: *mut u32, r: Result<u32, DeviceError>) -> Status {
    match r {
        // SAFETY: callers check `out` for null; caller guarantees it is writable.
        Ok(v) => unsafe {
            *out = v;
            Status::Ok
        },
        Err(e) => status(Err(e)),
    }
}

fn config(c: AudioDeviceConfig) -> DeviceConfig {
    DeviceConfig {
        sample_rate: c.sample_rate,
        period_frames: c.period_frames as usize,
        master_gain: if c.master_gain > 0.0 {
            c.master_gain
        } else {
            1.0
        },
        voice_cap: c.voice_cap as usize,
    }
}

// ---------------------------------------------------------------------------
// Device
// ---------------------------------------------------------------------------

/// Open the default audio output (WASAPI shared mode on Windows). `DeviceUnavailable` if none.
#[unsafe(no_mangle)]
pub extern "C" fn la_device_open(cfg: AudioDeviceConfig, out: *mut *mut AudioDevice) -> Status {
    guard(|| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let opened = Device::open_default(config(cfg));
        match opened {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Ok(d) => unsafe {
                *out = Box::into_raw(Box::new(AudioDevice(d)));
                Status::Ok
            },
            Err(e) => status(Err(e)),
        }
    })
}

/// Open a device with no hardware: periods are rendered on demand with [`la_device_render`].
/// For tests and harnesses.
#[unsafe(no_mangle)]
pub extern "C" fn la_device_open_null(
    cfg: AudioDeviceConfig,
    sample_rate: u32,
    period_frames: u32,
    out: *mut *mut AudioDevice,
) -> Status {
    guard(|| {
        if out.is_null() {
            return Status::NullPointer;
        }
        if sample_rate == 0 || period_frames == 0 {
            return Status::InvalidArgument;
        }
        let d = Device::open_null(
            config(cfg),
            NullBackend::new(sample_rate, period_frames as usize),
        );
        // SAFETY: `out` non-null checked; caller guarantees it is writable.
        unsafe { *out = Box::into_raw(Box::new(AudioDevice(d))) };
        Status::Ok
    })
}

/// Close a device, stopping its audio thread and releasing every space and buffer. Null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn la_device_close(dev: *mut AudioDevice) {
    let _ = guard(|| {
        if !dev.is_null() {
            // SAFETY: `dev` came from `Box::into_raw` in an open function; the caller won't reuse it.
            drop(unsafe { Box::from_raw(dev) });
        }
        Status::Ok
    });
}

/// The format the backend actually opened.
#[unsafe(no_mangle)]
pub extern "C" fn la_device_info(dev: *mut AudioDevice, out: *mut AudioStreamInfo) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let info = d.info();
        // SAFETY: `out` non-null checked; caller guarantees it is writable.
        unsafe {
            *out = AudioStreamInfo {
                sample_rate: info.sample_rate,
                period_frames: info.period_frames as u32,
                channels: info.channels as u32,
                low_latency: d.low_latency() as u32,
            }
        };
        Status::Ok
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_device_master_gain(dev: *mut AudioDevice, gain: f32) -> Status {
    with_device(dev, |d| status(d.master_gain(gain)))
}

/// Null devices only: apply pending commands and render `frames` frames of interleaved
/// stereo into `out` (`2 * frames` floats). A no-op on hardware devices.
#[unsafe(no_mangle)]
pub extern "C" fn la_device_render(dev: *mut AudioDevice, out: *mut f32, frames: usize) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `out` holds `2 * frames` writable floats.
        let buf = unsafe { std::slice::from_raw_parts_mut(out, frames * 2) };
        d.render(buf);
        Status::Ok
    })
}

/// Drain the `(space, id)` pairs whose sequences ended since the last call. Buffer convention
/// as `ls_path_nodes`: `*written` receives the count; `BufferTooSmall` if it exceeds `capacity`
/// (nothing is consumed in that case).
#[unsafe(no_mangle)]
pub extern "C" fn la_device_finished(
    dev: *mut AudioDevice,
    out: *mut AudioFinished,
    capacity: usize,
    written: *mut usize,
) -> Status {
    with_device(dev, |d| {
        if written.is_null() {
            return Status::NullPointer;
        }
        let items = d.take_finished();
        // SAFETY: `written` non-null checked; caller guarantees it is writable.
        unsafe { *written = items.len() };
        if items.len() > capacity {
            d.return_finished(items); // nothing consumed: the caller comes back with room
            return Status::BufferTooSmall;
        }
        if items.is_empty() {
            return Status::Ok;
        }
        if out.is_null() {
            return Status::NullPointer;
        }
        for (i, (space, id)) in items.iter().enumerate() {
            // SAFETY: `out` non-null and the caller guarantees `capacity >= items.len()` slots.
            unsafe {
                *out.add(i) = AudioFinished {
                    space: *space,
                    _reserved: 0,
                    id: *id,
                }
            };
        }
        Status::Ok
    })
}

// ---------------------------------------------------------------------------
// Spaces, listener, buses
// ---------------------------------------------------------------------------

/// Create a space. `mode` is an `AudioSpaceMode`; `unit_scale` = units per metre (0 = 1).
#[unsafe(no_mangle)]
pub extern "C" fn la_space_create(
    dev: *mut AudioDevice,
    mode: u32,
    gain: f32,
    unit_scale: f32,
    out: *mut u32,
) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let mode = match mode {
            0 => SpaceMode::Planar,
            1 => SpaceMode::Spatial,
            _ => return Status::InvalidArgument,
        };
        out_u32(out, d.space_create(mode, gain, unit_scale))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_space_destroy(dev: *mut AudioDevice, space: u32) -> Status {
    with_device(dev, |d| status(d.space_destroy(space)))
}

#[unsafe(no_mangle)]
pub extern "C" fn la_space_gain(dev: *mut AudioDevice, space: u32, gain: f32) -> Status {
    with_device(dev, |d| status(d.space_gain(space, gain)))
}

/// Listener pose for a space, each frame.
#[unsafe(no_mangle)]
pub extern "C" fn la_listener_set(
    dev: *mut AudioDevice,
    space: u32,
    position: Vec3,
    forward: Vec3,
    up: Vec3,
) -> Status {
    with_device(dev, |d| {
        status(d.set_listener(space, position, forward, up))
    })
}

/// Per-frame hook for simulating renderers (audio spec §3). A no-op for the head model.
#[unsafe(no_mangle)]
pub extern "C" fn la_space_update(dev: *mut AudioDevice, space: u32, dt: f32) -> Status {
    with_device(dev, |d| status(d.space_update(space, dt)))
}

#[unsafe(no_mangle)]
pub extern "C" fn la_bus_create(
    dev: *mut AudioDevice,
    space: u32,
    gain: f32,
    out: *mut u32,
) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        out_u32(out, d.bus_create(space, gain))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_bus_gain(dev: *mut AudioDevice, space: u32, bus: u32, gain: f32) -> Status {
    with_device(dev, |d| status(d.bus_gain(space, bus, gain)))
}

// ---------------------------------------------------------------------------
// Buffers
// ---------------------------------------------------------------------------

/// Upload interleaved PCM; converted to mono at the mixer rate (copied — the caller keeps ownership).
#[unsafe(no_mangle)]
pub extern "C" fn la_buffer_create(
    dev: *mut AudioDevice,
    samples: *const f32,
    samples_len: usize,
    channels: u32,
    sample_rate: u32,
    out: *mut u32,
) -> Status {
    with_device(dev, |d| {
        if out.is_null() || samples.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `samples` is readable for `samples_len` floats.
        let pcm = unsafe { std::slice::from_raw_parts(samples, samples_len) };
        out_u32(out, d.buffer_create(pcm, channels as usize, sample_rate))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_buffer_click(dev: *mut AudioDevice, ms: f32, out: *mut u32) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        out_u32(out, d.buffer_click(ms))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_buffer_tone(
    dev: *mut AudioDevice,
    hz: f32,
    ms: f32,
    attack_ms: f32,
    release_ms: f32,
    out: *mut u32,
) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        out_u32(out, d.buffer_tone(hz, ms, attack_ms, release_ms))
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_buffer_free(dev: *mut AudioDevice, buffer: u32) -> Status {
    with_device(dev, |d| status(d.buffer_free(buffer)))
}

// ---------------------------------------------------------------------------
// Streams
// ---------------------------------------------------------------------------

/// Create a streaming source (audio spec §3): a ring of `capacity` mono frames at the mixer rate
/// that starts playing once `prefill` frames are in. Reference it from a source record with
/// `kind = Stream` and `buffer = id`; feed it with `la_stream_write`.
#[unsafe(no_mangle)]
pub extern "C" fn la_stream_create(
    dev: *mut AudioDevice,
    capacity: usize,
    prefill: usize,
    out: *mut u32,
) -> Status {
    with_device(dev, |d| {
        if out.is_null() {
            return Status::NullPointer;
        }
        out_u32(out, d.stream_create(capacity, prefill))
    })
}

/// Write mono samples at the mixer rate; `written` receives how many were taken (the rest did not
/// fit — come back next frame).
#[unsafe(no_mangle)]
pub extern "C" fn la_stream_write(
    dev: *mut AudioDevice,
    stream: u32,
    samples: *const f32,
    samples_len: usize,
    written: *mut usize,
) -> Status {
    with_device(dev, |d| {
        if samples.is_null() || written.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `samples` is readable for `samples_len` floats.
        let pcm = unsafe { std::slice::from_raw_parts(samples, samples_len) };
        match d.stream_write(stream, pcm) {
            Ok(n) => {
                // SAFETY: non-null checked.
                unsafe { *written = n };
                Status::Ok
            }
            Err(e) => status(Err(e)),
        }
    })
}

/// Frames written and played so far, and whether the producer has finished.
#[unsafe(no_mangle)]
pub extern "C" fn la_stream_progress(
    dev: *mut AudioDevice,
    stream: u32,
    written: *mut u64,
    played: *mut u64,
    finished: *mut bool,
) -> Status {
    with_device(dev, |d| match d.stream_progress(stream) {
        Ok((w, p, done)) => {
            if written.is_null() || played.is_null() || finished.is_null() {
                return Status::NullPointer;
            }
            // SAFETY: all three out-pointers checked.
            unsafe {
                *written = w;
                *played = p;
                *finished = done;
            }
            Status::Ok
        }
        Err(e) => status(Err(e)),
    })
}

/// No more samples will come: the voice ends once the ring drains.
#[unsafe(no_mangle)]
pub extern "C" fn la_stream_finish(dev: *mut AudioDevice, stream: u32) -> Status {
    with_device(dev, |d| status(d.stream_finish(stream)))
}

/// Release the stream id (after its voice finished).
#[unsafe(no_mangle)]
pub extern "C" fn la_stream_free(dev: *mut AudioDevice, stream: u32) -> Status {
    with_device(dev, |d| status(d.stream_free(stream)))
}

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

fn read_descs(descs: *const AudioSourceDesc, len: usize) -> Option<Vec<SourceDesc>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if descs.is_null() {
        return None;
    }
    // SAFETY: non-null checked; caller guarantees `descs` is readable for `len` records.
    unsafe { std::slice::from_raw_parts(descs, len) }
        .iter()
        .map(AudioSourceDesc::to_core)
        .collect()
}

/// Declarative snapshot of a space's declared sources (audio spec §3). `InvalidArgument`
/// rejects the whole snapshot on any bad bounds kind.
#[unsafe(no_mangle)]
pub extern "C" fn la_space_sync(
    dev: *mut AudioDevice,
    space: u32,
    descs: *const AudioSourceDesc,
    descs_len: usize,
) -> Status {
    with_device(dev, |d| match read_descs(descs, descs_len) {
        Some(list) => status(d.sync(space, &list)),
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_source_upsert(
    dev: *mut AudioDevice,
    space: u32,
    desc: *const AudioSourceDesc,
) -> Status {
    with_device(dev, |d| {
        if desc.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees a readable record.
        match unsafe { *desc }.to_core() {
            Some(s) => status(d.upsert(space, s)),
            None => Status::InvalidArgument,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_source_remove(dev: *mut AudioDevice, space: u32, id: u64) -> Status {
    with_device(dev, |d| status(d.remove(space, id)))
}

/// Play a sequence and remove the source when it drains (audio spec §3, fired sources).
#[unsafe(no_mangle)]
pub extern "C" fn la_space_fire(
    dev: *mut AudioDevice,
    space: u32,
    desc: *const AudioSourceDesc,
) -> Status {
    with_device(dev, |d| {
        if desc.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees a readable record.
        match unsafe { *desc }.to_core() {
            Some(s) => status(d.fire(space, s)),
            None => Status::InvalidArgument,
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn la_source_stop(dev: *mut AudioDevice, space: u32, id: u64) -> Status {
    with_device(dev, |d| status(d.stop(space, id)))
}

// ---------------------------------------------------------------------------
// Effect chains
// ---------------------------------------------------------------------------

/// Replace a chain wholesale (`nodes` may be null when `nodes_len` is 0).
#[unsafe(no_mangle)]
pub extern "C" fn la_chain_set(
    dev: *mut AudioDevice,
    target: AudioChainTarget,
    nodes: *const AudioEffectDesc,
    nodes_len: usize,
) -> Status {
    with_device(dev, |d| {
        let Some(target) = target.to_core() else {
            return Status::InvalidArgument;
        };
        let descs: Option<Vec<EffectDesc>> = if nodes_len == 0 {
            Some(Vec::new())
        } else if nodes.is_null() {
            return Status::NullPointer;
        } else {
            // SAFETY: non-null checked; caller guarantees `nodes` is readable for `nodes_len` records.
            unsafe { std::slice::from_raw_parts(nodes, nodes_len) }
                .iter()
                .map(AudioEffectDesc::to_core)
                .collect()
        };
        match descs {
            Some(list) => status(d.set_chain(target, &list)),
            None => Status::InvalidArgument,
        }
    })
}

/// Retune one parameter of one node in place.
#[unsafe(no_mangle)]
pub extern "C" fn la_effect_set_param(
    dev: *mut AudioDevice,
    target: AudioChainTarget,
    index: u32,
    param: u32,
    value: f32,
) -> Status {
    with_device(dev, |d| match target.to_core() {
        Some(t) => status(d.set_effect_param(t, index as usize, param as usize, value)),
        None => Status::InvalidArgument,
    })
}

/// Enable or disable one node, crossfaded.
#[unsafe(no_mangle)]
pub extern "C" fn la_effect_set_enabled(
    dev: *mut AudioDevice,
    target: AudioChainTarget,
    index: u32,
    enabled: bool,
) -> Status {
    with_device(dev, |d| match target.to_core() {
        Some(t) => status(d.set_effect_enabled(t, index as usize, enabled)),
        None => Status::InvalidArgument,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::BoundsKind;

    #[test]
    fn abi_struct_sizes_are_pinned() {
        assert_eq!(std::mem::size_of::<AudioSourceDesc>(), 96);
        assert_eq!(std::mem::size_of::<AudioEffectDesc>(), 40);
        assert_eq!(std::mem::size_of::<AudioChainTarget>(), 24);
        assert_eq!(std::mem::size_of::<AudioDeviceConfig>(), 16);
        assert_eq!(std::mem::size_of::<AudioStreamInfo>(), 16);
        assert_eq!(std::mem::size_of::<AudioFinished>(), 16);
    }

    fn point(x: f32, z: f32) -> BoundsDesc {
        BoundsDesc {
            kind: BoundsKind::Point as u32,
            layer: 0,
            ref_id: 0,
            _reserved: 0,
            a: Vec3::new(x, 0.0, z),
            b: Vec3::default(),
        }
    }

    fn source(id: u64, buffer: u32, bounds: BoundsDesc) -> AudioSourceDesc {
        AudioSourceDesc {
            id,
            kind: 0,
            waveform: 0,
            priority: 0,
            _reserved0: 0,
            repeat_count: 0,
            _reserved1: 0,
            buffer,
            bus: 0,
            flags: AudioSourceFlags::Playing as u32,
            bounds,
            gain: 1.0,
            pitch: 1.0,
            pan_width: 1.0,
            repeat_gap_ms: 0.0,
            _reserved2: [0; 3],
        }
    }

    #[test]
    fn null_device_through_the_abi() {
        let mut dev = std::ptr::null_mut();
        assert_eq!(
            la_device_open_null(AudioDeviceConfig::default(), 48000, 256, &mut dev),
            Status::Ok
        );
        let mut info = AudioStreamInfo::default();
        assert_eq!(la_device_info(dev, &mut info), Status::Ok);
        assert_eq!(
            (info.sample_rate, info.period_frames, info.low_latency),
            (48000, 256, 0)
        );

        let mut space = 0;
        assert_eq!(la_space_create(dev, 0, 1.0, 1.0, &mut space), Status::Ok);
        assert_eq!(
            la_space_create(dev, 7, 1.0, 1.0, &mut space),
            Status::InvalidArgument
        );
        let mut tone = 0;
        assert_eq!(
            la_buffer_tone(dev, 440.0, 500.0, 5.0, 5.0, &mut tone),
            Status::Ok
        );
        assert_eq!(
            la_listener_set(
                dev,
                space,
                Vec3::default(),
                Vec3::new(0.0, 0.0, -1.0),
                Vec3::new(0.0, 1.0, 0.0)
            ),
            Status::Ok
        );
        let descs = [source(1, tone, point(4.0, 0.0))];
        assert_eq!(la_space_sync(dev, space, descs.as_ptr(), 1), Status::Ok);
        assert_eq!(
            la_space_sync(dev, 9, descs.as_ptr(), 1),
            Status::InvalidArgument
        );

        let mut out = vec![0.0f32; 512];
        let (mut l, mut r) = (0.0, 0.0);
        for _ in 0..6 {
            assert_eq!(la_device_render(dev, out.as_mut_ptr(), 256), Status::Ok);
            l += out.iter().step_by(2).map(|s| s * s).sum::<f32>();
            r += out.iter().skip(1).step_by(2).map(|s| s * s).sum::<f32>();
        }
        assert!(r > l * 2.0, "east → right: l={l} r={r}");

        let lowpass = AudioEffectDesc {
            kind: AudioEffectKind::LowPass as u32,
            flags: 1,
            p: [200.0, 0.707, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0],
        };
        let target = AudioChainTarget {
            kind: AudioChainKind::Space as u32,
            space,
            bus: 0,
            _reserved: 0,
            source_id: 0,
        };
        assert_eq!(la_chain_set(dev, target, &lowpass, 1), Status::Ok);
        assert_eq!(la_effect_set_enabled(dev, target, 0, false), Status::Ok);
        let bad = AudioChainTarget { kind: 9, ..target };
        assert_eq!(la_chain_set(dev, bad, &lowpass, 1), Status::InvalidArgument);

        let fired = AudioSourceDesc {
            repeat_count: 1,
            id: 77,
            ..source(0, tone, point(0.0, -1.0))
        };
        assert_eq!(la_space_fire(dev, space, &fired), Status::Ok);
        for _ in 0..120 {
            la_device_render(dev, out.as_mut_ptr(), 256);
        }
        let mut done = [AudioFinished {
            space: 0,
            _reserved: 0,
            id: 0,
        }; 4];
        let mut n = 0;
        assert_eq!(
            la_device_finished(dev, std::ptr::null_mut(), 0, &mut n),
            Status::BufferTooSmall,
            "two-call pattern: size query"
        );
        assert_eq!(n, 1);
        assert_eq!(
            la_device_finished(dev, done.as_mut_ptr(), 4, &mut n),
            Status::Ok,
            "...consumes nothing, so the real call still gets it"
        );
        assert_eq!(n, 1);
        assert_eq!(
            done[0],
            AudioFinished {
                space,
                _reserved: 0,
                id: 77
            }
        );

        assert_eq!(la_buffer_free(dev, 9), Status::InvalidArgument);
        assert_eq!(la_space_destroy(dev, space), Status::Ok);
        la_device_close(dev);
        la_device_close(std::ptr::null_mut());
    }
}
