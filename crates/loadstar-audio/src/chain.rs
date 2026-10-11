//! Effect chains as data (spec §4.2–§4.6). A chain is an ordered list of nodes; each node is a
//! fixed-size [`EffectDesc`] the mod can send across an ABI. Chains are built on the game
//! thread (allocation happens there) and swapped into the audio thread whole; parameter and
//! enable changes are applied in place without allocation and take effect through ramps.

use crate::dsp::{Biquad, Ramp, db_to_gain};

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EffectKind {
    Gain = 0,
    LowPass = 1,
    HighPass = 2,
    LowShelf = 3,
    HighShelf = 4,
    Peak = 5,
    Delay = 6,
    Limiter = 7,
    /// Configures the voice's distance model; not processed in the chain.
    Distance = 8,
    /// Configures the voice's renderer parameters; not processed in the chain.
    Spatializer = 9,
}

impl EffectKind {
    pub fn from_u32(v: u32) -> Option<EffectKind> {
        Some(match v {
            0 => EffectKind::Gain,
            1 => EffectKind::LowPass,
            2 => EffectKind::HighPass,
            3 => EffectKind::LowShelf,
            4 => EffectKind::HighShelf,
            5 => EffectKind::Peak,
            6 => EffectKind::Delay,
            7 => EffectKind::Limiter,
            8 => EffectKind::Distance,
            9 => EffectKind::Spatializer,
            _ => return None,
        })
    }
}

/// Flag bit shared by every node. Renderer cue bits live above it (see `render::flags`).
pub const ENABLED: u32 = 1;

/// One node of a chain. `p` is kind-specific:
/// - `Gain`: p0 = linear gain
/// - `LowPass`/`HighPass`: p0 = corner Hz, p1 = Q
/// - `LowShelf`/`HighShelf`: p0 = corner Hz, p1 = gain dB
/// - `Peak`: p0 = centre Hz, p1 = gain dB, p2 = Q
/// - `Delay`: p0 = ms, p1 = feedback 0..1, p2 = mix 0..1
/// - `Limiter`: p0 = threshold dB, p1 = release ms
/// - `Distance`: p0 = min, p1 = max (0 = none), p2 = rolloff, p3 = model (0 inverse, 1 linear, 2 reference, 3 none = gain 1 everywhere)
/// - `Spatializer`: see `render::SpatializerParams`
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EffectDesc {
    pub kind: EffectKind,
    pub flags: u32,
    pub p: [f32; 8],
}

impl EffectDesc {
    pub fn new(kind: EffectKind, p: [f32; 8]) -> EffectDesc {
        EffectDesc { kind, flags: ENABLED, p }
    }

