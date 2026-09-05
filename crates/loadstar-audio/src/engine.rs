//! Audio-thread state (spec §4.7, §5): buffers, spaces, the master chain and the voice cap.
//! [`Engine::process`] is the period callback; [`Engine::apply`] runs commands at the start
//! of a period.

use std::sync::Arc;

use crate::chain::Chain;
use crate::commands::{ChainTarget, Command, Garbage};
use crate::dsp::Ramp;
use crate::space::{Buffer, Space};

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EngineConfig {
    pub sample_rate: u32,
    pub block_frames: usize,
    pub master_gain: f32,
    pub voice_cap: usize,
}

impl Default for EngineConfig {
    fn default() -> Self {
        EngineConfig { sample_rate: 48000, block_frames: 128, master_gain: 1.0, voice_cap: 64 }
    }
}

pub struct Engine {
    cfg: EngineConfig,
    spaces: Vec<Option<Space>>,
    buffers: Vec<Option<Arc<Buffer>>>,
    master_gain: Ramp,
    master: Chain,
    finished: Vec<(u32, u64)>,
    audible: Vec<(u32, usize, u8, f32)>,
}

impl Engine {
    pub fn new(cfg: EngineConfig) -> Engine {
        let cfg = EngineConfig { block_frames: cfg.block_frames.max(1), voice_cap: if cfg.voice_cap == 0 { 64 } else { cfg.voice_cap }, ..cfg };
        Engine {
            cfg,
            spaces: Vec::new(),
            buffers: Vec::new(),
            master_gain: Ramp::new(cfg.master_gain),
            master: Chain::empty(cfg.sample_rate as f32, 2, cfg.block_frames),
            finished: Vec::new(),
            audible: Vec::with_capacity(256),
        }
    }

    pub fn config(&self) -> EngineConfig {
        self.cfg
    }

    fn space_mut(&mut self, id: u32) -> Option<&mut Space> {
        self.spaces.get_mut(id as usize).and_then(Option::as_mut)
    }

    fn chain_mut(&mut self, target: ChainTarget) -> Option<&mut Chain> {
        match target {
            ChainTarget::Master => Some(&mut self.master),
            ChainTarget::Space(s) => self.space_mut(s).map(Space::chain_mut),
            ChainTarget::Bus(s, b) => self.space_mut(s).and_then(|sp| sp.bus_chain_mut(b)),
            ChainTarget::Source(s, id, stage) => self.space_mut(s).and_then(|sp| sp.source_chain_mut(id, stage)),
        }
    }

