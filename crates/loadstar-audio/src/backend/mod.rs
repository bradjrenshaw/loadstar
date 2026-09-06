//! How periods reach hardware. A backend owns the audio thread and calls
//! `Engine::process` once per period; the null backend has no thread and is pumped by the
//! caller, which is what the tests and the FFI smoke harness use.

pub mod null;
pub use null::NullBackend;

#[cfg(windows)]
pub mod wasapi;

#[cfg(feature = "miniaudio")]
pub mod miniaudio;

/// Live status of an audio thread, readable from the game thread.
#[derive(Debug, Default)]
pub struct StreamStatus {
    pub running: std::sync::atomic::AtomicBool,
    /// True when a low-latency path (`IAudioClient3` or equivalent) is active.
    pub low_latency: std::sync::atomic::AtomicBool,
    pub reopen_count: std::sync::atomic::AtomicU32,
    pub last_hresult: std::sync::atomic::AtomicI32,
    pub underruns: std::sync::atomic::AtomicU32,
}

/// The format a backend actually opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub period_frames: usize,
    pub channels: usize,
}
