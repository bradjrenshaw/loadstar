//! WASAPI shared-mode backend (spec §2): `IAudioClient3` at the driver's minimum period when
//! available, the legacy `IAudioClient` path with format auto-conversion otherwise; the audio
//! thread runs under MMCSS "Pro Audio", drains the command queue at every period, and reopens
//! the stream on device loss while the engine keeps all of its state.
//!
//! This is the only module in the crate with `unsafe` (COM and buffer pointers).

#![allow(unsafe_code)]

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    AUDCLNT_E_DEVICE_INVALIDATED, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEX, WAVEFORMATEXTENSIBLE, eConsole, eRender,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize};
use windows::Win32::System::Threading::{AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW, CreateEventW, WaitForSingleObject};
use windows::core::{GUID, Interface, w};

use super::StreamInfo;
use crate::commands::{Command, Garbage};
use crate::engine::{Engine, EngineConfig};

const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID = GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WasapiConfig {
    /// Frames per period; 0 = the smallest the driver offers.
    pub period_frames: usize,
    pub master_gain: f32,
    pub voice_cap: usize,
}

/// Live status of the audio thread, readable from the game thread.
#[derive(Debug, Default)]
pub struct StreamStatus {
    pub running: AtomicBool,
    /// True when the `IAudioClient3` low-latency path is active.
    pub low_latency: AtomicBool,
    pub reopen_count: AtomicU32,
    pub last_hresult: AtomicI32,
    pub underruns: AtomicU32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WasapiError(pub String);

impl std::fmt::Display for WasapiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<windows::core::Error> for WasapiError {
    fn from(e: windows::core::Error) -> Self {
        WasapiError(format!("{e} (0x{:08X})", e.code().0 as u32))
    }
}

pub struct WasapiStream {
    pub info: StreamInfo,
    stop: Arc<AtomicBool>,
    status: Arc<StreamStatus>,
    thread: Option<JoinHandle<()>>,
}

impl WasapiStream {
    pub fn status(&self) -> &StreamStatus {
        &self.status
    }

    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for WasapiStream {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Open the default render endpoint on a new audio thread. Blocks until the stream is running
/// (or fails), so the achieved format is known to the caller.
pub fn spawn(
    cfg: WasapiConfig,
    inbox: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Garbage>,
    finished: rtrb::Producer<(u32, u64)>,
) -> Result<WasapiStream, WasapiError> {
    let stop = Arc::new(AtomicBool::new(false));
    let status = Arc::new(StreamStatus::default());
    let (report_tx, report_rx) = mpsc::channel::<Result<StreamInfo, WasapiError>>();
    let thread = {
        let stop = Arc::clone(&stop);
        let status = Arc::clone(&status);
        std::thread::Builder::new()
            .name("loadstar-audio".into())
            .spawn(move || audio_thread(cfg, inbox, garbage, finished, stop, status, report_tx))
            .map_err(|e| WasapiError(format!("could not start the audio thread: {e}")))?
    };
    match report_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(info)) => Ok(WasapiStream { info, stop, status, thread: Some(thread) }),
        Ok(Err(e)) => {
            let _ = thread.join();
            Err(e)
        }
        Err(_) => {
            stop.store(true, Ordering::Relaxed);
            Err(WasapiError("timed out opening the audio device".into()))
        }
    }
}

struct Stream {
    client: IAudioClient,
    render: IAudioRenderClient,
    event: HANDLE,
    buffer_frames: u32,
    info: StreamInfo,
    low_latency: bool,
}

impl Drop for Stream {
    fn drop(&mut self) {
        unsafe {
            let _ = self.client.Stop();
            let _ = CloseHandle(self.event);
        }
    }
}

fn float_stereo(rate: u32) -> WAVEFORMATEXTENSIBLE {
    WAVEFORMATEXTENSIBLE {
        Format: WAVEFORMATEX {
            wFormatTag: WAVE_FORMAT_EXTENSIBLE,
            nChannels: 2,
            nSamplesPerSec: rate,
            nAvgBytesPerSec: rate * 8,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 22,
        },
        Samples: windows::Win32::Media::Audio::WAVEFORMATEXTENSIBLE_0 { wValidBitsPerSample: 32 },
        dwChannelMask: 0x3,
        SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
    }
}

/// Whether a mix format is exactly what the mixer produces: 32-bit float, stereo.
unsafe fn is_float_stereo(fmt: *const WAVEFORMATEX) -> bool {
    let f: WAVEFORMATEX = unsafe { std::ptr::read_unaligned(fmt) };
    if f.nChannels != 2 || f.wBitsPerSample != 32 {
        return false;
    }
    if f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT {
        return true;
    }
    if f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && f.cbSize >= 22 {
        // Copy the packed field out before comparing (no reference into a packed struct).
        let sub: GUID = unsafe { std::ptr::read_unaligned(std::ptr::addr_of!((*(fmt as *const WAVEFORMATEXTENSIBLE)).SubFormat)) };
        return sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
    }
    false
}

/// Open the stream. `engine_rate` pins the format to an existing engine's rate on reopen.
unsafe fn open_stream(cfg: &WasapiConfig, engine_rate: Option<u32>) -> Result<Stream, WasapiError> {
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;

        let mix = client.GetMixFormat()?;
        let mix_rate = std::ptr::read_unaligned(mix).nSamplesPerSec;
        let mix_is_ours = is_float_stereo(mix);
        let flags = AUDCLNT_STREAMFLAGS_EVENTCALLBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;

        let mut low_latency = false;
        let mut rate = engine_rate.unwrap_or(mix_rate);
        let mut period_frames: u32 = 0;

        // Low-latency path: only with the engine's own mix format, which is what IAudioClient3 requires.
        if mix_is_ours && engine_rate.is_none_or(|r| r == mix_rate) {
            if let Ok(client3) = client.cast::<IAudioClient3>() {
                let (mut default, mut fundamental, mut min, mut max) = (0u32, 0u32, 0u32, 0u32);
                if client3.GetSharedModeEnginePeriod(mix, &mut default, &mut fundamental, &mut min, &mut max).is_ok() {
                    let fundamental = fundamental.max(1);
                    let want = if cfg.period_frames == 0 {
                        min
                    } else {
                        let p = (cfg.period_frames as u32).clamp(min, max);
                        (p / fundamental * fundamental).max(min)
                    };
                    if client3.InitializeSharedAudioStream(AUDCLNT_STREAMFLAGS_EVENTCALLBACK, want, mix, None).is_ok() {
                        low_latency = true;
                        rate = mix_rate;
                        period_frames = want;
                    }
                }
            }
        }
        CoTaskMemFree(Some(mix as *const _));

        if !low_latency {
            // Legacy path: our format at the engine rate; the engine converts.
            let format = float_stereo(rate);
            client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, &format.Format, None)?;
            let mut default_period = 0i64;
            client.GetDevicePeriod(Some(&mut default_period), None)?;
            period_frames = ((default_period as f64) * 1e-7 * rate as f64).round().max(1.0) as u32;
        }

