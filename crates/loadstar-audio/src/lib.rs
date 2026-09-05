//! Loadstar audio: low-latency spatial audio for game accessibility mods (see `docs/audio-spec.md`).
//!
//! - [`dsp`] — ramps, biquads, delay lines, pan laws, generators, resampling: pure functions.
//! - [`chain`] — effect chains as data ([`chain::EffectDesc`]) with click-free parameter changes.
//! - [`render`] — the pluggable mono → stereo renderer stage; [`render::HeadModel`] is built in.
//! - [`space`] — a listener, buses and sources with geometry; the per-period mix of one space.
//! - [`engine`] — audio-thread state: buffers, spaces, master chain, voice cap.
//! - [`device`] — the game-thread handle: the public API, feeding the engine through a
//!   lock-free command queue.
//! - [`backend`] — how periods reach hardware; the null backend drives everything in tests.
//!
//! The mixer never allocates, locks or blocks inside a period. Every audible parameter is a
//! target that ramps; filters retune once per block with state preserved.

#![deny(unsafe_code)]

pub mod backend;
pub mod chain;
pub mod commands;
pub mod device;
pub mod dsp;
pub mod engine;
pub mod render;
pub mod space;

pub use loadstar_core::geometry::{Bounds, Vec3};
