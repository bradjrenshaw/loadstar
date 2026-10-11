//! Prism as a backend (speech-spec §4): `prism.dll` loaded at runtime with `libloading`, every
//! symbol resolved by name, each reader it reports available becoming a leaf under
//! `screen_readers`. Prism's synthesizer backends never become leaves on Windows (the native
//! paths are faster and would duplicate them). Declarations mirror `external/prism/include/prism.h`
//! at the pinned tag; a signature there changing means this file changes with it.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::{Path, PathBuf};
use std::time::Duration;

use libloading::Library;

use crate::backend::{Backend, BackendError, LeafSpec, Params, Pcm, RenderBackend, Slot};
use crate::features::{self, prism as pf};
use crate::tree;

/// The Prism release this file was written and validated against.
pub const PINNED_TAG: &str = "v0.18.3";

type Ctx = *mut c_void;
type Be = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
#[allow(dead_code)]
struct PrismConfig {
    version: u8,
}

const PRISM_OK: c_int = 0;
const PRISM_ERROR_ALREADY_INITIALIZED: c_int = 15;

type FnConfigInit = unsafe extern "C" fn() -> PrismConfig;
type FnInit = unsafe extern "C" fn(*mut PrismConfig) -> Ctx;
type FnShutdown = unsafe extern "C" fn(Ctx);
type FnRegistryCount = unsafe extern "C" fn(Ctx) -> usize;
type FnRegistryIdAt = unsafe extern "C" fn(Ctx, usize) -> u64;
type FnRegistryName = unsafe extern "C" fn(Ctx, u64) -> *const c_char;
type FnRegistryCreate = unsafe extern "C" fn(Ctx, u64) -> Be;
type FnBackendFree = unsafe extern "C" fn(Be);
type FnBackendFeatures = unsafe extern "C" fn(Be) -> u64;
type FnBackendInit = unsafe extern "C" fn(Be) -> c_int;
type FnBackendText = unsafe extern "C" fn(Be, *const c_char, bool) -> c_int;
type FnBackendBraille = unsafe extern "C" fn(Be, *const c_char) -> c_int;
type FnBackendVoid = unsafe extern "C" fn(Be) -> c_int;
type FnBackendOutBool = unsafe extern "C" fn(Be, *mut bool) -> c_int;
type FnBackendF32 = unsafe extern "C" fn(Be, f32) -> c_int;
type AudioCallback = unsafe extern "C" fn(*mut c_void, *const f32, usize, usize, usize);
type FnSpeakToMemory = unsafe extern "C" fn(Be, *const c_char, AudioCallback, *mut c_void) -> c_int;
type FnErrorString = unsafe extern "C" fn(c_int) -> *const c_char;

/// Function pointers resolved from the library. Load-bearing ones are plain; optional ones are
/// `Option` and mask off the feature they serve when absent.
struct Api {
    _lib: Library,
    #[allow(dead_code)]
    config_init: Option<FnConfigInit>,
    init: FnInit,
    shutdown: FnShutdown,
    registry_count: FnRegistryCount,
    registry_id_at: FnRegistryIdAt,
    registry_name: FnRegistryName,
    registry_create: FnRegistryCreate,
    backend_free: FnBackendFree,
    backend_features: FnBackendFeatures,
    backend_init: FnBackendInit,
    speak: FnBackendText,
    output: Option<FnBackendText>,
    braille: Option<FnBackendBraille>,
    stop: FnBackendVoid,
    pause: Option<FnBackendVoid>,
    resume: Option<FnBackendVoid>,
    is_speaking: Option<FnBackendOutBool>,
    set_volume: Option<FnBackendF32>,
    set_rate: Option<FnBackendF32>,
    set_pitch: Option<FnBackendF32>,
    speak_to_memory: Option<FnSpeakToMemory>,
    error_string: Option<FnErrorString>,
}

macro_rules! required {
    ($lib:expr, $name:literal, $t:ty) => {
        // SAFETY: the symbol's type is the header's declaration for this name.
        match unsafe { $lib.get::<$t>(concat!($name, "\0").as_bytes()) } {
            Ok(s) => *s,
            Err(_) => return Err(format!("prism: missing export {}", $name)),
        }
    };
}