        let buffer_frames = client.GetBufferSize()?;
        let render: IAudioRenderClient = client.GetService()?;
        let event = CreateEventW(None, false, false, None)?;
        client.SetEventHandle(event)?;
        client.Start()?;

        Ok(Stream {
            client,
            render,
            event,
            buffer_frames,
            info: StreamInfo { sample_rate: rate, period_frames: period_frames as usize, channels: 2 },
            low_latency,
        })
    }
}

enum Exit {
    Stopped,
    DeviceLost,
}

/// Render periods until told to stop or the device goes away.
unsafe fn run(stream: &Stream, engine: &mut Engine, inbox: &mut rtrb::Consumer<Command>, garbage: &mut rtrb::Producer<Garbage>, finished: &mut rtrb::Producer<(u32, u64)>, stop: &AtomicBool, status: &StreamStatus) -> Exit {
    let block = engine.config().block_frames as u32;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Exit::Stopped;
        }
        let wait = unsafe { WaitForSingleObject(stream.event, 2000) };
        if wait != WAIT_OBJECT_0 {
            status.underruns.fetch_add(1, Ordering::Relaxed);
            return Exit::DeviceLost; // stalled: treat like loss and reopen
        }

        // Commands land together at a period boundary.
        while let Ok(cmd) = inbox.pop() {
            if let Some(g) = engine.apply(cmd) {
                let _ = garbage.push(g);
            }
        }

        let padding = match unsafe { stream.client.GetCurrentPadding() } {
            Ok(p) => p,
            Err(e) => {
                status.last_hresult.store(e.code().0, Ordering::Relaxed);
                return if e.code() == AUDCLNT_E_DEVICE_INVALIDATED { Exit::DeviceLost } else { Exit::DeviceLost };
            }
        };
        let mut available = stream.buffer_frames.saturating_sub(padding);
        while available >= block {
            let ptr = match unsafe { stream.render.GetBuffer(block) } {
                Ok(p) => p,
                Err(e) => {
                    status.last_hresult.store(e.code().0, Ordering::Relaxed);
                    return Exit::DeviceLost;
                }
            };
            // SAFETY: WASAPI hands us `block` frames of our 32-bit float stereo format.
            let out = unsafe { std::slice::from_raw_parts_mut(ptr as *mut f32, block as usize * 2) };
            engine.process(out);
            if unsafe { stream.render.ReleaseBuffer(block, 0) }.is_err() {
                return Exit::DeviceLost;
            }
            for f in engine.take_finished() {
                let _ = finished.push(f);
            }
            available -= block;
        }
    }
}

