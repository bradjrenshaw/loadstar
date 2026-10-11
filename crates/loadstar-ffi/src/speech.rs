//! The speech ABI (`lv_*`, see `docs/speech-spec.md` §7). The one surface where text crosses:
//! inbound as NUL-terminated UTF-8, outbound through the two-call buffer pattern (null buffer →
//! required capacity including the terminator; a real buffer is filled and NUL-terminated, the
//! reported length excludes the terminator). Everything else is flat data and status codes.

use std::ffi::{CStr, c_char};

use loadstar_speech::context::{Profile, QueueOptions, RenderState, SpeechError};
use loadstar_speech::{Config, Speech};

use crate::{Status, guard};

/// Opaque: a speech context. Exported to C as `LsSpeech`.
pub struct Speech_(Speech);

/// `lv_create` configuration.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpeechConfig {
    /// Explicit Prism library path (NUL-terminated UTF-8), or null for the default search.
    pub prism_path: *const c_char,
    /// Nonzero = load Prism (default). Zero = never, for deterministic tests.
    pub load_prism: u32,
    /// Utterances that keep a queryable state; 0 = 1024.
    pub utterance_history: u32,
}

/// A profile (speech-spec §2): flat, caller-owned, 24 bytes. `voice` 0 = root auto.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpeechProfile {
    pub voice: u64,
    pub rate: f32,
    pub pitch: f32,
    pub volume: f32,
    /// `SpeechProfileFlags`.
    pub flags: u32,
}

impl SpeechProfile {
    fn to_core(&self) -> Profile {
        Profile {
            voice: self.voice,
            rate: self.rate,
            pitch: self.pitch,
            volume: self.volume,
            flags: self.flags,
        }
    }
}

/// `SpeechProfile.flags` bits.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechProfileFlags {
    None = 0,
    /// Fail with `VoiceUnavailable` rather than walk up the tree.
    NoFallback = 1,
    /// Trim leading/trailing silence from rendered speech.
    TrimSilence = 2,
}

/// `lv_speak` / `lv_speak_at` flags.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeakFlags {
    None = 0,
    /// Stop what this queue is speaking, drop what it has pending, then speak this.
    Interrupt = 1,
    /// Go to the head of the queue without cutting anything.
    Front = 2,
    /// The text is SSML.
    Ssml = 4,
    /// Speech plus braille where the leaf supports it.
    BrailleToo = 8,
    /// Positional only: render through the first synthesizer category's auto when the voice cannot.
    RenderElsewhere = 16,
    /// Positional only: speak unpositioned when the voice cannot render; the state carries `Flat`.
    SpeakFlat = 32,
}

/// Leaf feature bits (`lv_voice_features`).
#[repr(u64)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpeechFeature {
    Speak = 1 << 0,
    Output = 1 << 1,
    Braille = 1 << 2,
    Stop = 1 << 3,
    Pause = 1 << 4,
    IsSpeaking = 1 << 5,
    SetRate = 1 << 6,
    SetPitch = 1 << 7,
    SetVolume = 1 << 8,
    Render = 1 << 9,
    Ssml = 1 << 10,
    MultiInstance = 1 << 11,
    ScreenReader = 1 << 12,
}

/// `lv_utterance_state` values; bit 0x10 (`Flat`) may be OR-ed onto `Speaking`/`Done`.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtteranceState {
    Pending = 0,
    Speaking = 1,
    Done = 2,
    Cancelled = 3,
    Unknown = 4,
    Flat = 0x10,
}

/// `lv_render_poll` values.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderJobState {
    Pending = 0,
    Done = 1,
    Failed = 2,
    Unknown = 3,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct SpeechQueueOptions {
    /// 1 = sequential; 0 = unlimited overlap.
    pub max_concurrent: u32,
    pub gap_ms: u32,
}

fn status(e: SpeechError) -> Status {
    match e {
        SpeechError::VoiceUnavailable => Status::VoiceUnavailable,
        SpeechError::NotSupported => Status::NotSupported,
        SpeechError::InvalidArgument => Status::InvalidArgument,
        SpeechError::Shutdown => Status::DeviceUnavailable,
    }
}

fn with<F: FnOnce(&Speech) -> Status>(ctx: *mut Speech_, f: F) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live handle from lv_create.
        f(&unsafe { &*ctx }.0)
    })
}

