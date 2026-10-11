//! SAPI 5 through COM (speech-spec §5.1): one `ISpVoice` per voice, more created on demand when a
//! queue asks for overlap, asynchronous speech with purge-to-interrupt, and render-to-memory on
//! separate instances (the render thread) so rendering never cuts or is cut by live speech.

#![allow(unsafe_code)]

use std::collections::HashMap;
use std::ffi::c_void;
use std::time::Duration;

use windows::Win32::Media::Audio::{WAVE_FORMAT_PCM, WAVEFORMATEX};
use windows::Win32::Media::Speech::{
    ISpObjectToken, ISpObjectTokenCategory, ISpStream, ISpVoice, SPCAT_VOICES, SPF_ASYNC,
    SPF_IS_NOT_XML, SPF_IS_XML, SPF_PURGEBEFORESPEAK, SPRS_DONE, SPVOICESTATUS, SpObjectToken,
    SpObjectTokenCategory, SpStream, SpVoice,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree, IStream,
    STATFLAG_NONAME, STATSTG, STREAM_SEEK_SET,
};
use windows::Win32::System::Com::{
    ISequentialStream_Impl, IStream_Impl, LOCKTYPE, STATFLAG, STGC, STREAM_SEEK,
};
use windows::Win32::UI::Shell::SHCreateMemStream;
use windows::core::{GUID, HRESULT, IUnknown, PCWSTR, PWSTR, implement, w};

use crate::backend::{
    Backend, BackendError, LeafSpec, Params, Pcm, RenderBackend, Slot, StreamSink,
};
use crate::features;
use crate::ids;
use crate::tree;

/// `SPDFID_WaveFormatEx` — the format-id GUID SAPI uses for raw PCM streams (not in the windows crate).
const SPDFID_WAVEFORMATEX: GUID = GUID::from_u128(0xc31adbae_527f_4ff5_a230_f62bb61ff70c);
const RENDER_RATE: u32 = 22050;

