//! What crosses from the game thread to the audio thread (spec §3, §4.2). Everything that
//! needs allocation — chains, buses, spaces, snapshots, buffers — is built on the game thread
//! and moved; the audio thread swaps things into place and returns what it displaced as
//! [`Garbage`] for the game thread to drop.

use std::any::Any;
use std::sync::Arc;

use crate::chain::Chain;
use crate::space::{Buffer, Bus, Listener, SourceDesc, Space, Stage};

/// Which chain an effect operation addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainTarget {
    Master,
    Space(u32),
    Bus(u32, u32),
    Source(u32, u64, Stage),
}

pub enum Command {
    SpaceCreate { id: u32, space: Box<Space> },
    SpaceDestroy(u32),
    SpaceGain { space: u32, gain: f32 },
    Listener { space: u32, listener: Listener },
    BusCreate { space: u32, bus: Box<Bus> },
    BusGain { space: u32, bus: u32, gain: f32 },
    Sync { space: u32, descs: Vec<SourceDesc> },
    Upsert { space: u32, desc: SourceDesc },
    Remove { space: u32, id: u64 },
    Fire { space: u32, desc: SourceDesc },
    Stop { space: u32, id: u64 },
    SetChain { target: ChainTarget, chain: Box<Chain> },
    SetEffectParam { target: ChainTarget, index: usize, param: usize, value: f32 },
    SetEffectEnabled { target: ChainTarget, index: usize, enabled: bool },
    BufferAdd { id: u32, buffer: Arc<Buffer> },
    BufferFree(u32),
    MasterGain(f32),
}

/// Anything the audio thread no longer needs, dropped on the game thread.
pub type Garbage = Box<dyn Any + Send>;
