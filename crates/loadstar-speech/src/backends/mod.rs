//! The backends (speech-spec §4, §5). Order here is the tree's population order: native screen
//! readers first, then Prism's, then native synthesizers, then clipboard.

pub mod fake;

#[cfg(feature = "prism")]
pub mod prism;

#[cfg(all(feature = "sapi", windows))]
pub mod sapi;

#[cfg(all(feature = "avspeech", target_os = "macos"))]
pub mod avspeech;

#[cfg(all(feature = "clipboard", windows))]
pub mod clipboard;