/// Join COM's multithreaded apartment on this thread (idempotent; a different mode already set
/// is left alone — SAPI works from either).
pub fn com_init() {
    // SAFETY: documented COM init; the result is informational here.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

struct Token {
    id: String,
    name: String,
    language: String,
    selector: u64,
    is_default: bool,
}

fn take_pwstr(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    // SAFETY: a CoTaskMem string SAPI handed us; we own it.
    unsafe {
        let s = p.to_string().unwrap_or_default();
        CoTaskMemFree(Some(p.0 as *const c_void));
        s
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// A SAPI `Language` attribute (hex LCIDs, `;`-separated) → a BCP-47 tag for the first one.
fn lcid_tag(attr: &str) -> String {
    let first = attr.split(';').next().unwrap_or("").trim();
    match u32::from_str_radix(first, 16).unwrap_or(0) {
        0x409 => "en-US",
        0x809 => "en-GB",
        0xc09 => "en-AU",
        0x1009 => "en-CA",
        0x4009 => "en-IN",
        0x407 => "de-DE",
        0x40c => "fr-FR",
        0xc0c => "fr-CA",
        0xc0a | 0x40a => "es-ES",
        0x80a => "es-MX",
        0x410 => "it-IT",
        0x416 => "pt-BR",
        0x816 => "pt-PT",
        0x411 => "ja-JP",
        0x412 => "ko-KR",
        0x804 => "zh-CN",
        0x404 => "zh-TW",
        0xc04 => "zh-HK",
        0x419 => "ru-RU",
        0x415 => "pl-PL",
        0x413 => "nl-NL",
        0x41d => "sv-SE",
        0x406 => "da-DK",
        0x414 => "nb-NO",
        0x40b => "fi-FI",
        0x41f => "tr-TR",
        0x40e => "hu-HU",
        0x405 => "cs-CZ",
        0x408 => "el-GR",
        0x40d => "he-IL",
        0x401 => "ar-SA",
        0x439 => "hi-IN",
        0x41e => "th-TH",
        0x42a => "vi-VN",
        0x421 => "id-ID",
        _ => "",
    }
    .to_string()
}

fn enumerate() -> windows::core::Result<Vec<Token>> {
    // SAFETY: the documented token-category enumeration; every out string is freed by take_pwstr.
    unsafe {
        let cat: ISpObjectTokenCategory =
            CoCreateInstance(&SpObjectTokenCategory, None, CLSCTX_ALL)?;
        cat.SetId(SPCAT_VOICES, false)?;
        let default = cat.GetDefaultTokenId().map(take_pwstr).unwrap_or_default();
        let e = cat.EnumTokens(PCWSTR::null(), PCWSTR::null())?;
        let mut out = Vec::new();
        loop {
            let mut tok: Option<ISpObjectToken> = None;
            let mut fetched = 0u32;
            if e.Next(1, &mut tok, Some(&mut fetched)).is_err() || fetched == 0 {
                break;
            }
            let Some(t) = tok else { break };
            let id = t.GetId().map(take_pwstr).unwrap_or_default();
            if id.is_empty() {
                continue;
            }
            let name = t
                .GetStringValue(PCWSTR::null())
                .map(take_pwstr)
                .unwrap_or_else(|_| id.clone());
            let language = t
                .OpenKey(w!("Attributes"))
                .and_then(|k| k.GetStringValue(w!("Language")))
                .map(take_pwstr)
                .map(|l| lcid_tag(&l))
                .unwrap_or_default();
            out.push(Token {
                selector: ids::hash(&id),
                is_default: id == default,
                id,
                name,
                language,
            });
        }
        Ok(out)
    }
}

fn make_voice(token_id: &str) -> Result<ISpVoice, BackendError> {
    // SAFETY: documented object creation; the token id came from enumeration.
    unsafe {
        let v: ISpVoice = CoCreateInstance(&SpVoice, None, CLSCTX_ALL).map_err(fail)?;
        let tok: ISpObjectToken =
            CoCreateInstance(&SpObjectToken, None, CLSCTX_ALL).map_err(fail)?;
        let id = wide(token_id);
        tok.SetId(PCWSTR::null(), PCWSTR(id.as_ptr()), false)
            .map_err(fail)?;
        v.SetVoice(&tok).map_err(fail)?;
        Ok(v)
    }
}

fn fail(e: windows::core::Error) -> BackendError {
    BackendError::Failed(e.to_string())
}

fn apply(voice: &ISpVoice, params: &Params) {
    // SAFETY: live voice; both setters are cheap and failures are not worth failing the utterance.
    unsafe {
        let _ = voice.SetRate(((params.rate.clamp(0.0, 1.0) * 20.0) - 10.0).round() as i32);
        let _ = voice.SetVolume((params.volume.clamp(0.0, 1.0) * 100.0).round() as u16);
    }
}

const LEAF_FEATURES: u64 = features::SPEAK
    | features::STOP
    | features::PAUSE
    | features::IS_SPEAKING
    | features::SET_RATE
    | features::SET_VOLUME
    | features::RENDER
    | features::SSML
    | features::MULTI_INSTANCE;

struct Instance {
    voice: ISpVoice,
    selector: u64,
    busy: bool,
    stream: u32,
}

/// Live SAPI speech (worker thread).
pub struct SapiBackend {
    tokens: Vec<Token>,
    instances: Vec<Instance>,
}

// ISpVoice is a COM pointer used only from the thread that owns this struct.
unsafe impl Send for SapiBackend {}

impl SapiBackend {
    pub fn new() -> SapiBackend {
        SapiBackend {
            tokens: Vec::new(),
            instances: Vec::new(),
        }
    }

    fn token(&self, selector: u64) -> Option<&Token> {
        self.tokens.iter().find(|t| t.selector == selector)
    }

    fn free_instance(&mut self, selector: u64) -> Result<usize, BackendError> {
        if let Some(i) = self
            .instances
            .iter()
            .position(|i| i.selector == selector && !i.busy)
        {
            return Ok(i);
        }
        let id = self
            .token(selector)
            .ok_or(BackendError::Unavailable)?
            .id
            .clone();
        let voice = make_voice(&id)?;
        self.instances.push(Instance {
            voice,
            selector,
            busy: false,
            stream: 0,
        });
        Ok(self.instances.len() - 1)
    }
}

impl Default for SapiBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Backend for SapiBackend {
    fn name(&self) -> &'static str {
        "sapi"
    }

    fn probe(&mut self) -> Vec<LeafSpec> {
        com_init();
        self.tokens = enumerate().unwrap_or_default();
        self.tokens
            .iter()
            .map(|t| LeafSpec {
                category: tree::SAPI5.into(),
                category_name: None,
                category_rank: None,
                name: t.name.clone(),
                language: t.language.clone(),
                native_id: t.id.clone(),
                dedupe: None,
                features: LEAF_FEATURES,
                selector: t.selector,
                is_default: t.is_default,
                available: true,
            })
            .collect()
    }

    fn speak(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        interrupt: bool,
        _output: bool,
        ssml: bool,
    ) -> Result<Slot, BackendError> {
        let idx = self.free_instance(selector)?;
        let inst = &mut self.instances[idx];
        apply(&inst.voice, params);
        let mut flags = SPF_ASYNC.0 | if ssml { SPF_IS_XML.0 } else { SPF_IS_NOT_XML.0 };
        if interrupt {
            flags |= SPF_PURGEBEFORESPEAK.0;
        }
        let w = wide(text);
        let mut stream = 0u32;
        // SAFETY: live voice, NUL-terminated UTF-16 text that outlives the call (SAPI copies it).
        unsafe {
            inst.voice
                .Speak(PCWSTR(w.as_ptr()), flags as u32, Some(&mut stream))
        }
        .map_err(fail)?;
        inst.busy = true;
        inst.stream = stream;
        Ok(idx as Slot)
    }

    fn braille(&mut self, _selector: u64, _text: &str) -> Result<(), BackendError> {
        Err(BackendError::NotSupported)
    }

    fn stop(&mut self, slot: Option<Slot>) {
        let range: Vec<usize> = match slot {
            Some(s) => vec![s as usize],
            None => (0..self.instances.len()).collect(),
        };
        for i in range {
            if let Some(inst) = self.instances.get_mut(i) {
                // SAFETY: the documented purge: Speak(NULL, PURGE).
                let _ = unsafe {
                    inst.voice.Speak(
                        PCWSTR::null(),
                        (SPF_PURGEBEFORESPEAK.0 | SPF_ASYNC.0) as u32,
                        None,
                    )
                };
                inst.busy = false;
            }
        }
    }

    fn pause(&mut self, slot: Slot) -> Result<(), BackendError> {
        let inst = self
            .instances
            .get(slot as usize)
            .ok_or(BackendError::Unavailable)?;
        // SAFETY: live voice.
        unsafe { inst.voice.Pause() }.map_err(fail)
    }

    fn resume(&mut self, slot: Slot) -> Result<(), BackendError> {
        let inst = self
            .instances
            .get(slot as usize)
            .ok_or(BackendError::Unavailable)?;
        // SAFETY: live voice.
        unsafe { inst.voice.Resume() }.map_err(fail)
    }

    fn speaking(&mut self, slot: Slot) -> Option<bool> {
        let inst = self.instances.get(slot as usize)?;
        let mut st = SPVOICESTATUS::default();
        // SAFETY: live voice, valid out struct, no bookmark requested.
        unsafe { inst.voice.GetStatus(&mut st, std::ptr::null_mut()) }.ok()?;
        let done = st.dwRunningState == SPRS_DONE.0 as u32 && st.ulCurrentStream >= inst.stream;
        Some(!done)
    }

    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_millis(15))
    }

    fn release(&mut self, slot: Slot) {
        if let Some(inst) = self.instances.get_mut(slot as usize) {
            inst.busy = false;
        }
    }
}

