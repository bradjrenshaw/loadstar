//! The clipboard leaf (speech-spec §5.4): copies the text. The fallback walk's last step, so
//! "something happens" when nothing else can.

#![allow(unsafe_code)]

use windows::Win32::Foundation::{HANDLE, HGLOBAL};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;

use crate::backend::{Backend, BackendError, LeafSpec, Params, Slot};
use crate::features;
use crate::tree;

pub struct ClipboardBackend;

impl Backend for ClipboardBackend {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    fn probe(&mut self) -> Vec<LeafSpec> {
        vec![LeafSpec {
            category: tree::CLIPBOARD.into(),
            category_name: None,
            category_rank: None,
            name: "Clipboard".into(),
            language: String::new(),
            native_id: "clipboard".into(),
            dedupe: None,
            features: features::SPEAK,
            selector: 0,
            is_default: true,
            available: true,
        }]
    }

    fn speak(
        &mut self,
        _selector: u64,
        text: &str,
        _params: &Params,
        _interrupt: bool,
        _output: bool,
        _ssml: bool,
    ) -> Result<Slot, BackendError> {
        copy(text).map(|_| 0)
    }

    fn braille(&mut self, _selector: u64, _text: &str) -> Result<(), BackendError> {
        Err(BackendError::NotSupported)
    }

    fn stop(&mut self, _slot: Option<Slot>) {}

    fn speaking(&mut self, _slot: Slot) -> Option<bool> {
        None
    }
}

fn copy(text: &str) -> Result<(), BackendError> {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: plain Win32 clipboard protocol; the global block is owned by the clipboard after
    // SetClipboardData succeeds and freed by us only on failure.
    unsafe {
        OpenClipboard(None).map_err(|e| BackendError::Failed(e.to_string()))?;
        let result = (|| -> Result<(), BackendError> {
            EmptyClipboard().map_err(|e| BackendError::Failed(e.to_string()))?;
            let bytes = wide.len() * 2;
            let h: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)
                .map_err(|e| BackendError::Failed(e.to_string()))?;
            let p = GlobalLock(h) as *mut u16;
            if p.is_null() {
                return Err(BackendError::Failed("GlobalLock".into()));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), p, wide.len());
            let _ = GlobalUnlock(h);
            SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(h.0)))
                .map_err(|e| BackendError::Failed(e.to_string()))?;
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}
