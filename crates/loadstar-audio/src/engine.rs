//! Audio-thread state (spec §4.7, §5): buffers, spaces, the master chain and the voice cap.
//! [`Engine::process`] is the period callback; [`Engine::run`] applies commands at the start
//! of a period. Nothing here allocates or frees in steady state: displaced objects travel back
//! to the game thread in the very boxes the commands arrived in, and what voices release goes
//! through [`Engine::drain_garbage`].

use std::sync::Arc;

use crate::chain::Chain;
use crate::commands::{ChainTarget, Command, Garbage};
use crate::dsp::Ramp;
use crate::space::{Buffer, Space};
use crate::stream::StreamSource;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineConfig {
    pub sample_rate: u32,
    pub block_frames: usize,
    pub master_gain: f32,
    pub voice_cap: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig {
            sample_rate: 48000,
            block_frames: 128,
            master_gain: 1.0,
            voice_cap: 64,
        }
    }
}

pub struct Engine {
    cfg: EngineConfig,
    spaces: Vec<Option<Box<Space>>>,
    buffers: Vec<Option<Arc<Buffer>>>,
    /// Streaming sources waiting for the voice that will take them.
    streams: Vec<Option<StreamSource>>,
    master_gain: Ramp,
    master: Chain,
    /// Garbage the ring refused last time, retried next period rather than dropped here.
    garbage: Vec<Garbage>,
    audible: Vec<(u32, usize, u8, f32)>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Engine {
        let cfg = EngineConfig {
            block_frames: cfg.block_frames.max(1),
            voice_cap: if cfg.voice_cap == 0 {
                64
            } else {
                cfg.voice_cap
            },
            ..cfg
        };
        Engine {
            cfg,
            spaces: Vec::with_capacity(8),
            buffers: Vec::with_capacity(64),
            streams: Vec::with_capacity(16),
            master_gain: Ramp::new(cfg.master_gain),
            master: Chain::empty(cfg.sample_rate as f32, 2, cfg.block_frames),
            garbage: Vec::with_capacity(64),
            audible: Vec::with_capacity(256),
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.cfg
    }

    fn space_mut(&mut self, id: u32) -> Option<&mut Space> {
        self.spaces
            .get_mut(id as usize)
            .and_then(|s| s.as_deref_mut())
    }

    fn chain_mut(&mut self, target: ChainTarget) -> Option<&mut Chain> {
        match target {
            ChainTarget::Master => Some(&mut self.master),
            ChainTarget::Space(s) => self.space_mut(s).map(Space::chain_mut),
            ChainTarget::Bus(s, b) => self.space_mut(s).and_then(|sp| sp.bus_chain_mut(b)),
            ChainTarget::Source(s, id, stage) => self
                .space_mut(s)
                .and_then(|sp| sp.source_chain_mut(id, stage)),
        }
    }

    /// Apply one command. Returns anything displaced that must be dropped off the audio thread —
    /// always carried in an allocation the command itself brought, never one made here.
    pub fn apply(&mut self, cmd: Command) -> Option<Garbage> {
        match cmd {
            Command::SpaceCreate { id, mut space } => {
                let idx = id as usize;
                if self.spaces.len() <= idx {
                    self.spaces.resize_with(idx + 1, || None);
                }
                match &mut self.spaces[idx] {
                    Some(old) => {
                        std::mem::swap(old, &mut space);
                        Some(Garbage::Any(space))
                    }
                    slot @ None => {
                        *slot = Some(space);
                        None
                    }
                }
            }
            Command::SpaceDestroy(id) => self
                .spaces
                .get_mut(id as usize)
                .and_then(Option::take)
                .map(|s| Garbage::Any(s)),
            Command::SpaceGain { space, gain } => {
                if let Some(s) = self.space_mut(space) {
                    s.set_gain(gain);
                }
                None
            }
            Command::Listener { space, listener } => {
                if let Some(s) = self.space_mut(space) {
                    s.set_listener(listener);
                }
                None
            }
            Command::BusCreate { space, bus } => match self.space_mut(space) {
                Some(s) => {
                    s.add_bus(*bus);
                    None
                }
                None => Some(Garbage::Any(bus)),
            },
            Command::BusGain { space, bus, gain } => {
                if let Some(s) = self.space_mut(space) {
                    s.set_bus_gain(bus, gain);
                }
                None
            }
            Command::Sync { space, descs } => {
                let buffers = std::mem::take(&mut self.buffers);
                let mut streams = std::mem::take(&mut self.streams);
                if let Some(s) = self.space_mut(space) {
                    s.sync_with(&descs, &buffers, &mut streams);
                }
                self.buffers = buffers;
                self.streams = streams;
                Some(Garbage::Descs(descs))
            }
            Command::Upsert { space, desc } => {
                let buffers = std::mem::take(&mut self.buffers);
                let mut streams = std::mem::take(&mut self.streams);
                if let Some(s) = self.space_mut(space) {
                    s.upsert_with(desc, &buffers, &mut streams);
                }
                self.buffers = buffers;
                self.streams = streams;
                None
            }
            Command::Remove { space, id } => {
                if let Some(s) = self.space_mut(space) {
                    s.remove(id);
                }
                None
            }
            Command::Fire { space, desc } => {
                let buffers = std::mem::take(&mut self.buffers);
                let mut streams = std::mem::take(&mut self.streams);
                if let Some(s) = self.space_mut(space) {
                    s.fire_with(desc, &buffers, &mut streams);
                }
                self.buffers = buffers;
                self.streams = streams;
                None
            }
            Command::Stop { space, id } => {
                if let Some(s) = self.space_mut(space) {
                    s.stop(id);
                }
                None
            }
            Command::SetChain { target, mut chain } => {
                // Swap the new chain into place and send the old one back in the same box.
                if let Some(current) = self.chain_mut(target) {
                    std::mem::swap(current, &mut *chain);
                }
                Some(Garbage::Any(chain))
            }
            Command::SetEffectParam {
                target,
                index,
                param,
                value,
            } => {
                if let Some(c) = self.chain_mut(target) {
                    c.set_param(index, param, value);
                }
                None
            }
            Command::SetEffectEnabled {
                target,
                index,
                enabled,
            } => {
                if let Some(c) = self.chain_mut(target) {
                    c.set_enabled(index, enabled);
                }
                None
            }
            Command::BufferAdd { id, buffer } => {
                let idx = id as usize;
                if self.buffers.len() <= idx {
                    self.buffers.resize_with(idx + 1, || None);
                }
                self.buffers[idx].replace(buffer).map(Garbage::Buffer)
            }
            Command::BufferFree(id) => self
                .buffers
                .get_mut(id as usize)
                .and_then(Option::take)
                .map(Garbage::Buffer),
            Command::StreamAdd { id, source } => {
                let idx = id as usize;
                if self.streams.len() <= idx {
                    self.streams.resize_with(idx + 1, || None);
                }
                self.streams[idx].replace(source).map(Garbage::Stream)
            }
            Command::StreamFree(id) => self
                .streams
                .get_mut(id as usize)
                .and_then(Option::take)
                .map(Garbage::Stream),
            Command::MasterGain(g) => {
                self.master_gain.set_target(g);
                None
            }
        }
    }

    /// Apply a command and keep its garbage for [`Engine::drain_garbage`]. What the period
    /// callbacks use.
    pub fn run(&mut self, cmd: Command) {
        if let Some(g) = self.apply(cmd) {
            self.garbage.push(g);
        }
    }

    /// Hand every released object to `sink` (the garbage ring). A refused item — the ring is
    /// full — is kept and retried next period; nothing is ever freed on this thread.
    pub fn drain_garbage(&mut self, mut sink: impl FnMut(Garbage) -> Result<(), Garbage>) {
        while let Some(g) = self.garbage.pop() {
            if let Err(g) = sink(g) {
                self.garbage.push(g);
                return;
            }
        }
        for space in self.spaces.iter_mut().flatten() {
            space.drain_garbage(&mut sink);
        }
    }

    /// `(space, id)` pairs whose sequences ended since the last call, in order. No allocation.
    pub fn drain_finished(&mut self, mut f: impl FnMut((u32, u64))) {
        for (si, space) in self.spaces.iter_mut().enumerate() {
            if let Some(s) = space {
                s.drain_finished(|id| f((si as u32, id)));
            }
        }
    }

    /// `(space, id)` pairs whose sequences ended since the last call (tests and the null
    /// backend, where the caller is the game thread; allocates).
    pub fn take_finished(&mut self) -> Vec<(u32, u64)> {
        let mut out = Vec::new();
        self.drain_finished(|f| out.push(f));
        out
    }

    /// Enforce the voice cap: the quietest, lowest-priority voices beyond it are muted.
    fn enforce_voice_cap(&mut self) {
        self.audible.clear();
        for (si, space) in self.spaces.iter_mut().enumerate() {
            if let Some(s) = space {
                s.unsteal_all();
                for (vi, priority, level) in s.audible() {
                    self.audible.push((si as u32, vi, priority, level));
                }
            }
        }
        if self.audible.len() <= self.cfg.voice_cap {
            return;
        }
        // Keep the highest priority, then loudest; steal the rest.
        self.audible.sort_by(|a, b| {
            b.2.cmp(&a.2)
                .then(b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal))
        });
        for &(si, vi, _, _) in &self.audible[self.cfg.voice_cap..] {
            if let Some(Some(s)) = self.spaces.get_mut(si as usize) {
                s.steal(vi);
            }
        }
    }