/// SAPI speak-to-memory (render thread): its own voices, never the live ones.
pub struct SapiRender {
    tokens: Vec<Token>,
    voices: HashMap<u64, ISpVoice>,
    probed: bool,
}

unsafe impl Send for SapiRender {}

impl SapiRender {
    pub fn new() -> SapiRender {
        SapiRender {
            tokens: Vec::new(),
            voices: HashMap::new(),
            probed: false,
        }
    }
}

impl Default for SapiRender {
    fn default() -> Self {
        Self::new()
    }
}

/// The IStream SAPI writes into when rendering: every `Write` forwards the PCM16 chunk to the
/// sink as f32, on SAPI's synthesis thread, while `Speak` blocks the render thread. SAPI only
/// writes and asks for the position; the rest of the interface is accepted and ignored.
#[implement(windows::Win32::System::Com::IStream)]
struct ForwardingStream {
    sink: std::sync::Arc<dyn StreamSink>,
    rate: u32,
    state: std::sync::Mutex<(u64, bool)>, // (bytes written, consumer gone)
    /// Set `LOADSTAR_SPEECH_TRACE` to log every chunk SAPI hands over (size, time since creation).
    trace: Option<std::time::Instant>,
}

impl ISequentialStream_Impl for ForwardingStream_Impl {
    fn Read(&self, _pv: *mut c_void, _cb: u32, _pcbread: *mut u32) -> HRESULT {
        HRESULT(0x80004001u32 as i32) // E_NOTIMPL
    }