macro_rules! optional {
    ($lib:expr, $name:literal, $t:ty) => {
        // SAFETY: as above; absence only masks a feature.
        unsafe { $lib.get::<$t>(concat!($name, "\0").as_bytes()) }
            .ok()
            .map(|s| *s)
    };
}

impl Api {
    fn load(path: &Path) -> Result<Api, String> {
        // SAFETY: loading a library runs its initializers; Prism's are the C++ runtime's.
        let lib = unsafe { Library::new(path) }.map_err(|e| format!("prism: {e}"))?;
        Ok(Api {
            config_init: optional!(lib, "prism_config_init", FnConfigInit),
            init: required!(lib, "prism_init", FnInit),
            shutdown: required!(lib, "prism_shutdown", FnShutdown),
            registry_count: required!(lib, "prism_registry_count", FnRegistryCount),
            registry_id_at: required!(lib, "prism_registry_id_at", FnRegistryIdAt),
            registry_name: required!(lib, "prism_registry_name", FnRegistryName),
            registry_create: required!(lib, "prism_registry_create", FnRegistryCreate),
            backend_free: required!(lib, "prism_backend_free", FnBackendFree),
            backend_features: required!(lib, "prism_backend_get_features", FnBackendFeatures),
            backend_init: required!(lib, "prism_backend_initialize", FnBackendInit),
            speak: required!(lib, "prism_backend_speak", FnBackendText),
            output: optional!(lib, "prism_backend_output", FnBackendText),
            braille: optional!(lib, "prism_backend_braille", FnBackendBraille),
            stop: required!(lib, "prism_backend_stop", FnBackendVoid),
            pause: optional!(lib, "prism_backend_pause", FnBackendVoid),
            resume: optional!(lib, "prism_backend_resume", FnBackendVoid),
            is_speaking: optional!(lib, "prism_backend_is_speaking", FnBackendOutBool),
            set_volume: optional!(lib, "prism_backend_set_volume", FnBackendF32),
            set_rate: optional!(lib, "prism_backend_set_rate", FnBackendF32),
            set_pitch: optional!(lib, "prism_backend_set_pitch", FnBackendF32),
            speak_to_memory: optional!(lib, "prism_backend_speak_to_memory", FnSpeakToMemory),
            error_string: optional!(lib, "prism_error_string", FnErrorString),
            _lib: lib,
        })
    }

    fn describe(&self, err: c_int) -> String {
        match self.error_string {
            // SAFETY: Prism returns a static string for every code.
            Some(f) => unsafe { CStr::from_ptr(f(err)) }
                .to_string_lossy()
                .into_owned(),
            None => format!("prism error {err}"),
        }
    }
}

/// Prism backend ids (prism.h) that are screen readers / assistive outputs rather than synthesizers.
const READERS: &[u64] = &[
    0x89CC19C5C4AC1A56, // NVDA
    0xAC3D60E9BD84B53E, // JAWS
    0x10AA1FC05A17F96C, // Orca
    0x6238F019DB678F8E, // UIA (Narrator)
    0x3D93C56C9E7F2A2E, // ZDSR
    0xAE439D62DC7B1479, // ZoomText
    0x285aba1c16f3300f, // Boy PC Reader
    0x344B951962E3B835, // PC-Talker
    0xED4760890B55C2F2, // SenseReader
    0x8380F2A37B2C3EB6, // System Access
    0x9120D89908785C13, // Window-Eyes
    0xCB4897961A754BCB, // VoiceOver
    0xD199C175AEEC494B, // Android screen reader
];

/// Where to look for the library (speech-spec §4.3).
pub fn candidates(explicit: Option<&Path>) -> Vec<PathBuf> {
    let name = if cfg!(windows) {
        "prism.dll"
    } else if cfg!(target_os = "macos") {
        "libprism.dylib"
    } else {
        "libprism.so"
    };
    let mut out = Vec::new();
    if let Some(p) = explicit {
        out.push(p.to_path_buf());
    }
    if let Some(dir) = own_module_dir() {
        out.push(dir.join(name));
    }
    if let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
    {
        out.push(dir.join(name));
    }
    out.push(PathBuf::from(name)); // the platform loader's default search
    out.dedup();
    out
}

