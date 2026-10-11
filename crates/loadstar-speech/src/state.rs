//! The bounded utterance table (speech-spec §2): the whole feedback channel, no callbacks. A
//! fixed ring of the most recent utterances, written by the worker, read by the game thread
//! through atomics; anything older than the ring answers `Unknown`.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UtteranceState {
    Pending = 0,
    Speaking = 1,
    Done = 2,
    Cancelled = 3,
    /// Not issued by this context, or older than the table remembers.
    Unknown = 4,
}

impl UtteranceState {
    pub fn from_u8(v: u8) -> UtteranceState {
        match v & 0x0f {
            0 => UtteranceState::Pending,
            1 => UtteranceState::Speaking,
            2 => UtteranceState::Done,
            3 => UtteranceState::Cancelled,
            _ => UtteranceState::Unknown,
        }
    }
}

/// Set on a positional utterance that was spoken flat (`SPEAK_FLAT`), so the mod can add its own cue.
pub const FLAT: u8 = 0x10;

pub struct UtteranceTable {
    ids: Vec<AtomicU64>,
    states: Vec<AtomicU8>,
    voices: Vec<AtomicU64>,
    next: AtomicU64,
}

impl UtteranceTable {
    pub fn new(capacity: usize) -> UtteranceTable {
        let capacity = capacity.max(16);
        UtteranceTable {
            ids: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            states: (0..capacity)
                .map(|_| AtomicU8::new(UtteranceState::Unknown as u8))
                .collect(),
            voices: (0..capacity).map(|_| AtomicU64::new(0)).collect(),
            next: AtomicU64::new(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.ids.len()
    }

    fn slot(&self, id: u64) -> usize {
        (id % self.ids.len() as u64) as usize
    }

    /// Issue a new id in `Pending`.
    pub fn alloc(&self) -> u64 {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let s = self.slot(id);
        self.states[s].store(UtteranceState::Pending as u8, Ordering::Relaxed);
        self.voices[s].store(0, Ordering::Relaxed);
        self.ids[s].store(id, Ordering::Release);
        id
    }

    /// Issue an id already in a final state (empty text).
    pub fn alloc_done(&self) -> u64 {
        let id = self.alloc();
        self.set(id, UtteranceState::Done, false);
        id
    }

    pub fn set(&self, id: u64, state: UtteranceState, flat: bool) {
        let s = self.slot(id);
        if self.ids[s].load(Ordering::Acquire) != id {
            return; // already rolled out of the ring
        }
        let prev = self.states[s].load(Ordering::Relaxed);
        self.states[s].store(
            state as u8 | (prev & FLAT) | if flat { FLAT } else { 0 },
            Ordering::Release,
        );
    }

    pub fn set_voice(&self, id: u64, voice: u64) {
        let s = self.slot(id);
        if self.ids[s].load(Ordering::Acquire) == id {
            self.voices[s].store(voice, Ordering::Release);
        }
    }

    /// `(state, flat, voice)`.
    pub fn get(&self, id: u64) -> (UtteranceState, bool, u64) {
        let s = self.slot(id);
        if id == 0 || self.ids[s].load(Ordering::Acquire) != id {
            return (UtteranceState::Unknown, false, 0);
        }
        let raw = self.states[s].load(Ordering::Acquire);
        (
            UtteranceState::from_u8(raw),
            raw & FLAT != 0,
            self.voices[s].load(Ordering::Acquire),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_remembers_recent_ids_and_forgets_old_ones() {
        let t = UtteranceTable::new(16);
        let a = t.alloc();
        t.set(a, UtteranceState::Speaking, false);
        assert_eq!(t.get(a), (UtteranceState::Speaking, false, 0));
        t.set_voice(a, 77);
        t.set(a, UtteranceState::Done, true);
        assert_eq!(t.get(a), (UtteranceState::Done, true, 77));
        for _ in 0..16 {
            t.alloc();
        }
        assert_eq!(t.get(a).0, UtteranceState::Unknown, "rolled out");
        assert_eq!(t.get(0).0, UtteranceState::Unknown);
        assert_eq!(t.get(t.alloc_done()).0, UtteranceState::Done);
    }
}