    fn Write(&self, pv: *const c_void, cb: u32, pcbwritten: *mut u32) -> HRESULT {
        // SAFETY: SAPI hands us `cb` readable bytes.
        let bytes = unsafe { std::slice::from_raw_parts(pv as *const u8, cb as usize) };
        let mut st = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let start = if st.0 == 0 && bytes.len() > 44 && &bytes[..4] == b"RIFF" {
            44
        } else {
            0
        };
        let samples: Vec<f32> = bytes[start..]
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
            .collect();
        st.0 += cb as u64;
        if let Some(t0) = self.trace {
            eprintln!(
                "[sapi-stream] +{:?}: {} bytes ({:.1} ms of audio), total {} bytes",
                t0.elapsed(),
                cb,
                samples.len() as f32 * 1000.0 / self.rate as f32,
                st.0
            );
        }
        if !st.1 && !self.sink.write(&samples, 1, self.rate) {
            st.1 = true; // consumer gone: keep accepting so SAPI finishes quietly, write nowhere
        }
        if !pcbwritten.is_null() {
            // SAFETY: SAPI passes a valid out pointer or null.
            unsafe { *pcbwritten = cb };
        }
        HRESULT(0)
    }
}

impl IStream_Impl for ForwardingStream_Impl {
    fn Seek(
        &self,
        _dlibmove: i64,
        _dworigin: STREAM_SEEK,
        plibnewposition: *mut u64,
    ) -> windows::core::Result<()> {
        if !plibnewposition.is_null() {
            // SAFETY: valid out pointer or null.
            unsafe { *plibnewposition = self.state.lock().unwrap_or_else(|e| e.into_inner()).0 };
        }
        Ok(())
    }
    fn SetSize(&self, _libnewsize: u64) -> windows::core::Result<()> {
        Ok(())
    }
    fn CopyTo(
        &self,
        _pstm: windows::core::Ref<'_, IStream>,
        _cb: u64,
        _pcbread: *mut u64,
        _pcbwritten: *mut u64,
    ) -> windows::core::Result<()> {
        Err(windows::core::Error::from_hresult(HRESULT(
            0x80004001u32 as i32,
        )))
    }
    fn Commit(&self, _grfcommitflags: &STGC) -> windows::core::Result<()> {
        Ok(())
    }
    fn Revert(&self) -> windows::core::Result<()> {
        Ok(())
    }
    fn LockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: &LOCKTYPE,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn UnlockRegion(
        &self,
        _liboffset: u64,
        _cb: u64,
        _dwlocktype: u32,
    ) -> windows::core::Result<()> {
        Ok(())
    }
    fn Stat(&self, pstatstg: *mut STATSTG, _grfstatflag: &STATFLAG) -> windows::core::Result<()> {
        if !pstatstg.is_null() {
            // SAFETY: valid out struct.
            unsafe {
                let mut st = STATSTG::default();
                st.cbSize = self.state.lock().unwrap_or_else(|e| e.into_inner()).0;
                *pstatstg = st;
            }
        }
        Ok(())
    }
    fn Clone(&self) -> windows::core::Result<IStream> {
        Err(windows::core::Error::from_hresult(HRESULT(
            0x80004001u32 as i32,
        )))
    }
}

