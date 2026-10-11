//! The pluggable mono → stereo stage (spec §4.8). A [`Renderer`] makes one [`VoiceRenderer`]
//! per voice; the voice receives a [`Placement`] every period and renders a mono block into an
//! interleaved stereo block. [`HeadModel`] is the built-in implementation.

use crate::chain::{ENABLED, EffectDesc, EffectKind};

pub mod head_model;
pub use head_model::HeadModel;

/// Where a source is relative to the listener, in the listener's frame (right, up, forward),
/// for one period. Distances are in the space's units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Placement {
    pub right: f32,
    pub up: f32,
    pub forward: f32,
    pub distance: f32,
    /// The listener is inside the source's bounds: centred, no directional cues.
    pub inside: bool,
    pub pan_width: f32,
    /// Listener-relative source: ignore geometry, use `pan` with a plain constant-power law.
    pub relative: bool,
    pub pan: f32,
}

impl Placement {
    pub fn relative(pan: f32) -> Placement {
        Placement {
            right: 0.0,
            up: 0.0,
            forward: 0.0,
            distance: 0.0,
            inside: false,
            pan_width: 1.0,
            relative: true,
            pan,
        }
    }
}

/// Cue toggles carried in a `Spatializer` node's flags (bit 0 is `ENABLED`).
pub mod flags {
    pub const ITD: u32 = 1 << 1;
    pub const SHADOW: u32 = 1 << 2;
    pub const REAR: u32 = 1 << 3;
    pub const ELEVATION: u32 = 1 << 4;
}

/// Head-model parameters (spec §4.4). Defaults are the constants tuned by ear in the Wrath of
/// the Righteous mod.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SpatializerParams {
    pub ild_cap_db: f32,
    pub pan_exponent: f32,
    pub itd_max_ms: f32,
    pub shadow_corner_hz: f32,
    pub shadow_max_db: f32,
    pub rear_corner_hz: f32,
    pub rear_max_db: f32,
    pub elevation_max_db: f32,
    pub flags: u32,
}

impl Default for SpatializerParams {
    fn default() -> Self {
        SpatializerParams {
            ild_cap_db: 12.0,
            pan_exponent: 0.82,
            itd_max_ms: 0.66,
            shadow_corner_hz: 1500.0,
            shadow_max_db: 8.0,
            rear_corner_hz: 3000.0,
            rear_max_db: 10.0,
            elevation_max_db: 4.0,
            flags: ENABLED | flags::ITD | flags::SHADOW | flags::REAR,
        }
    }
}

impl SpatializerParams {
    pub fn from_desc(d: &EffectDesc) -> SpatializerParams {
        let def = SpatializerParams::default();
        let or = |v: f32, d: f32| if v > 0.0 { v } else { d };
        SpatializerParams {
            ild_cap_db: or(d.p[0], def.ild_cap_db),
            pan_exponent: or(d.p[1], def.pan_exponent),
            itd_max_ms: or(d.p[2], def.itd_max_ms),
            shadow_corner_hz: or(d.p[3], def.shadow_corner_hz),
            shadow_max_db: or(d.p[4], def.shadow_max_db),
            rear_corner_hz: or(d.p[5], def.rear_corner_hz),
            rear_max_db: or(d.p[6], def.rear_max_db),
            elevation_max_db: or(d.p[7], def.elevation_max_db),
            flags: d.flags,
        }
    }

    pub fn to_desc(&self) -> EffectDesc {
        EffectDesc {
            kind: EffectKind::Spatializer,
            flags: self.flags,
            p: [
                self.ild_cap_db,
                self.pan_exponent,
                self.itd_max_ms,
                self.shadow_corner_hz,
                self.shadow_max_db,
                self.rear_corner_hz,
                self.rear_max_db,
                self.elevation_max_db,
            ],
        }
    }

    pub fn has(&self, flag: u32) -> bool {
        self.flags & flag != 0
    }
}

pub trait Renderer: Send {
    fn name(&self) -> &'static str;
    /// Per-voice state. Called when a voice is created (allocation allowed).
    fn make_voice(&self, rate: f32, block: usize) -> Box<dyn VoiceRenderer>;
}

pub trait VoiceRenderer: Send {
    /// Render `mono` into interleaved stereo `out` (`out.len() == 2 * mono.len()`), overwriting.
    /// Must not allocate; all parameter motion is smoothed internally.
    fn process(
        &mut self,
        mono: &[f32],
        placement: &Placement,
        params: &SpatializerParams,
        out: &mut [f32],
    );
    fn reset(&mut self);
}
