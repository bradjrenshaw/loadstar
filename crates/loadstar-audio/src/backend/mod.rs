//! How periods reach hardware. A backend owns the audio thread and calls
//! `Engine::process` once per period; the null backend has no thread and is pumped by the
//! caller, which is what the tests and the FFI smoke harness use.

pub mod null;
pub use null::NullBackend;

/// The format a backend actually opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StreamInfo {
    pub sample_rate: u32,
    pub period_frames: usize,
    pub channels: usize,
}