/// Inbound text: NUL-terminated UTF-8.
fn text_in<'a>(p: *const c_char) -> Result<&'a str, Status> {
    if p.is_null() {
        return Err(Status::NullPointer);
    }
    // SAFETY: the caller guarantees a NUL-terminated string.
    unsafe { CStr::from_ptr(p) }
        .to_str()
        .map_err(|_| Status::InvalidArgument)
}

/// Outbound text: the two-call buffer pattern.
fn text_out(s: &str, buf: *mut c_char, cap: usize, written: *mut usize) -> Status {
    if written.is_null() {
        return Status::NullPointer;
    }
    let need = s.len() + 1;
    if buf.is_null() || cap < need {
        // SAFETY: non-null checked.
        unsafe { *written = need };
        return Status::BufferTooSmall;
    }
    // SAFETY: cap >= len + 1 bytes are writable at buf.
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, s.len());
        *buf.add(s.len()) = 0;
        *written = s.len();
    }
    Status::Ok
}

fn out<T>(p: *mut T, v: T) -> Status {
    if p.is_null() {
        return Status::NullPointer;
    }
    // SAFETY: non-null checked; the caller guarantees it is writable.
    unsafe { *p = v };
    Status::Ok
}

fn profile(p: *const SpeechProfile) -> Result<Profile, Status> {
    if p.is_null() {
        return Err(Status::NullPointer);
    }
    // SAFETY: non-null checked; a readable 24-byte record.
    Ok(unsafe { *p }.to_core())
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

/// Open a speech context. Cheap: backends probe on the worker; nothing speaks.
#[unsafe(no_mangle)]
pub extern "C" fn lv_create(cfg: *const SpeechConfig, out_ctx: *mut *mut Speech_) -> Status {
    guard(|| {
        if out_ctx.is_null() {
            return Status::NullPointer;
        }
        let mut config = Config::default();
        if !cfg.is_null() {
            // SAFETY: non-null checked; a readable config record.
            let c = unsafe { *cfg };
            if !c.prism_path.is_null() {
                match text_in(c.prism_path) {
                    Ok(p) => config.prism_path = Some(p.into()),
                    Err(s) => return s,
                }
            }
            config.load_prism = c.load_prism != 0;
            if c.utterance_history != 0 {
                config.utterance_history = c.utterance_history as usize;
            }
        }
        // SAFETY: out_ctx non-null checked.
        unsafe { *out_ctx = Box::into_raw(Box::new(Speech_(Speech::new(config)))) };
        Status::Ok
    })
}

/// Close a context: stops every queue, joins the worker. Null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn lv_destroy(ctx: *mut Speech_) {
    let _ = guard(|| {
        if !ctx.is_null() {
            // SAFETY: from lv_create; the caller will not reuse it.
            drop(unsafe { Box::from_raw(ctx) });
        }
        Status::Ok
    });
}

/// The Prism tag this build was validated against, and where the library was loaded from
/// (`"v0.18.3 @ C:\...\prism.dll"`); empty when Prism is not loaded.
#[unsafe(no_mangle)]
pub extern "C" fn lv_prism_version(
    ctx: *mut Speech_,
    buf: *mut c_char,
    cap: usize,
    written: *mut usize,
) -> Status {
    with(ctx, |s| match s.prism_loaded_from() {
        #[cfg(feature = "speech-prism")]
        Some(p) => text_out(
            &format!(
                "{} @ {}",
                loadstar_speech::backends::prism::PINNED_TAG,
                p.display()
            ),
            buf,
            cap,
            written,
        ),
        #[cfg(not(feature = "speech-prism"))]
        Some(p) => text_out(&p.display().to_string(), buf, cap, written),
        None => text_out("", buf, cap, written),
    })
}

// ---------------------------------------------------------------------------
// The tree
// ---------------------------------------------------------------------------

/// Ask for a re-probe; `changed` reports whether the tree's shape changed since the last call
/// (the re-probe lands asynchronously — call again next frame to see its result).
#[unsafe(no_mangle)]
pub extern "C" fn lv_tree_refresh(ctx: *mut Speech_, changed: *mut bool) -> Status {
    with(ctx, |s| {
        let c = s.refresh();
        if changed.is_null() {
            Status::Ok
        } else {
            out(changed, c)
        }
    })
}