#[cfg(windows)]
fn own_module_dir() -> Option<PathBuf> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    use windows::core::PCWSTR;
    let mut module = HMODULE::default();
    // SAFETY: a function address inside this module identifies it; the buffer is sized by MAX_PATH.
    unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            PCWSTR(own_module_dir as *const u16),
            &mut module,
        )
        .ok()?;
        let mut buf = [0u16; 1024];
        let n = GetModuleFileNameW(Some(module), &mut buf) as usize;
        if n == 0 {
            return None;
        }
        PathBuf::from(String::from_utf16_lossy(&buf[..n]))
            .parent()
            .map(Path::to_path_buf)
    }
}

#[cfg(not(windows))]
fn own_module_dir() -> Option<PathBuf> {
    None
}

struct Instance {
    be: Be,
    features: u64,
    rate: Option<f32>,
    volume: Option<f32>,
    pitch: Option<f32>,
}

/// The Prism backend. Absent library = no leaves, never an error.
pub struct PrismBackend {
    api: Option<Api>,
    ctx: Ctx,
    loaded_from: Option<PathBuf>,
    instances: HashMap<u64, Instance>,
    /// Prism's own feature bits per backend id, from the last probe.
    raw_features: HashMap<u64, u64>,
}

unsafe impl Send for PrismBackend {}

impl PrismBackend {
    pub fn load(explicit: Option<&Path>) -> PrismBackend {
        let mut api = None;
        let mut loaded_from = None;
        for c in candidates(explicit) {
            if let Ok(a) = Api::load(&c) {
                loaded_from = Some(c);
                api = Some(a);
                break;
            }
        }
        let ctx = match &api {
            // A null config means "defaults" and is what every consumer of Prism has shipped with.
            // `prism_config_init` is deliberately NOT called: in the v0.18.3 Windows x64 release
            // the call itself faults (the by-value 1-byte struct return does not match the binary),
            // and the header promises nothing a null config does not already give.
            // SAFETY: Prism's documented init entry with its documented null-config meaning.
            Some(a) => unsafe { (a.init)(std::ptr::null_mut()) },
            None => std::ptr::null_mut(),
        };
        if ctx.is_null() {
            api = None;
        }
        PrismBackend {
            api,
            ctx,
            loaded_from,
            instances: HashMap::new(),
            raw_features: HashMap::new(),
        }
    }

    pub fn is_loaded(&self) -> bool {
        self.api.is_some()
    }

    pub fn loaded_from(&self) -> Option<&Path> {
        self.loaded_from.as_deref()
    }

    /// Create (and initialize) the instance for a backend id if it does not exist yet.
    fn ensure_instance(&mut self, id: u64) -> Result<(), BackendError> {
        if !self.instances.contains_key(&id) {
            let api = self.api.as_ref().ok_or(BackendError::Unavailable)?;
            // SAFETY: create + initialize per Prism's contract; freed in Drop.
            let (be, features) = unsafe {
                let be = (api.registry_create)(self.ctx, id);
                if be.is_null() {
                    return Err(BackendError::Unavailable);
                }
                let r = (api.backend_init)(be);
                if r != PRISM_OK && r != PRISM_ERROR_ALREADY_INITIALIZED {
                    let msg = api.describe(r);
                    (api.backend_free)(be);
                    return Err(BackendError::Failed(msg));
                }
                (be, (api.backend_features)(be))
            };
            self.instances.insert(
                id,
                Instance {
                    be,
                    features,
                    rate: None,
                    volume: None,
                    pitch: None,
                },
            );
        }
        Ok(())
    }

    /// `(api, instance)` for a backend id, borrowing the two fields disjointly.
    fn parts(&mut self, id: u64) -> Result<(&Api, &mut Instance), BackendError> {
        self.ensure_instance(id)?;
        let api = self.api.as_ref().ok_or(BackendError::Unavailable)?;
        let inst = self
            .instances
            .get_mut(&id)
            .ok_or(BackendError::Unavailable)?;
        Ok((api, inst))
    }