    /// Render one period into `out` (interleaved stereo), overwriting.
    pub fn process(&mut self, out: &mut [f32]) {
        let frames = (out.len() / 2).min(self.cfg.block_frames);
        let out = &mut out[..frames * 2];
        out.iter_mut().for_each(|s| *s = 0.0);
        self.enforce_voice_cap();
        for space in self.spaces.iter_mut().flatten() {
            space.process(out);
        }
        let (g0, gs) = self.master_gain.block(frames);
        for f in 0..frames {
            let g = g0 + gs * f as f32;
            out[2 * f] *= g;
            out[2 * f + 1] *= g;
        }
        self.master.process(out);
    }

    pub fn space(&self, id: u32) -> Option<&Space> {
        self.spaces.get(id as usize).and_then(|s| s.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HeadModel;
    use crate::space::{SourceDesc, SpaceConfig};
    use loadstar_core::geometry::{Bounds, Vec3};

    fn engine() -> Engine {
        Engine::new(EngineConfig {
            voice_cap: 2,
            block_frames: 128,
            ..Default::default()
        })
    }

    fn space(e: &Engine) -> Box<Space> {
        Box::new(Space::new(
            SpaceConfig::default(),
            e.cfg.sample_rate as f32,
            e.cfg.block_frames,
            Box::new(HeadModel),
        ))
    }

    fn energy(buf: &[f32]) -> f32 {
        buf.iter().map(|s| s * s).sum()
    }

    #[test]
    fn voice_cap_keeps_the_highest_priority_voices() {
        let mut e = engine();
        e.apply(Command::SpaceCreate {
            id: 0,
            space: space(&e),
        });
        e.apply(Command::BufferAdd {
            id: 0,
            buffer: Arc::new(Buffer {
                samples: vec![0.5; 48000],
                rate: 48000,
            }),
        });
        let at = |x: f32| Bounds::Point(Vec3::new(x, 0.0, -1.0));
        let mut descs: Vec<SourceDesc> = (1..=3)
            .map(|i| SourceDesc::buffer(i, 0, at(i as f32 - 2.0)).looping())
            .collect();
        descs[2].priority = 9;
        e.apply(Command::Sync { space: 0, descs });
        let mut out = vec![0.0; 256];
        for _ in 0..4 {
            e.process(&mut out);
        }
        let s = e.space(0).unwrap();
        let playing: Vec<bool> = (1..=3).map(|i| s.state(i).unwrap().playing).collect();
        assert_eq!(playing.iter().filter(|&&p| p).count(), 2, "{playing:?}");
        assert!(playing[2], "priority 9 survives");
    }

    #[test]
    fn master_gain_and_chain_apply_last_and_finished_ids_are_collected() {
        let mut e = Engine::new(EngineConfig::default());
        e.apply(Command::SpaceCreate {
            id: 0,
            space: space(&e),
        });
        e.apply(Command::BufferAdd {
            id: 0,
            buffer: Arc::new(Buffer {
                samples: vec![0.5; 128],
                rate: 48000,
            }),
        });
        e.apply(Command::Fire {
            space: 0,
            desc: SourceDesc::buffer(5, 0, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))),
        });
        e.apply(Command::MasterGain(0.0));
        let mut out = vec![0.0; 256];
        e.process(&mut out);
        assert!(
            energy(&out) < energy(&vec![0.5; 256]) * 0.5,
            "master ramping to 0 within the block"
        );
        e.process(&mut out);
        assert_eq!(e.take_finished(), vec![(0, 5)]);
        let garbage = e.apply(Command::SpaceDestroy(0));
        assert!(
            garbage.is_some(),
            "displaced state comes back to be dropped on the game thread"
        );
    }

