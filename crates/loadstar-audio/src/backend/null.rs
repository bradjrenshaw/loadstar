use super::StreamInfo;

/// A backend with no device and no thread: the owner pumps periods by hand with
/// [`crate::device::Device::render`]. Deterministic, allocation-free, used by every test.
#[derive(Clone, Copy, Debug)]
pub struct NullBackend {
    pub info: StreamInfo,
}

impl NullBackend {
    pub fn new(sample_rate: u32, period_frames: usize) -> NullBackend {
        NullBackend {
            info: StreamInfo {
                sample_rate,
                period_frames: period_frames.max(1),
                channels: 2,
            },
        }
    }
}