fn audio_thread(
    cfg: WasapiConfig,
    mut inbox: rtrb::Consumer<Command>,
    mut garbage: rtrb::Producer<Garbage>,
    mut finished: rtrb::Producer<(u32, u64)>,
    stop: Arc<AtomicBool>,
    status: Arc<StreamStatus>,
    report: mpsc::Sender<Result<StreamInfo, WasapiError>>,
) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
    }
    let mut task_index = 0u32;
    let mmcss = unsafe { AvSetMmThreadCharacteristicsW(w!("Pro Audio"), &mut task_index) }.ok();

    let mut engine: Option<Engine> = None;
    let mut report = Some(report);
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let engine_rate = engine.as_ref().map(|e| e.config().sample_rate);
        match unsafe { open_stream(&cfg, engine_rate) } {
            Ok(stream) => {
                status.low_latency.store(stream.low_latency, Ordering::Relaxed);
                let engine = engine.get_or_insert_with(|| {
                    Engine::new(EngineConfig {
                        sample_rate: stream.info.sample_rate,
                        block_frames: stream.info.period_frames,
                        master_gain: cfg.master_gain,
                        voice_cap: cfg.voice_cap,
                    })
                });
                if let Some(r) = report.take() {
                    let _ = r.send(Ok(stream.info));
                } else {
                    status.reopen_count.fetch_add(1, Ordering::Relaxed);
                }
                status.running.store(true, Ordering::Relaxed);
                let exit = unsafe { run(&stream, engine, &mut inbox, &mut garbage, &mut finished, &stop, &status) };
                status.running.store(false, Ordering::Relaxed);
                drop(stream);
                if matches!(exit, Exit::Stopped) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => {
                if let Some(r) = report.take() {
                    let _ = r.send(Err(e));
                    break;
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }

    if let Some(h) = mmcss {
        unsafe {
            let _ = AvRevertMmThreadCharacteristics(h);
        }
    }
    unsafe { CoUninitialize() };
}