    pub fn gain(gain: f32) -> EffectDesc {
        Self::new(EffectKind::Gain, [gain, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn lowpass(hz: f32, q: f32) -> EffectDesc {
        Self::new(EffectKind::LowPass, [hz, q, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn highpass(hz: f32, q: f32) -> EffectDesc {
        Self::new(EffectKind::HighPass, [hz, q, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn lowshelf(hz: f32, db: f32) -> EffectDesc {
        Self::new(EffectKind::LowShelf, [hz, db, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn highshelf(hz: f32, db: f32) -> EffectDesc {
        Self::new(EffectKind::HighShelf, [hz, db, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn peak(hz: f32, db: f32, q: f32) -> EffectDesc {
        Self::new(EffectKind::Peak, [hz, db, q, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn delay(ms: f32, feedback: f32, mix: f32) -> EffectDesc {
        Self::new(EffectKind::Delay, [ms, feedback, mix, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn limiter(threshold_db: f32, release_ms: f32) -> EffectDesc {
        Self::new(EffectKind::Limiter, [threshold_db, release_ms, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0])
    }
    pub fn distance(min: f32, max: f32, rolloff: f32, model: DistanceModel) -> EffectDesc {
        Self::new(EffectKind::Distance, [min, max, rolloff, model as u32 as f32, 0.0, 0.0, 0.0, 0.0])
    }

    pub fn enabled(&self) -> bool {
        self.flags & ENABLED != 0
    }

    pub fn with_enabled(mut self, enabled: bool) -> EffectDesc {
        if enabled { self.flags |= ENABLED } else { self.flags &= !ENABLED }
        self
    }
}

/// Spec §4.5.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum DistanceModel {
    /// `(min / d)^rolloff` between min and max.
    #[default]
    Inverse = 0,
    /// Linear fade from 1 at min to 0 at max.
    Linear = 1,
    /// `min / (min + d)`, never below `1 / rolloff` (rolloff ≥ 1 as the floor divisor).
    Reference = 2,
    /// Gain 1 at every distance: the mod owns distance → gain (it computes the source's `gain`
    /// itself, as wotr's NAudio path always did). `max` still culls when set.
    None = 3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DistanceParams {
    pub min: f32,
    pub max: f32,
    pub rolloff: f32,
    pub model: DistanceModel,
}

impl DistanceParams {
    /// 1 metre of full gain, inverse falloff, no cutoff.
    pub fn default_for(unit_scale: f32) -> DistanceParams {
        DistanceParams { min: unit_scale.max(1e-3), max: 0.0, rolloff: 1.0, model: DistanceModel::Inverse }
    }

    pub fn from_desc(d: &EffectDesc) -> DistanceParams {
        let model = match d.p[3] as u32 {
            1 => DistanceModel::Linear,
            2 => DistanceModel::Reference,
            3 => DistanceModel::None,
            _ => DistanceModel::Inverse,
        };
        DistanceParams { min: d.p[0].max(1e-3), max: d.p[1].max(0.0), rolloff: d.p[2].max(0.0), model }
    }

    pub fn gain(&self, distance: f32) -> f32 {
        let d = distance.max(0.0);
        if self.max > 0.0 && d >= self.max {
            return 0.0;
        }
        if d <= self.min || self.model == DistanceModel::None {
            return 1.0;
        }
        match self.model {
            DistanceModel::None => 1.0,
            DistanceModel::Inverse => (self.min / d).powf(self.rolloff),
            DistanceModel::Linear => {
                if self.max <= self.min {
                    1.0
                } else {
                    1.0 - (d - self.min) / (self.max - self.min)
                }
            }
            DistanceModel::Reference => {
                let floor = if self.rolloff > 1.0 { 1.0 / self.rolloff } else { 0.0 };
                (self.min / (self.min + (d - self.min))).max(floor)
            }
        }
    }
}

enum NodeState {
    Gain(Ramp),
    /// One biquad per channel, retuned once per block toward the desc's parameters.
    Filter { filters: [Biquad; 2], current: [f32; 3] },
    Delay { lines: Vec<f32>, len: usize, pos: usize },
    Limiter { env: f32 },
    /// Distance / Spatializer: configuration only.
    Config,
}

struct Node {
    desc: EffectDesc,
    wet: Ramp,
    state: NodeState,
}

/// Effect nodes moved out of a chain with [`Chain::take_nodes`], opaque to callers: exists so a
/// retiring voice can hand its effect state to the garbage ring instead of freeing it in the
/// period callback.
pub struct ChainNodes(Vec<Node>);

impl ChainNodes {
    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// An ordered list of effect nodes processing interleaved audio of a fixed channel count.
pub struct Chain {
    nodes: Vec<Node>,
    channels: usize,
    rate: f32,
    scratch: Vec<f32>,
}

impl Chain {
    /// Build a chain (allocates — game thread). `block` is the maximum frames per period.
    pub fn new(descs: &[EffectDesc], rate: f32, channels: usize, block: usize) -> Chain {
        let channels = channels.max(1);
        let nodes = descs
            .iter()
            .map(|&desc| {
                let state = match desc.kind {
                    EffectKind::Gain => NodeState::Gain(Ramp::new(desc.p[0])),
                    EffectKind::LowPass | EffectKind::HighPass | EffectKind::LowShelf | EffectKind::HighShelf | EffectKind::Peak => {
                        let mut filters = [Biquad::identity(); 2];
                        let current = [desc.p[0], desc.p[1], desc.p[2]];
                        Self::tune(desc.kind, &mut filters, current, rate);
                        NodeState::Filter { filters, current }
                    }
                    EffectKind::Delay => {
                        let len = ((desc.p[0].max(0.0) / 1000.0 * rate) as usize).max(1);
                        NodeState::Delay { lines: vec![0.0; len * channels], len, pos: 0 }
                    }
                    EffectKind::Limiter => NodeState::Limiter { env: 1.0 },
                    EffectKind::Distance | EffectKind::Spatializer => NodeState::Config,
                };
                Node { desc, wet: Ramp::new(if desc.enabled() { 1.0 } else { 0.0 }), state }
            })
            .collect();
        Chain { nodes, channels, rate, scratch: vec![0.0; block.max(1) * channels] }
    }

    pub fn empty(rate: f32, channels: usize, block: usize) -> Chain {
        Chain::new(&[], rate, channels, block)
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Move the nodes out without allocating, leaving an empty chain with its scratch intact.
    pub fn take_nodes(&mut self) -> ChainNodes {
        ChainNodes(std::mem::take(&mut self.nodes))
    }

    pub fn descs(&self) -> impl Iterator<Item = &EffectDesc> {
        self.nodes.iter().map(|n| &n.desc)
    }

    /// The distance model configured in this chain, if any.
    pub fn distance(&self) -> Option<DistanceParams> {
        self.nodes.iter().find(|n| n.desc.kind == EffectKind::Distance && n.desc.enabled()).map(|n| DistanceParams::from_desc(&n.desc))
    }

    /// The spatializer override configured in this chain, if any.
    pub fn spatializer(&self) -> Option<&EffectDesc> {
        self.nodes.iter().find(|n| n.desc.kind == EffectKind::Spatializer && n.desc.enabled()).map(|n| &n.desc)
    }

    /// Change one parameter in place (no allocation). Delay length changes are ignored — the
    /// line was sized at build; send a new chain for that.
    pub fn set_param(&mut self, index: usize, param: usize, value: f32) -> bool {
        let Some(node) = self.nodes.get_mut(index) else { return false };
        if param >= node.desc.p.len() {
            return false;
        }
        node.desc.p[param] = value;
        if let NodeState::Gain(ramp) = &mut node.state {
            ramp.set_target(node.desc.p[0]);
        }
        true
    }

    /// Crossfaded enable/disable — A/B by ear without clicks.
    pub fn set_enabled(&mut self, index: usize, enabled: bool) -> bool {
        let Some(node) = self.nodes.get_mut(index) else { return false };
        node.desc = node.desc.with_enabled(enabled);
        node.wet.set_target(if enabled { 1.0 } else { 0.0 });
        true
    }

    pub fn reset(&mut self) {
        for node in &mut self.nodes {
            match &mut node.state {
                NodeState::Filter { filters, .. } => filters.iter_mut().for_each(Biquad::reset_state),
                NodeState::Delay { lines, pos, .. } => {
                    lines.iter_mut().for_each(|s| *s = 0.0);
                    *pos = 0;
                }
                NodeState::Limiter { env } => *env = 1.0,
                _ => {}
            }
        }
    }

    fn tune(kind: EffectKind, filters: &mut [Biquad; 2], p: [f32; 3], rate: f32) {
        for f in filters.iter_mut() {
            match kind {
                EffectKind::LowPass => f.set_lowpass(rate, p[0], if p[1] > 0.0 { p[1] } else { 0.707 }),
                EffectKind::HighPass => f.set_highpass(rate, p[0], if p[1] > 0.0 { p[1] } else { 0.707 }),
                EffectKind::LowShelf => f.set_lowshelf(rate, p[0], p[1]),
                EffectKind::HighShelf => f.set_highshelf(rate, p[0], p[1]),
                EffectKind::Peak => f.set_peak(rate, p[0], p[1], if p[2] > 0.0 { p[2] } else { 1.0 }),
                _ => {}
            }
        }
    }

    /// Process one block of interleaved audio in place. `buf.len()` must be a multiple of
    /// the channel count and no larger than the block the chain was built for.
    pub fn process(&mut self, buf: &mut [f32]) {
        let channels = self.channels;
        let frames = buf.len() / channels;
        if frames == 0 {
            return;
        }
        let rate = self.rate;
        for node in &mut self.nodes {
            if matches!(node.state, NodeState::Config) {
                continue;
            }
            let (wet_start, wet_step) = node.wet.block(frames);
            if wet_start == 0.0 && wet_step == 0.0 {
                continue; // fully bypassed and settled
            }
            if wet_start == 1.0 && wet_step == 0.0 {
                // Fully wet and settled: process in place, no scratch round-trip.
                Self::run(&mut node.state, &node.desc, buf, channels, frames, rate);
                continue;
            }
            let scratch = &mut self.scratch[..buf.len()];
            scratch.copy_from_slice(buf);
            Self::run(&mut node.state, &node.desc, scratch, channels, frames, rate);
            {
                for f in 0..frames {
                    let w = wet_start + wet_step * f as f32;
                    for c in 0..channels {
                        let i = f * channels + c;
                        buf[i] = buf[i] * (1.0 - w) + scratch[i] * w;
                    }
                }
            }
        }
    }

    fn run(state: &mut NodeState, desc: &EffectDesc, buf: &mut [f32], channels: usize, frames: usize, rate: f32) {
        match state {
            NodeState::Gain(ramp) => {
                let (start, step) = ramp.block(frames);
                for f in 0..frames {
                    let g = start + step * f as f32;
                    for c in 0..channels {
                        buf[f * channels + c] *= g;
                    }
                }
            }
            NodeState::Filter { filters, current } => {
                // Retune once per block toward the target parameters, state preserved.
                let target = [desc.p[0], desc.p[1], desc.p[2]];
                if current.iter().zip(&target).any(|(c, t)| (c - t).abs() > 1e-3) {
                    for (c, t) in current.iter_mut().zip(&target) {
                        *c += (t - *c) * 0.5;
                        if (*c - t).abs() < 1e-3 {
                            *c = *t;
                        }
                    }
                    Self::tune(desc.kind, filters, *current, rate);
                }
                for f in 0..frames {
                    for c in 0..channels.min(2) {
                        let i = f * channels + c;
                        buf[i] = filters[c].process(buf[i]);
                    }
                }
            }
            NodeState::Delay { lines, len, pos } => {
                let feedback = desc.p[1].clamp(0.0, 0.99);
                let mix = desc.p[2].clamp(0.0, 1.0);
                for f in 0..frames {
                    for c in 0..channels {
                        let i = f * channels + c;
                        let li = *pos * channels + c;
                        let delayed = lines[li];
                        let x = buf[i];
                        lines[li] = x + delayed * feedback;
                        buf[i] = x + delayed * mix;
                    }
                    *pos += 1;
                    if *pos >= *len {
                        *pos = 0;
                    }
                }
            }
            NodeState::Limiter { env } => {
                let threshold = db_to_gain(desc.p[0].min(0.0));
                let release = (-1.0 / (desc.p[1].max(1.0) / 1000.0 * rate)).exp();
                for f in 0..frames {
                    let mut peak = 0.0f32;
                    for c in 0..channels {
                        peak = peak.max(buf[f * channels + c].abs());
                    }
                    let needed = if peak > threshold { threshold / peak } else { 1.0 };
                    *env = if needed <= *env { needed } else { *env + (1.0 - *env) * (1.0 - release) };
                    for c in 0..channels {
                        buf[f * channels + c] *= *env;
                    }
                }
            }
            NodeState::Config => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(hz: f32, rate: f32, frames: usize, channels: usize) -> Vec<f32> {
        (0..frames * channels)
            .map(|i| ((i / channels) as f32 * hz * std::f32::consts::TAU / rate).sin())
            .collect()
    }

    fn rms(buf: &[f32]) -> f32 {
        (buf.iter().map(|s| s * s).sum::<f32>() / buf.len() as f32).sqrt()
    }

    #[test]
    fn gain_node_ramps_without_jumps() {
        let mut chain = Chain::new(&[EffectDesc::gain(1.0)], 48000.0, 1, 256);
        let mut buf = vec![1.0; 256];
        chain.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 1.0));
        chain.set_param(0, 0, 0.0);
        let mut buf = vec![1.0; 256];
        chain.process(&mut buf);
        assert_eq!(buf[0], 1.0);
        assert!(buf[255] < 0.01);
        assert!(buf.windows(2).all(|w| (w[0] - w[1]).abs() < 0.01), "linear ramp");
    }

    #[test]
    fn lowpass_removes_highs_and_toggle_crossfades() {
        let mut chain = Chain::new(&[EffectDesc::lowpass(500.0, 0.707)], 48000.0, 2, 512);
        let mut hi = sine(8000.0, 48000.0, 512, 2);
        for _ in 0..8 {
            chain.process(&mut hi);
            hi = sine(8000.0, 48000.0, 512, 2);
            chain.process(&mut hi);
        }
        assert!(rms(&hi) < 0.05, "8 kHz through a 500 Hz lowpass: {}", rms(&hi));

        chain.set_enabled(0, false);
        let mut buf = sine(8000.0, 48000.0, 512, 2);
        chain.process(&mut buf);
        let dry_rms = std::f32::consts::FRAC_1_SQRT_2;
        let mid = rms(&buf);
        assert!(mid > 0.1 && mid < dry_rms, "the crossfade block sits between wet and dry: {mid}");
        let mut buf = sine(8000.0, 48000.0, 512, 2);
        chain.process(&mut buf);
        assert!((rms(&buf) - dry_rms).abs() < 0.02, "bypassed once settled");
    }

    #[test]
    fn toggling_a_node_crossfades_linearly() {
        let mut chain = Chain::new(&[EffectDesc::gain(0.0)], 48000.0, 1, 128);
        let mut buf = vec![1.0; 128];
        chain.process(&mut buf);
        assert!(buf.iter().all(|&s| s == 0.0));
        chain.set_enabled(0, false);
        let mut buf = vec![1.0; 128];
        chain.process(&mut buf);
        for (f, &s) in buf.iter().enumerate() {
            assert!((s - f as f32 / 128.0).abs() < 1e-4, "sample {f}: {s}");
        }
        chain.set_enabled(0, true);
        let mut buf = vec![1.0; 128];
        chain.process(&mut buf);
        assert!((buf[0] - 1.0).abs() < 1e-4 && buf[127] < 0.01, "and back toward wet");
    }

    #[test]
    fn limiter_holds_peaks_at_threshold() {
        let mut chain = Chain::new(&[EffectDesc::limiter(-6.0, 50.0)], 48000.0, 2, 512);
        let mut buf = vec![1.0; 1024];
        chain.process(&mut buf);
        let ceiling = db_to_gain(-6.0);
        assert!(buf.iter().all(|&s| s <= ceiling + 1e-4), "max {}", buf.iter().cloned().fold(0.0, f32::max));
    }

    #[test]
    fn distance_models() {
        let inv = DistanceParams { min: 1.0, max: 0.0, rolloff: 1.0, model: DistanceModel::Inverse };
        assert_eq!(inv.gain(0.5), 1.0);
        assert_eq!(inv.gain(4.0), 0.25);
        let lin = DistanceParams { min: 1.0, max: 5.0, rolloff: 1.0, model: DistanceModel::Linear };
        assert_eq!(lin.gain(3.0), 0.5);
        assert_eq!(lin.gain(9.0), 0.0);
        let reference = DistanceParams { min: 2.0, max: 0.0, rolloff: 5.0, model: DistanceModel::Reference };
        assert!((reference.gain(4.0) - 0.5).abs() < 1e-6);
        assert!((reference.gain(1000.0) - 0.2).abs() < 1e-6, "floored at 1/rolloff");
        let d = EffectDesc::distance(1.0, 10.0, 2.0, DistanceModel::Linear);
        assert_eq!(DistanceParams::from_desc(&d).model, DistanceModel::Linear);
        let none = DistanceParams { min: 1.0, max: 0.0, rolloff: 1.0, model: DistanceModel::None };
        assert_eq!(none.gain(0.5), 1.0);
        assert_eq!(none.gain(500.0), 1.0, "none: the mod owns distance gain");
        let none_capped = DistanceParams { min: 1.0, max: 20.0, rolloff: 1.0, model: DistanceModel::None };
        assert_eq!(none_capped.gain(19.0), 1.0);
        assert_eq!(none_capped.gain(25.0), 0.0, "max still culls");
        assert_eq!(DistanceParams::from_desc(&EffectDesc::distance(1.0, 0.0, 0.0, DistanceModel::None)).model, DistanceModel::None);
    }

    #[test]
    fn config_nodes_are_reported_and_skipped() {
        let chain = Chain::new(&[EffectDesc::distance(1.0, 0.0, 1.0, DistanceModel::Inverse), EffectDesc::gain(0.5)], 48000.0, 1, 64);
        assert_eq!(chain.distance().unwrap().min, 1.0);
        assert!(chain.spatializer().is_none());
    }
}