    #[test]
    fn displaced_chains_travel_back_in_their_own_box_and_voice_garbage_drains() {
        let mut e = Engine::new(EngineConfig::default());
        e.run(Command::SpaceCreate {
            id: 0,
            space: space(&e),
        });
        let chain = Box::new(Chain::empty(48000.0, 2, 128));
        let addr = &*chain as *const Chain;
        e.run(Command::SetChain {
            target: ChainTarget::Master,
            chain,
        });
        let mut got = Vec::new();
        e.drain_garbage(|g| {
            got.push(g);
            Ok(())
        });
        assert_eq!(got.len(), 1);
        match &got[0] {
            Garbage::Any(b) => assert_eq!(
                b.downcast_ref::<Chain>().map(|c| c as *const Chain),
                Some(addr),
                "the incoming box carries the old chain out"
            ),
            _ => panic!("expected a boxed chain"),
        }
        // A fired voice's buffer reference surfaces through the same drain.
        e.run(Command::BufferAdd {
            id: 0,
            buffer: Arc::new(Buffer {
                samples: vec![0.5; 128],
                rate: 48000,
            }),
        });
        e.run(Command::Fire {
            space: 0,
            desc: SourceDesc::buffer(5, 0, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))),
        });
        let mut out = vec![0.0; 256];
        e.process(&mut out);
        e.process(&mut out);
        let mut buffers = 0;
        e.drain_garbage(|g| {
            if matches!(g, Garbage::Buffer(_)) {
                buffers += 1;
            }
            Ok(())
        });
        assert_eq!(buffers, 1);
    }
}
