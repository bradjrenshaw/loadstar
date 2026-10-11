//! Loadstar speech (`docs/speech-spec.md`): the user picks a **voice** from one categorised tree;
//! the library owns **queues** with explicit interrupt semantics, a never-polling **worker** that
//! talks to the backends, polled **utterance states** in place of callbacks, render-to-PCM and —
//! with the `audio` feature — positioned speech fired through `loadstar-audio`'s in-process intake.
//!
//! Module map: [`tree`] (categories, leaves, autos, resolution), [`backend`] (what a speech source
//! implements), [`backends`] (Prism loader, SAPI 5, clipboard, a deterministic fake for tests),
//! [`worker`] (queues + the thread), [`state`] (the bounded utterance table), [`context`] (the
//! game-thread handle the FFI wraps).

pub mod backend;
pub mod backends;
pub mod context;
pub mod features;
pub mod ids;
pub mod state;
pub mod tree;
pub mod worker;

pub use backend::{Params, Pcm};
pub use context::{Config, Profile, QueueOptions, RenderState, Resolved, Speech, SpeechError};
pub use state::{FLAT, UtteranceState};
pub use tree::{Category, Leaf, Tree};