    fn apply(api: &Api, inst: &mut Instance, params: &Params) {
        // SAFETY: optional setters on a live backend; failures are ignored (the text still speaks).
        unsafe {
            if let (Some(f), true) = (api.set_rate, inst.features & pf::SUPPORTS_SET_RATE != 0) {
                if inst.rate != Some(params.rate) {
                    let _ = f(inst.be, params.rate.clamp(0.0, 1.0));
                    inst.rate = Some(params.rate);
                }
            }
            if let (Some(f), true) = (api.set_volume, inst.features & pf::SUPPORTS_SET_VOLUME != 0)
            {
                if inst.volume != Some(params.volume) {
                    let _ = f(inst.be, params.volume.clamp(0.0, 1.0));
                    inst.volume = Some(params.volume);
                }
            }
            if let (Some(f), true) = (api.set_pitch, inst.features & pf::SUPPORTS_SET_PITCH != 0) {
                if inst.pitch != Some(params.pitch) {
                    let _ = f(inst.be, params.pitch.clamp(0.0, 1.0));
                    inst.pitch = Some(params.pitch);
                }
            }
        }
    }
}

impl Drop for PrismBackend {
    fn drop(&mut self) {
        if let Some(api) = &self.api {
            // SAFETY: every instance came from registry_create; the context from init.
            unsafe {
                for (_, inst) in self.instances.drain() {
                    (api.backend_free)(inst.be);
                }
                if !self.ctx.is_null() {
                    (api.shutdown)(self.ctx);
                }
            }
        }
    }
}

fn cstring(text: &str) -> Result<CString, BackendError> {
    CString::new(text).map_err(|_| BackendError::Failed("text contains NUL".into()))
}

impl Backend for PrismBackend {
    fn name(&self) -> &'static str {
        "prism"
    }

    fn probe(&mut self) -> Vec<LeafSpec> {
        let Some(api) = self.api.as_ref() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        self.raw_features.clear();
        // SAFETY: registry enumeration per the header; probe instances are freed immediately.
        unsafe {
            let n = (api.registry_count)(self.ctx);
            for i in 0..n {
                let id = (api.registry_id_at)(self.ctx, i);
                let synth_allowed = !cfg!(windows);
                if !READERS.contains(&id) && !synth_allowed {
                    continue;
                }
                let name_ptr = (api.registry_name)(self.ctx, id);
                let name = if name_ptr.is_null() {
                    format!("{id:016x}")
                } else {
                    CStr::from_ptr(name_ptr).to_string_lossy().into_owned()
                };
                let be = (api.registry_create)(self.ctx, id);
                if be.is_null() {
                    continue;
                }
                let raw = (api.backend_features)(be);
                (api.backend_free)(be);
                self.raw_features.insert(id, raw);
                let is_reader = READERS.contains(&id);
                let mut f = features::from_prism(raw);
                if is_reader {
                    f |= features::SCREEN_READER;
                    f &= !features::RENDER; // a reader never renders, whatever its bits say
                }
                out.push(LeafSpec {
                    category: if is_reader {
                        tree::SCREEN_READERS.into()
                    } else {
                        "prism".into()
                    },
                    category_name: if is_reader {
                        None
                    } else {
                        Some("Prism".into())
                    },
                    category_rank: if is_reader { None } else { Some(600) },
                    name: name.clone(),
                    language: String::new(),
                    native_id: format!("{id:016x}"),
                    dedupe: Some(format!("reader:{}", name.to_lowercase())),
                    features: f,
                    selector: id,
                    is_default: false,
                    available: raw & pf::IS_SUPPORTED_AT_RUNTIME != 0,
                });
            }
        }
        out
    }

    fn reprobe_every(&self) -> Option<Duration> {
        if self.api.is_some() {
            Some(Duration::from_secs(10))
        } else {
            None
        }
    }

    fn speak(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        interrupt: bool,
        output: bool,
        _ssml: bool,
    ) -> Result<Slot, BackendError> {
        let c = cstring(text)?;
        let (api, inst) = self.parts(selector)?;
        Self::apply(api, inst, params);
        // SAFETY: live backend, NUL-terminated text.
        let r = unsafe {
            match (output, api.output) {
                (true, Some(f)) if inst.features & pf::SUPPORTS_OUTPUT != 0 => {
                    f(inst.be, c.as_ptr(), interrupt)
                }
                _ => (api.speak)(inst.be, c.as_ptr(), interrupt),
            }
        };
        if r == PRISM_OK {
            Ok(0)
        } else {
            Err(BackendError::Failed(api.describe(r)))
        }
    }

    fn braille(&mut self, selector: u64, text: &str) -> Result<(), BackendError> {
        let c = cstring(text)?;
        let (api, inst) = self.parts(selector)?;
        let Some(f) = api.braille else {
            return Err(BackendError::NotSupported);
        };
        // SAFETY: live backend, NUL-terminated text.
        let r = unsafe { f(inst.be, c.as_ptr()) };
        if r == PRISM_OK {
            Ok(())
        } else {
            Err(BackendError::Failed(api.describe(r)))
        }
    }

    fn stop(&mut self, _slot: Option<Slot>) {
        if let Some(api) = &self.api {
            for inst in self.instances.values() {
                // SAFETY: live backend.
                let _ = unsafe { (api.stop)(inst.be) };
            }
        }
    }

    fn pause(&mut self, _slot: Slot) -> Result<(), BackendError> {
        let api = self.api.as_ref().ok_or(BackendError::Unavailable)?;
        let f = api.pause.ok_or(BackendError::NotSupported)?;
        for inst in self.instances.values() {
            // SAFETY: live backend.
            let _ = unsafe { f(inst.be) };
        }
        Ok(())
    }

    fn resume(&mut self, _slot: Slot) -> Result<(), BackendError> {
        let api = self.api.as_ref().ok_or(BackendError::Unavailable)?;
        let f = api.resume.ok_or(BackendError::NotSupported)?;
        for inst in self.instances.values() {
            // SAFETY: live backend.
            let _ = unsafe { f(inst.be) };
        }
        Ok(())
    }

    fn speaking(&mut self, _slot: Slot) -> Option<bool> {
        let api = self.api.as_ref()?;
        let f = api.is_speaking?;
        let mut any = false;
        let mut known = false;
        for inst in self.instances.values() {
            if inst.features & pf::SUPPORTS_IS_SPEAKING == 0 {
                continue;
            }
            let mut b = false;
            // SAFETY: live backend, valid out pointer.
            if unsafe { f(inst.be, &mut b) } == PRISM_OK {
                known = true;
                any |= b;
            }
        }
        if known { Some(any) } else { None }
    }

    fn poll_interval(&self) -> Option<Duration> {
        if self.api.as_ref()?.is_speaking.is_some() {
            Some(Duration::from_millis(20))
        } else {
            None
        }
    }
}