impl SapiRender {
    fn voice_for(&mut self, selector: u64) -> Result<&ISpVoice, BackendError> {
        if !self.probed {
            com_init();
            self.tokens = enumerate().unwrap_or_default();
            self.probed = true;
        }
        if !self.voices.contains_key(&selector) {
            let id = self
                .tokens
                .iter()
                .find(|t| t.selector == selector)
                .ok_or(BackendError::Unavailable)?
                .id
                .clone();
            self.voices.insert(selector, make_voice(&id)?);
        }
        Ok(&self.voices[&selector])
    }

    /// Bind `voice`'s output to `target` in our PCM format, speak synchronously, unbind.
    fn speak_into(
        voice: &ISpVoice,
        target: &IStream,
        text: &str,
        params: &Params,
        ssml: bool,
    ) -> Result<(), BackendError> {
        // SAFETY: documented SAPI render-to-stream sequence; every object is released by drop.
        unsafe {
            let stream: ISpStream = CoCreateInstance(&SpStream, None, CLSCTX_ALL).map_err(fail)?;
            let wfx = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 1,
                nSamplesPerSec: RENDER_RATE,
                nAvgBytesPerSec: RENDER_RATE * 2,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            stream
                .SetBaseStream(target, &SPDFID_WAVEFORMATEX, &wfx)
                .map_err(fail)?;
            voice.SetOutput(&stream, false).map_err(fail)?;
            apply(voice, params);
            let w = wide(text);
            let flags = if ssml { SPF_IS_XML.0 } else { SPF_IS_NOT_XML.0 } as u32;
            let spoke = voice.Speak(PCWSTR(w.as_ptr()), flags, None); // synchronous: data complete on return
            let _ = voice.SetOutput(None::<&IUnknown>, true);
            let _ = stream.Close();
            spoke.map_err(fail)
        }
    }
}

impl RenderBackend for SapiRender {
    fn name(&self) -> &'static str {
        "sapi"
    }

    fn warm(&mut self, selector: u64) {
        let Ok(voice) = self.voice_for(selector).cloned() else {
            return;
        };
        // SAFETY: a throwaway memory stream; the render is a single space.
        unsafe {
            if let Some(mem) = SHCreateMemStream(None) {
                let _ = Self::speak_into(&voice, &mem, " ", &Params::default(), false);
            }
        }
    }

    fn render_streaming(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        ssml: bool,
        sink: std::sync::Arc<dyn StreamSink>,
    ) -> Result<(), BackendError> {
        let voice = self.voice_for(selector)?.clone();
        let target: IStream = ForwardingStream {
            sink,
            rate: RENDER_RATE,
            state: std::sync::Mutex::new((0, false)),
            trace: std::env::var_os("LOADSTAR_SPEECH_TRACE").map(|_| std::time::Instant::now()),
        }
        .into();
        Self::speak_into(&voice, &target, text, params, ssml)
    }

    fn render(
        &mut self,
        selector: u64,
        text: &str,
        params: &Params,
        ssml: bool,
    ) -> Result<Pcm, BackendError> {
        let voice = self.voice_for(selector)?.clone();
        // SAFETY: a memory stream we own; read back after the synchronous speak.
        unsafe {
            let mem: IStream = SHCreateMemStream(None)
                .ok_or_else(|| BackendError::Failed("SHCreateMemStream".into()))?;
            Self::speak_into(&voice, &mem, text, params, ssml)?;
            mem.Seek(0, STREAM_SEEK_SET, None).map_err(fail)?;
            let mut stat = STATSTG::default();
            mem.Stat(&mut stat, STATFLAG_NONAME).map_err(fail)?;
            let len = stat.cbSize as usize;
            let mut bytes = vec![0u8; len];
            let mut read = 0u32;
            mem.Read(
                bytes.as_mut_ptr() as *mut c_void,
                len as u32,
                Some(&mut read),
            )
            .ok()
            .map_err(fail)?;
            bytes.truncate(read as usize);
            let start = if bytes.len() > 44 && &bytes[..4] == b"RIFF" {
                44
            } else {
                0
            };
            let samples: Vec<f32> = bytes[start..]
                .chunks_exact(2)
                .map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0)
                .collect();
            Ok(Pcm {
                samples,
                channels: 1,
                rate: RENDER_RATE,
            })
        }
    }
}