    /// Apply one command. Returns anything displaced that must be dropped off the audio thread.
    pub fn apply(&mut self, cmd: Command) -> Option<Garbage> {
        match cmd {
            Command::SpaceCreate { id, space } => {
                let idx = id as usize;
                if self.spaces.len() <= idx {
                    self.spaces.resize_with(idx + 1, || None);
                }
                self.spaces[idx].replace(*space).map(|old| Box::new(old) as Garbage)
            }
            Command::SpaceDestroy(id) => self.spaces.get_mut(id as usize).and_then(Option::take).map(|s| Box::new(s) as Garbage),
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
            Command::BusCreate { space, bus } => {
                match self.space_mut(space) {
                    Some(s) => {
                        s.add_bus(*bus);
                        None
                    }
                    None => Some(bus as Garbage),
                }
            }
            Command::BusGain { space, bus, gain } => {
                if let Some(s) = self.space_mut(space) {
                    s.set_bus_gain(bus, gain);
                }
                None
            }
            Command::Sync { space, descs } => {
                let buffers = std::mem::take(&mut self.buffers);
                if let Some(s) = self.space_mut(space) {
                    s.sync(&descs, &buffers);
                }
                self.buffers = buffers;
                Some(Box::new(descs) as Garbage)
            }
            Command::Upsert { space, desc } => {
                let buffers = std::mem::take(&mut self.buffers);
                if let Some(s) = self.space_mut(space) {
                    s.upsert(desc, &buffers);
                }
                self.buffers = buffers;
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
                if let Some(s) = self.space_mut(space) {
                    s.fire(desc, &buffers);
                }
                self.buffers = buffers;
                None
            }
            Command::Stop { space, id } => {
                if let Some(s) = self.space_mut(space) {
                    s.stop(id);
                }
                None
            }
            Command::SetChain { target, chain } => match target {
                ChainTarget::Master => Some(Box::new(std::mem::replace(&mut self.master, *chain)) as Garbage),
                ChainTarget::Space(s) => self.space_mut(s).map(|sp| Box::new(sp.set_chain(*chain)) as Garbage),
                ChainTarget::Bus(s, b) => self.space_mut(s).and_then(|sp| sp.set_bus_chain(b, *chain)).map(|c| Box::new(c) as Garbage),
                ChainTarget::Source(s, id, stage) => {
                    self.space_mut(s).and_then(|sp| sp.set_source_chain(id, stage, *chain)).map(|c| Box::new(c) as Garbage)
                }
            },
            Command::SetEffectParam { target, index, param, value } => {
                if let Some(c) = self.chain_mut(target) {
                    c.set_param(index, param, value);
                }
                None
            }
            Command::SetEffectEnabled { target, index, enabled } => {
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
                self.buffers[idx].replace(buffer).map(|old| Box::new(old) as Garbage)
            }
            Command::BufferFree(id) => self.buffers.get_mut(id as usize).and_then(Option::take).map(|b| Box::new(b) as Garbage),
            Command::MasterGain(g) => {
                self.master_gain.set_target(g);
                None
            }
        }
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
        self.audible.sort_by(|a, b| b.2.cmp(&a.2).then(b.3.partial_cmp(&a.3).unwrap_or(std::cmp::Ordering::Equal)));
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
        for (si, space) in self.spaces.iter_mut().enumerate() {
            if let Some(s) = space {
                s.process(out);
                for id in s.take_finished() {
                    self.finished.push((si as u32, id));
                }
            }
        }
        let (g0, gs) = self.master_gain.block(frames);
        for f in 0..frames {
            let g = g0 + gs * f as f32;
            out[2 * f] *= g;
            out[2 * f + 1] *= g;
        }
        self.master.process(out);
    }

    /// `(space, id)` pairs whose sequences ended since the last call.
    pub fn take_finished(&mut self) -> Vec<(u32, u64)> {
        std::mem::take(&mut self.finished)
    }

    pub fn space(&self, id: u32) -> Option<&Space> {
        self.spaces.get(id as usize).and_then(Option::as_ref)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render::HeadModel;
    use crate::space::{SourceDesc, SpaceConfig};
    use loadstar_core::geometry::{Bounds, Vec3};

    fn engine() -> Engine {
        Engine::new(EngineConfig { voice_cap: 2, block_frames: 128, ..Default::default() })
    }

    fn space(e: &Engine) -> Box<Space> {
        Box::new(Space::new(SpaceConfig::default(), e.cfg.sample_rate as f32, e.cfg.block_frames, Box::new(HeadModel)))
    }

    fn energy(buf: &[f32]) -> f32 {
        buf.iter().map(|s| s * s).sum()
    }

    #[test]
    fn voice_cap_keeps_the_highest_priority_voices() {
        let mut e = engine();
        e.apply(Command::SpaceCreate { id: 0, space: space(&e) });
        e.apply(Command::BufferAdd { id: 0, buffer: Arc::new(Buffer { samples: vec![0.5; 48000], rate: 48000 }) });
        let at = |x: f32| Bounds::Point(Vec3::new(x, 0.0, -1.0));
        let mut descs: Vec<SourceDesc> = (1..=3).map(|i| SourceDesc::buffer(i, 0, at(i as f32 - 2.0)).looping()).collect();
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
        e.apply(Command::SpaceCreate { id: 0, space: space(&e) });
        e.apply(Command::BufferAdd { id: 0, buffer: Arc::new(Buffer { samples: vec![0.5; 128], rate: 48000 }) });
        e.apply(Command::Fire { space: 0, desc: SourceDesc::buffer(5, 0, Bounds::Point(Vec3::new(0.0, 0.0, -1.0))) });
        e.apply(Command::MasterGain(0.0));
        let mut out = vec![0.0; 256];
        e.process(&mut out);
        assert!(energy(&out) < energy(&vec![0.5; 256]) * 0.5, "master ramping to 0 within the block");
        e.process(&mut out);
        assert_eq!(e.take_finished(), vec![(0, 5)]);
        let garbage = e.apply(Command::SpaceDestroy(0));
        assert!(garbage.is_some(), "displaced state comes back to be dropped on the game thread");
    }
}
