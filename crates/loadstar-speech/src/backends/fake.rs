//! A deterministic backend for tests and harnesses: scripted leaves, a shared log of every call,
//! and utterances that finish when the test says so (or after a fixed number of polls).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::backend::{Backend, BackendError, LeafSpec, Params, Pcm, RenderBackend, Slot};
use crate::features;
use crate::tree;

#[derive(Clone, Debug, PartialEq)]
pub enum Call {
    Probe,
    Speak {
        selector: u64,
        text: String,
        interrupt: bool,
        output: bool,
        slot: Slot,
        rate: f32,
    },
    Braille {
        selector: u64,
        text: String,
    },
    Stop(Option<Slot>),
    Pause(Slot),
    Resume(Slot),
    Release(Slot),
    Render {
        selector: u64,
        text: String,
    },
}

#[derive(Default)]
pub struct Shared {
    pub calls: Vec<Call>,
    /// Slots still speaking; a test removes one to "finish" it.
    pub speaking: HashMap<Slot, bool>,
    /// Polls remaining before a slot auto-finishes (None = until told).
    pub auto_finish_after: Option<u32>,
    polls: HashMap<Slot, u32>,
    pub available: bool,
}

pub struct FakeBackend {
    name: &'static str,
    leaves: Vec<LeafSpec>,
    knows_state: bool,
    multi: bool,
    next_slot: Slot,
    pub shared: Arc<Mutex<Shared>>,
}

impl FakeBackend {
    /// A reader-like backend: one channel, can't report state.
    pub fn reader(name: &'static str, running: bool) -> FakeBackend {
        let mut f = FakeBackend::new(name, false, false);
        f.leaves.push(LeafSpec {
            category: tree::SCREEN_READERS.into(),
            category_name: None,
            category_rank: None,
            name: name.to_uppercase(),
            language: String::new(),
            native_id: name.into(),
            dedupe: Some(format!("reader:{name}")),
            features: features::SPEAK
                | features::OUTPUT
                | features::BRAILLE
                | features::STOP
                | features::SCREEN_READER,
            selector: 1,
            is_default: false,
            available: running,
        });
        f.shared.lock().unwrap().available = running;
        f
    }

    /// A synthesizer-like backend: many instances, reports state, renders.
    pub fn synth(name: &'static str, voices: &[&str]) -> FakeBackend {
        let mut f = FakeBackend::new(name, true, true);
        for (i, v) in voices.iter().enumerate() {
            f.leaves.push(LeafSpec {
                category: tree::SAPI5.into(),
                category_name: None,
                category_rank: None,
                name: v.to_string(),
                language: "en-US".into(),
                native_id: v.to_lowercase(),
                dedupe: None,
                features: features::SPEAK
                    | features::STOP
                    | features::IS_SPEAKING
                    | features::SET_RATE
                    | features::SET_VOLUME
                    | features::RENDER
                    | features::MULTI_INSTANCE,
                selector: i as u64 + 1,
                is_default: i == 0,
                available: true,
            });
        }
        f.shared.lock().unwrap().available = true;
        f
    }

    fn new(name: &'static str, knows_state: bool, multi: bool) -> FakeBackend {
        FakeBackend {
            name,
            leaves: Vec::new(),
            knows_state,
            multi,
            next_slot: 0,
            shared: Arc::default(),
        }
    }

    pub fn log(&self) -> Vec<Call> {
        self.shared.lock().unwrap().calls.clone()
    }
}

impl Backend for FakeBackend {
    fn name(&self) -> &'static str {
        self.name
    }

    fn probe(&mut self) -> Vec<LeafSpec> {
        let mut s = self.shared.lock().unwrap();
        s.calls.push(Call::Probe);
        let available = s.available;
        self.leaves
            .iter()
            .cloned()
            .map(|mut l| {
                l.available = available;
                l
            })
            .collect()
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
        let mut s = self.shared.lock().unwrap();
        if !s.available {
            return Err(BackendError::Unavailable);
        }
        let slot = if self.multi {
            self.next_slot += 1;
            self.next_slot
        } else {
            0
        };
        s.calls.push(Call::Speak {
            selector,
            text: text.to_string(),
            interrupt,
            output,
            slot,
            rate: params.rate,
        });
        s.speaking.insert(slot, true);
        s.polls.insert(slot, 0);
        Ok(slot)
    }

    fn braille(&mut self, selector: u64, text: &str) -> Result<(), BackendError> {
        self.shared.lock().unwrap().calls.push(Call::Braille {
            selector,
            text: text.to_string(),
        });
        Ok(())
    }

    fn stop(&mut self, slot: Option<Slot>) {
        let mut s = self.shared.lock().unwrap();
        s.calls.push(Call::Stop(slot));
        match slot {
            Some(k) => {
                s.speaking.remove(&k);
            }
            None => s.speaking.clear(),
        }
    }

    fn pause(&mut self, slot: Slot) -> Result<(), BackendError> {
        self.shared.lock().unwrap().calls.push(Call::Pause(slot));
        Ok(())
    }

    fn resume(&mut self, slot: Slot) -> Result<(), BackendError> {
        self.shared.lock().unwrap().calls.push(Call::Resume(slot));
        Ok(())
    }

    fn speaking(&mut self, slot: Slot) -> Option<bool> {
        if !self.knows_state {
            return None;
        }
        let mut s = self.shared.lock().unwrap();
        if let Some(limit) = s.auto_finish_after {
            let n = s.polls.entry(slot).or_insert(0);
            *n += 1;
            if *n >= limit {
                s.speaking.remove(&slot);
            }
        }
        Some(s.speaking.contains_key(&slot))
    }

    fn poll_interval(&self) -> Option<Duration> {
        if self.knows_state {
            Some(Duration::from_millis(1))
        } else {
            None
        }
    }

    fn release(&mut self, slot: Slot) {
        self.shared.lock().unwrap().calls.push(Call::Release(slot));
    }
}

/// The fake's render side: a short constant tone whose length encodes the text length.
pub struct FakeRender {
    pub shared: Arc<Mutex<Shared>>,
    pub rate: u32,
}

impl RenderBackend for FakeRender {
    fn name(&self) -> &'static str {
        "fake"
    }

    fn render(
        &mut self,
        selector: u64,
        text: &str,
        _params: &Params,
        _ssml: bool,
    ) -> Result<Pcm, BackendError> {
        self.shared.lock().unwrap().calls.push(Call::Render {
            selector,
            text: text.to_string(),
        });
        let frames = (text.len().max(1) * (self.rate as usize / 100)).max(1); // 10 ms per character
        Ok(Pcm {
            samples: (0..frames)
                .map(|i| ((i as f32) * 0.1).sin() * 0.5)
                .collect(),
            channels: 1,
            rate: self.rate,
        })
    }
}