/// Snapshot generation; 0 until the first probe has published.
#[unsafe(no_mangle)]
pub extern "C" fn lv_tree_generation(ctx: *mut Speech_, generation: *mut u64) -> Status {
    with(ctx, |s| out(generation, s.generation()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_category_count(ctx: *mut Speech_, count: *mut usize) -> Status {
    with(ctx, |s| out(count, s.tree().categories.len()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_category_at(ctx: *mut Speech_, index: usize, id: *mut u64) -> Status {
    with(ctx, |s| match s.tree().categories.get(index) {
        Some(c) => out(id, c.id),
        None => Status::InvalidArgument,
    })
}

/// The category's fixed key (`screen_readers`, `sapi5`, …) — what a mod localizes.
#[unsafe(no_mangle)]
pub extern "C" fn lv_category_key(
    ctx: *mut Speech_,
    id: u64,
    buf: *mut c_char,
    cap: usize,
    written: *mut usize,
) -> Status {
    with(ctx, |s| match s.tree().category(id) {
        Some(c) => text_out(&c.key, buf, cap, written),
        None => Status::InvalidArgument,
    })
}

/// The category's default (English) display name — the fallback for unknown keys.
#[unsafe(no_mangle)]
pub extern "C" fn lv_category_name(
    ctx: *mut Speech_,
    id: u64,
    buf: *mut c_char,
    cap: usize,
    written: *mut usize,
) -> Status {
    with(ctx, |s| match s.tree().category(id) {
        Some(c) => text_out(&c.name, buf, cap, written),
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_category_rank(ctx: *mut Speech_, id: u64, rank: *mut i32) -> Status {
    with(ctx, |s| match s.tree().category(id) {
        Some(c) => out(rank, c.rank),
        None => Status::InvalidArgument,
    })
}

/// The voice id that means "this category's best".
#[unsafe(no_mangle)]
pub extern "C" fn lv_category_auto(ctx: *mut Speech_, id: u64, voice: *mut u64) -> Status {
    with(ctx, |s| match s.tree().category(id) {
        Some(c) => out(voice, c.auto),
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_count(ctx: *mut Speech_, category: u64, count: *mut usize) -> Status {
    with(ctx, |s| out(count, s.tree().leaves_in(category).count()))
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_at(
    ctx: *mut Speech_,
    category: u64,
    index: usize,
    id: *mut u64,
) -> Status {
    with(ctx, |s| match s.tree().leaves_in(category).nth(index) {
        Some(l) => out(id, l.id),
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_name(
    ctx: *mut Speech_,
    id: u64,
    buf: *mut c_char,
    cap: usize,
    written: *mut usize,
) -> Status {
    with(ctx, |s| match s.tree().leaf(id) {
        Some(l) => text_out(&l.name, buf, cap, written),
        None => Status::InvalidArgument,
    })
}

/// BCP-47 where the backend knows it; empty otherwise.
#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_language(
    ctx: *mut Speech_,
    id: u64,
    buf: *mut c_char,
    cap: usize,
    written: *mut usize,
) -> Status {
    with(ctx, |s| match s.tree().leaf(id) {
        Some(l) => text_out(&l.language, buf, cap, written),
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_category(ctx: *mut Speech_, id: u64, category: *mut u64) -> Status {
    with(ctx, |s| match s.tree().leaf(id) {
        Some(l) => out(category, l.category),
        None => Status::InvalidArgument,
    })
}

/// Bitwise OR of `SpeechFeature`.
#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_features(ctx: *mut Speech_, id: u64, bits: *mut u64) -> Status {
    with(ctx, |s| match s.tree().leaf(id) {
        Some(l) => out(bits, l.features),
        None => Status::InvalidArgument,
    })
}

/// Whether the leaf can speak right now (reader running, voice installed).
#[unsafe(no_mangle)]
pub extern "C" fn lv_voice_available(ctx: *mut Speech_, id: u64, available: *mut bool) -> Status {
    with(ctx, |s| match s.tree().leaf(id) {
        Some(l) => out(available, l.available),
        None => Status::InvalidArgument,
    })
}

/// What `profile` would use right now for a call needing `required` features. `voice` is the
/// leaf that would speak (0 = nothing, with `VoiceUnavailable`), `missing` the required bits it lacks.
#[unsafe(no_mangle)]
pub extern "C" fn lv_resolve(
    ctx: *mut Speech_,
    profile_: *const SpeechProfile,
    required: u64,
    voice: *mut u64,
    fell_back: *mut bool,
    missing: *mut u64,
) -> Status {
    with(ctx, |s| {
        let p = match profile(profile_) {
            Ok(p) => p,
            Err(st) => return st,
        };
        let r = s.resolve(&p, required);
        let st = out(voice, r.voice);
        if st != Status::Ok {
            return st;
        }
        if !fell_back.is_null() {
            out(fell_back, r.fell_back);
        }
        if !missing.is_null() {
            out(missing, r.missing);
        }
        if r.unprobed {
            Status::Ok
        } else if r.voice == 0 {
            Status::VoiceUnavailable
        } else if r.missing != 0 {
            Status::NotSupported
        } else {
            Status::Ok
        }
    })
}

// ---------------------------------------------------------------------------
// Queues
// ---------------------------------------------------------------------------

fn opts(p: *const SpeechQueueOptions) -> QueueOptions {
    if p.is_null() {
        return QueueOptions::default();
    }
    // SAFETY: non-null checked; a readable record.
    let o = unsafe { *p };
    QueueOptions {
        max_concurrent: o.max_concurrent,
        gap_ms: o.gap_ms,
    }
}

/// Create a queue (`options` may be null for sequential, no gap). Queue 0 always exists.
#[unsafe(no_mangle)]
pub extern "C" fn lv_queue_create(
    ctx: *mut Speech_,
    options: *const SpeechQueueOptions,
    queue: *mut u32,
) -> Status {
    with(ctx, |s| out(queue, s.queue_create(opts(options))))
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_queue_set(
    ctx: *mut Speech_,
    queue: u32,
    options: *const SpeechQueueOptions,
) -> Status {
    with(ctx, |s| {
        s.queue_set(queue, opts(options));
        Status::Ok
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_queue_destroy(ctx: *mut Speech_, queue: u32) -> Status {
    with(ctx, |s| {
        s.queue_destroy(queue);
        Status::Ok
    })
}

// ---------------------------------------------------------------------------
// Speaking
// ---------------------------------------------------------------------------

/// Speak `text` (NUL-terminated UTF-8) on `queue` through `profile`. Returns the utterance id.
#[unsafe(no_mangle)]
pub extern "C" fn lv_speak(
    ctx: *mut Speech_,
    queue: u32,
    profile_: *const SpeechProfile,
    text: *const c_char,
    flags: u32,
    utterance: *mut u64,
) -> Status {
    with(ctx, |s| {
        let (p, t) = match (profile(profile_), text_in(text)) {
            (Ok(p), Ok(t)) => (p, t),
            (Err(st), _) | (_, Err(st)) => return st,
        };
        match s.speak(queue, &p, t, flags) {
            Ok(id) => out(utterance, id),
            Err(e) => status(e),
        }
    })
}

/// Speech plus braille where the leaf supports both (`lv_speak` with `BrailleToo`).
#[unsafe(no_mangle)]
pub extern "C" fn lv_output(
    ctx: *mut Speech_,
    queue: u32,
    profile_: *const SpeechProfile,
    text: *const c_char,
    flags: u32,
    utterance: *mut u64,
) -> Status {
    lv_speak(
        ctx,
        queue,
        profile_,
        text,
        flags | SpeakFlags::BrailleToo as u32,
        utterance,
    )
}

/// Braille only.
#[unsafe(no_mangle)]
pub extern "C" fn lv_braille(
    ctx: *mut Speech_,
    queue: u32,
    profile_: *const SpeechProfile,
    text: *const c_char,
    utterance: *mut u64,
) -> Status {
    with(ctx, |s| {
        let (p, t) = match (profile(profile_), text_in(text)) {
            (Ok(p), Ok(t)) => (p, t),
            (Err(st), _) | (_, Err(st)) => return st,
        };
        match s.braille(queue, &p, t) {
            Ok(id) => out(utterance, id),
            Err(e) => status(e),
        }
    })
}

/// Silence as a queue item.
#[unsafe(no_mangle)]
pub extern "C" fn lv_gap(ctx: *mut Speech_, queue: u32, ms: u32, utterance: *mut u64) -> Status {
    with(ctx, |s| match s.gap(queue, ms) {
        Ok(id) => out(utterance, id),
        Err(e) => status(e),
    })
}

/// Cut what this queue is speaking; keep what is pending.
#[unsafe(no_mangle)]
pub extern "C" fn lv_stop(ctx: *mut Speech_, queue: u32) -> Status {
    with(ctx, |s| {
        s.stop(queue);
        Status::Ok
    })
}

/// Drop everything pending on this queue; keep what is speaking.
#[unsafe(no_mangle)]
pub extern "C" fn lv_flush(ctx: *mut Speech_, queue: u32) -> Status {
    with(ctx, |s| {
        s.flush(queue);
        Status::Ok
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_pause(ctx: *mut Speech_, queue: u32) -> Status {
    with(ctx, |s| {
        s.pause(queue);
        Status::Ok
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_resume(ctx: *mut Speech_, queue: u32) -> Status {
    with(ctx, |s| {
        s.resume(queue);
        Status::Ok
    })
}

/// One utterance: cut if speaking, drop if pending.
#[unsafe(no_mangle)]
pub extern "C" fn lv_cancel(ctx: *mut Speech_, utterance: u64) -> Status {
    with(ctx, |s| {
        s.cancel(utterance);
        Status::Ok
    })
}

/// Every queue, every backend.
#[unsafe(no_mangle)]
pub extern "C" fn lv_stop_all(ctx: *mut Speech_) -> Status {
    with(ctx, |s| {
        s.stop_all();
        Status::Ok
    })
}

/// `UtteranceState`, possibly with `Flat` OR-ed in.
#[unsafe(no_mangle)]
pub extern "C" fn lv_utterance_state(ctx: *mut Speech_, utterance: u64, state: *mut u32) -> Status {
    with(ctx, |s| {
        let (st, flat, _) = s.utterance(utterance);
        out(
            state,
            st as u32 | if flat { UtteranceState::Flat as u32 } else { 0 },
        )
    })
}

/// The leaf that spoke (or will speak) the utterance after any fallback; 0 if not yet known.
#[unsafe(no_mangle)]
pub extern "C" fn lv_utterance_voice(ctx: *mut Speech_, utterance: u64, voice: *mut u64) -> Status {
    with(ctx, |s| out(voice, s.utterance(utterance).2))
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Start a render job (worker thread, polled). `NotSupported` synchronously if the voice cannot render.
#[unsafe(no_mangle)]
pub extern "C" fn lv_render(
    ctx: *mut Speech_,
    profile_: *const SpeechProfile,
    text: *const c_char,
    flags: u32,
    job: *mut u64,
) -> Status {
    with(ctx, |s| {
        let (p, t) = match (profile(profile_), text_in(text)) {
            (Ok(p), Ok(t)) => (p, t),
            (Err(st), _) | (_, Err(st)) => return st,
        };
        match s.render(&p, t, flags & SpeakFlags::Ssml as u32 != 0) {
            Ok(id) => out(job, id),
            Err(e) => status(e),
        }
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_render_poll(ctx: *mut Speech_, job: u64, state: *mut u32) -> Status {
    with(ctx, |s| {
        let st = match s.render_poll(job) {
            RenderState::Pending => RenderJobState::Pending,
            RenderState::Done => RenderJobState::Done,
            RenderState::Failed => RenderJobState::Failed,
            RenderState::Unknown => RenderJobState::Unknown,
        };
        out(state, st as u32)
    })
}

/// The finished PCM (interleaved f32), library-owned and valid until `lv_render_free`.
/// `InvalidArgument` while the job is not done.
#[unsafe(no_mangle)]
pub extern "C" fn lv_render_take(
    ctx: *mut Speech_,
    job: u64,
    samples: *mut *const f32,
    frames: *mut usize,
    channels: *mut u32,
    rate: *mut u32,
) -> Status {
    with(ctx, |s| match s.render_take(job) {
        Some((p, f, c, r)) => {
            if samples.is_null() || frames.is_null() || channels.is_null() || rate.is_null() {
                return Status::NullPointer;
            }
            // SAFETY: all four out-pointers checked non-null.
            unsafe {
                *samples = p;
                *frames = f;
                *channels = c;
                *rate = r;
            }
            Status::Ok
        }
        None => Status::InvalidArgument,
    })
}

#[unsafe(no_mangle)]
pub extern "C" fn lv_render_free(ctx: *mut Speech_, job: u64) -> Status {
    with(ctx, |s| {
        s.render_free(job);
        Status::Ok
    })
}

/// Positional speech (audio feature): rendered on the worker, fired into `space` as a one-shot
/// source built from `desc` (the mod's bus, bounds, gain, pan width, priority; `id`, `buffer` and
/// the PLAYING/AUTO_REMOVE flags are the library's), honouring the queue like any utterance.
/// `source` is the mod's handle for `la_source_stop` / `la_source_upsert` from the moment this
/// returns.
#[cfg(feature = "audio")]
#[unsafe(no_mangle)]
pub extern "C" fn lv_speak_at(
    ctx: *mut Speech_,
    queue: u32,
    profile_: *const SpeechProfile,
    text: *const c_char,
    flags: u32,
    device: *mut crate::audio::AudioDevice,
    space: u32,
    desc: *const crate::audio::AudioSourceDesc,
    utterance: *mut u64,
    source: *mut u64,
) -> Status {
    with(ctx, |s| {
        let (p, t) = match (profile(profile_), text_in(text)) {
            (Ok(p), Ok(t)) => (p, t),
            (Err(st), _) | (_, Err(st)) => return st,
        };
        if device.is_null() || desc.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live device handle and a readable source record.
        let intake = unsafe { &*device }.0.intake();
        let Some(template) = unsafe { *desc }.to_core() else {
            return Status::InvalidArgument;
        };
        let target = loadstar_speech::context::PositionalTarget {
            intake,
            space,
            desc: template,
        };
        match s.speak_at(queue, &p, t, flags, target) {
            Ok((id, src)) => {
                let st = out(utterance, id);
                if st != Status::Ok {
                    return st;
                }
                out(source, src)
            }
            Err(e) => status(e),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_struct_sizes_are_pinned() {
        assert_eq!(std::mem::size_of::<SpeechProfile>(), 24);
        assert_eq!(std::mem::size_of::<SpeechQueueOptions>(), 8);
        assert_eq!(std::mem::size_of::<SpeechConfig>(), 16);
    }

    #[test]
    fn strings_cross_as_specified() {
        let mut n = 0usize;
        assert_eq!(
            text_out("abc", std::ptr::null_mut(), 0, &mut n),
            Status::BufferTooSmall
        );
        assert_eq!(n, 4, "capacity including the terminator");
        let mut buf = [1i8 as c_char; 8];
        assert_eq!(
            text_out("abc", buf.as_mut_ptr(), 3, &mut n),
            Status::BufferTooSmall
        );
        assert_eq!(text_out("abc", buf.as_mut_ptr(), 4, &mut n), Status::Ok);
        assert_eq!(
            (n, buf[3]),
            (3, 0),
            "length excludes the terminator, which is written"
        );
        assert_eq!(text_in(c"héllo".as_ptr()), Ok("héllo"));
        assert_eq!(text_in(std::ptr::null()), Err(Status::NullPointer));
        let bad = [0xffu8 as c_char, 0];
        assert_eq!(text_in(bad.as_ptr()), Err(Status::InvalidArgument));
    }

    #[test]
    fn a_context_without_prism_speaks_through_the_abi() {
        let cfg = SpeechConfig {
            prism_path: std::ptr::null(),
            load_prism: 0,
            utterance_history: 64,
        };
        let mut ctx = std::ptr::null_mut();
        assert_eq!(lv_create(&cfg, &mut ctx), Status::Ok);
        // Wait for the first probe.
        let mut generation = 0u64;
        let start = std::time::Instant::now();
        while generation == 0 && start.elapsed() < std::time::Duration::from_secs(5) {
            lv_tree_generation(ctx, &mut generation);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        let mut count = 0usize;
        assert_eq!(lv_category_count(ctx, &mut count), Status::Ok);
        let mut buf = vec![0 as c_char; 64];
        let mut n = 0usize;
        for i in 0..count {
            let mut id = 0u64;
            assert_eq!(lv_category_at(ctx, i, &mut id), Status::Ok);
            assert_eq!(
                lv_category_key(ctx, id, buf.as_mut_ptr(), buf.len(), &mut n),
                Status::Ok
            );
        }
        let p = SpeechProfile {
            voice: 0,
            rate: 0.5,
            pitch: 0.5,
            volume: 1.0,
            flags: 0,
        };
        let mut utt = 0u64;
        // Empty text is an already-done utterance regardless of what is installed.
        assert_eq!(lv_speak(ctx, 0, &p, c"".as_ptr(), 0, &mut utt), Status::Ok);
        let mut state = 99u32;
        assert_eq!(lv_utterance_state(ctx, utt, &mut state), Status::Ok);
        assert_eq!(state, UtteranceState::Done as u32);
        assert_eq!(
            lv_speak(ctx, 0, std::ptr::null(), c"x".as_ptr(), 0, &mut utt),
            Status::NullPointer
        );
        lv_destroy(ctx);
        lv_destroy(std::ptr::null_mut());
    }
}
