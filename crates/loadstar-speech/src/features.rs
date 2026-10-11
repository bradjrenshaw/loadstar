//! Leaf feature bits (speech-spec §2). One vocabulary for every backend; Prism's bits map onto it.

pub const SPEAK: u64 = 1 << 0;
/// Speech plus braille where the reader supports both.
pub const OUTPUT: u64 = 1 << 1;
pub const BRAILLE: u64 = 1 << 2;
pub const STOP: u64 = 1 << 3;
pub const PAUSE: u64 = 1 << 4;
/// The backend can say whether an utterance is still being spoken.
pub const IS_SPEAKING: u64 = 1 << 5;
pub const SET_RATE: u64 = 1 << 6;
pub const SET_PITCH: u64 = 1 << 7;
pub const SET_VOLUME: u64 = 1 << 8;
/// Speak to memory.
pub const RENDER: u64 = 1 << 9;
pub const SSML: u64 = 1 << 10;
/// More than one utterance may be in flight at once on this leaf (synthesizers yes, readers no).
pub const MULTI_INSTANCE: u64 = 1 << 11;
/// The output is the user's screen reader, not a synthesizer.
pub const SCREEN_READER: u64 = 1 << 12;

/// Prism's `PrismBackendFeature` bits (prism.h, pinned in `external/prism`).
pub mod prism {
    pub const IS_SUPPORTED_AT_RUNTIME: u64 = 1 << 0;
    pub const SUPPORTS_SPEAK: u64 = 1 << 2;
    pub const SUPPORTS_SPEAK_TO_MEMORY: u64 = 1 << 3;
    pub const SUPPORTS_BRAILLE: u64 = 1 << 4;
    pub const SUPPORTS_OUTPUT: u64 = 1 << 5;
    pub const SUPPORTS_IS_SPEAKING: u64 = 1 << 6;
    pub const SUPPORTS_STOP: u64 = 1 << 7;
    pub const SUPPORTS_PAUSE: u64 = 1 << 8;
    pub const SUPPORTS_RESUME: u64 = 1 << 9;
    pub const SUPPORTS_SET_VOLUME: u64 = 1 << 10;
    pub const SUPPORTS_SET_RATE: u64 = 1 << 12;
    pub const SUPPORTS_SET_PITCH: u64 = 1 << 14;
    pub const SUPPORTS_COUNT_VOICES: u64 = 1 << 17;
    pub const SUPPORTS_SET_VOICE: u64 = 1 << 21;
    pub const SUPPORTS_SPEAK_SSML: u64 = 1 << 27;
}

/// Translate a Prism feature set into ours.
pub fn from_prism(bits: u64) -> u64 {
    use prism::*;
    let mut f = 0;
    let mut map = |p: u64, ours: u64| {
        if bits & p != 0 {
            f |= ours;
        }
    };
    map(SUPPORTS_SPEAK, SPEAK);
    map(SUPPORTS_OUTPUT, OUTPUT);
    map(SUPPORTS_BRAILLE, BRAILLE);
    map(SUPPORTS_STOP, STOP);
    map(SUPPORTS_PAUSE, PAUSE);
    map(SUPPORTS_IS_SPEAKING, IS_SPEAKING);
    map(SUPPORTS_SET_RATE, SET_RATE);
    map(SUPPORTS_SET_PITCH, SET_PITCH);
    map(SUPPORTS_SET_VOLUME, SET_VOLUME);
    map(SUPPORTS_SPEAK_TO_MEMORY, RENDER);
    map(SUPPORTS_SPEAK_SSML, SSML);
    f
}

/// Human-readable names, for diagnostics.
pub fn describe(bits: u64) -> String {
    const NAMES: &[(u64, &str)] = &[
        (SPEAK, "speak"),
        (OUTPUT, "output"),
        (BRAILLE, "braille"),
        (STOP, "stop"),
        (PAUSE, "pause"),
        (IS_SPEAKING, "is-speaking"),
        (SET_RATE, "rate"),
        (SET_PITCH, "pitch"),
        (SET_VOLUME, "volume"),
        (RENDER, "render"),
        (SSML, "ssml"),
        (MULTI_INSTANCE, "multi-instance"),
        (SCREEN_READER, "screen-reader"),
    ];
    NAMES
        .iter()
        .filter(|(b, _)| bits & b != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(",")
}