/// Speak-to-memory through Prism, for platforms where a Prism synthesizer is the only one.
pub struct PrismRender {
    inner: PrismBackend,
}

impl PrismRender {
    pub fn load(explicit: Option<&Path>) -> PrismRender {
        PrismRender {
            inner: PrismBackend::load(explicit),
        }
    }
}

struct Sink {
    pcm: Pcm,
}

unsafe extern "C" fn on_audio(
    user: *mut c_void,
    samples: *const f32,
    count: usize,
    channels: usize,
    rate: usize,
) {
    // SAFETY: `user` is the Sink for this call; Prism hands `count` valid floats.
    let sink = unsafe { &mut *(user as *mut Sink) };
    if samples.is_null() || count == 0 {
        return;
    }
    sink.pcm.channels = channels.max(1) as u32;
    sink.pcm.rate = rate as u32;
    sink.pcm
        .samples
        .extend_from_slice(unsafe { std::slice::from_raw_parts(samples, count) });
}

impl RenderBackend for PrismRender {
    fn name(&self) -> &'static str {
        "prism"
    }

    fn render(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        _ssml: bool,
    ) -> Result<Pcm, BackendError> {
        let c = cstring(text)?;
        let (api, inst) = self.inner.parts(selector)?;
        let f = api.speak_to_memory.ok_or(BackendError::NotSupported)?;
        if inst.features & pf::SUPPORTS_SPEAK_TO_MEMORY == 0 {
            return Err(BackendError::NotSupported);
        }
        PrismBackend::apply(api, inst, params);
        let mut sink = Sink {
            pcm: Pcm::default(),
        };
        // SAFETY: the callback only touches `sink`, which outlives the synchronous call.
        let r = unsafe {
            f(
                inst.be,
                c.as_ptr(),
                on_audio,
                &mut sink as *mut Sink as *mut c_void,
            )
        };
        if r != PRISM_OK {
            return Err(BackendError::Failed(api.describe(r)));
        }
        Ok(sink.pcm)
    }
}
