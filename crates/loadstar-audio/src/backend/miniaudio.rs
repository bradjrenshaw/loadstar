//! Device layer through a vendored miniaudio (`vendor/miniaudio`), compiled into this crate.
//! miniaudio picks the best backend it can (WASAPI with `IAudioClient3` low-latency shared
//! mode, then DirectSound, then WinMM on Windows; CoreAudio, ALSA, PulseAudio, JACK elsewhere),
//! handles default-device changes, and asks us for interleaved f32 stereo frames. Its own
//! engine, spatializer and resampler are compiled out — Loadstar's mixer produces every sample.
//!
//! Only four C functions cross the boundary (see `loadstar_ma.c`), so miniaudio upgrades never
//! touch Rust code.

#![allow(unsafe_code)]

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use super::{StreamInfo, StreamStatus};
use crate::commands::{Command, Garbage};
use crate::engine::{Engine, EngineConfig};

type Callback = unsafe extern "C" fn(user: *mut c_void, out: *mut f32, frames: u32);

unsafe extern "C" {
    fn ls_ma_open(
        sample_rate: u32,
        period_frames: u32,
        cb: Callback,
        user: *mut c_void,
        out_rate: *mut u32,
        out_period: *mut u32,
        out_backend: *mut *const c_char,
        out_result: *mut i32,
    ) -> *mut c_void;
    fn ls_ma_start(handle: *mut c_void) -> i32;
    fn ls_ma_close(handle: *mut c_void);
    fn ls_ma_result_description(result: i32) -> *const c_char;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MiniaudioConfig {
    /// 0 = the device's native rate.
    pub sample_rate: u32,
    /// 0 = the smallest sensible period (about 3 ms; the driver may clamp it up).
    pub period_frames: usize,
    pub master_gain: f32,
    pub voice_cap: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MiniaudioError(pub String);

/// Everything the data callback touches. Boxed so its address is stable while miniaudio holds
/// the pointer; only the audio thread uses it once the device has started.
struct State {
    engine: Option<Engine>,
    inbox: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Garbage>,
    finished: rtrb::Producer<(u32, u64)>,
    scheduled: bool,
}

pub struct MiniaudioStream {
    handle: *mut c_void,
    /// Owns the callback state; miniaudio holds a raw pointer into it until `ls_ma_close`.
    _state: Box<State>,
    pub info: StreamInfo,
    pub backend_name: String,
    status: Arc<StreamStatus>,
}

// The raw handle is used only by this thread (stop/close) and by miniaudio's own audio thread.
unsafe impl Send for MiniaudioStream {}

impl MiniaudioStream {
    pub fn status(&self) -> &StreamStatus {
        &self.status
    }
}

impl Drop for MiniaudioStream {
    fn drop(&mut self) {
        // Stopping the device joins miniaudio's thread, so the callback cannot run after this.
        unsafe { ls_ma_close(self.handle) };
        self.handle = std::ptr::null_mut();
        self.status.running.store(false, Ordering::Relaxed);
    }
}

unsafe extern "C" fn data_callback(user: *mut c_void, out: *mut f32, frames: u32) {
    // SAFETY: `user` is the `State` box that outlives the device (see Drop); miniaudio calls
    // this from one thread at a time, and the game thread never touches `State` after start.
    let state = unsafe { &mut *(user as *mut State) };
    if !state.scheduled {
        state.scheduled = true;
        schedule_realtime();
    }
    let Some(engine) = state.engine.as_mut() else {
        return;
    };

    while let Ok(cmd) = state.inbox.pop() {
        engine.run(cmd);
    }
    // SAFETY: miniaudio guarantees `out` holds `frames` interleaved stereo f32 samples.
    let out = unsafe { std::slice::from_raw_parts_mut(out, frames as usize * 2) };
    let block = engine.config().block_frames * 2;
    for chunk in out.chunks_mut(block) {
        engine.process(chunk);
    }
    // Nothing below allocates or frees: finished ids and released objects move into the rings;
    // what a full ring refuses waits for the next period.
    let finished = &mut state.finished;
    engine.drain_finished(|f| {
        let _ = finished.push(f);
    });
    let garbage = &mut state.garbage;
    engine.drain_garbage(|g| garbage.push(g).map_err(|rtrb::PushError::Full(g)| g));
}

/// MMCSS "Pro Audio" for miniaudio's thread — the same scheduling the direct backend uses.
fn schedule_realtime() {
    #[cfg(windows)]
    unsafe {
        let mut index = 0u32;
        let _ = windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
            windows::core::w!("Pro Audio"),
            &mut index,
        );
    }
}

/// Open the default playback device. Blocks until the device is initialised (the achieved
/// format is needed to size the engine) and started.
pub fn open(
    cfg: MiniaudioConfig,
    inbox: rtrb::Consumer<Command>,
    garbage: rtrb::Producer<Garbage>,
    finished: rtrb::Producer<(u32, u64)>,
) -> Result<MiniaudioStream, MiniaudioError> {
    let status = Arc::new(StreamStatus::default());
    let mut state = Box::new(State {
        engine: None,
        inbox,
        garbage,
        finished,
        scheduled: false,
    });

    // miniaudio only takes the IAudioClient3 low-latency path when a specific period is
    // requested (0 means its 10 ms default), so ask for ~3 ms; the driver clamps it up if needed.
    // Likewise `sample_rate` should stay 0: an application-set rate disables that path.
    let requested_period = if cfg.period_frames == 0 {
        128
    } else {
        cfg.period_frames as u32
    };
    let (mut rate, mut period, mut backend, mut result) =
        (0u32, 0u32, std::ptr::null::<c_char>(), 0i32);
    let user = &mut *state as *mut State as *mut c_void;
    // SAFETY: plain FFI call; out-pointers are valid locals.
    let handle = unsafe {
        ls_ma_open(
            cfg.sample_rate,
            requested_period,
            data_callback,
            user,
            &mut rate,
            &mut period,
            &mut backend,
            &mut result,
        )
    };
    if handle.is_null() {
        return Err(MiniaudioError(describe(result)));
    }
    let backend_name = if backend.is_null() {
        "unknown".to_string()
    } else {
        // SAFETY: miniaudio returns a static NUL-terminated string.
        unsafe { CStr::from_ptr(backend) }
            .to_string_lossy()
            .into_owned()
    };
    let info = StreamInfo {
        sample_rate: rate,
        period_frames: period.max(1) as usize,
        channels: 2,
    };

    state.engine = Some(Engine::new(EngineConfig {
        sample_rate: info.sample_rate,
        block_frames: info.period_frames,
        master_gain: cfg.master_gain,
        voice_cap: cfg.voice_cap,
    }));
    // SAFETY: valid handle from ls_ma_open.
    let started = unsafe { ls_ma_start(handle) };
    if started != 0 {
        unsafe { ls_ma_close(handle) };
        return Err(MiniaudioError(format!(
            "start failed: {}",
            describe(started)
        )));
    }
    status.running.store(true, Ordering::Relaxed);
    // A period under 5 ms only happens on a low-latency path (IAudioClient3 or equivalent).
    status.low_latency.store(
        info.period_frames as f32 * 1000.0 / info.sample_rate as f32 <= 5.0,
        Ordering::Relaxed,
    );

    Ok(MiniaudioStream {
        handle,
        _state: state,
        info,
        backend_name,
        status,
    })
}

fn describe(result: i32) -> String {
    // SAFETY: returns a static string for every code.
    let s = unsafe { CStr::from_ptr(ls_ma_result_description(result)) };
    format!("miniaudio: {} ({result})", s.to_string_lossy())
}
