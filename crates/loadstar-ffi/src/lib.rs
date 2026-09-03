//! Loadstar C ABI.
//!
//! Rules of this crate (see `docs/spec.md` §9):
//! - Every exported function is `extern "C"`, takes flat data, and returns a [`Status`].
//! - No callbacks. No strings. No panics escape — every entry point runs inside [`guard`].
//! - Handles are opaque pointers owned by the library; each has a matching `*_destroy`/`*_free`.
//! - `bool` parameters are C `_Bool` (one byte); bindings must marshal them as such.
//! - This is the only crate in the workspace that may contain `unsafe`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use loadstar_core::context::{GoalMode as CoreGoalMode, PathOptions as CorePathOptions, Profile, TileContext};
use loadstar_core::entities::{Entity, SyncStats as CoreSyncStats};
use loadstar_core::tracker::{Action as CoreAction, Filter, FilterEntry, Tracker};
use loadstar_core::geometry::Bounds;
use loadstar_core::graph::LinkId;
use loadstar_core::tile::{Dir, EdgeOverride, GridError, Link, TileGrid};
pub use loadstar_core::geometry::Vec3;

/// Bumped only on breaking ABI changes. Companions check this via [`ls_abi_version`] at load.
const ABI_VERSION: u32 = 1;

/// Result of every ABI call. Exported to C as `LsStatus`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Ok = 0,
    NullPointer = 1,
    InvalidArgument = 2,
    /// A caller-provided buffer was too small; the required size was written to the count out-param.
    BufferTooSmall = 3,
    /// No route exists (or an endpoint is off the map). Not an error; the out handle is null.
    NoPath = 4,
    /// Internal panic caught at the boundary. The context may be inconsistent; the host is intact.
    Panic = 100,
}

/// Opaque: a map plus everything consistent with it (spec §4). Exported to C as `LsContext`.
pub struct Context(TileContext);

/// Opaque: a path result (spec §8). Exported to C as `LsPath`.
pub struct Path(loadstar_core::graph::Path);

/// One entry of the tier-2 edge override table (spec §6.1). `dir` is a `Dir` discriminant (0–3).
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TileEdgeOverride {
    pub node: u32,
    pub dir: u8,
    pub target_layer: u32,
    pub cost: f32,
}

/// Flags for [`LinkDesc`].
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkFlags {
    None = 0,
    Bidirectional = 1,
    /// Create the link disabled; enable later with `ls_link_set_enabled`.
    StartDisabled = 2,
}

/// A link to add (spec §6.3). Endpoints are positions and are snapped to their containing nodes.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct LinkDesc {
    pub id: u64,
    pub from: Vec3,
    pub to: Vec3,
    pub cost: f32,
    /// Bitwise OR of `LinkFlags`.
    pub flags: u32,
}

/// A link traversal within a path: the node at `index` was reached by taking `link_id`.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PathLink {
    pub link_id: u64,
    pub index: u32,
    /// Keeps the struct 16 bytes on every ABI regardless of how `u64` is aligned.
    pub _reserved: u32,
}

/// How target nodes are treated as a destination (spec §8).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GoalMode {
    /// Arrive on a target. Targets are exempt from blocking.
    Onto = 0,
    /// Arrive next to a target (one ordinary step away). Targets stay blocked.
    Adjacent = 1,
}

/// Per-query options. A zeroed struct means: nothing blocked, no snapping, `Onto`, default agent.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PathOptions {
    /// Nodes carrying any of these groups are impassable.
    pub blocked_mask: u32,
    /// Ring radius for start snapping when the start has no edges. 0 disables.
    pub snap_radius: u32,
    /// A `GoalMode` discriminant.
    pub goal_mode: u32,
    /// Agent profile id from `ls_profile_register`; 0 = built-in one-cell agent.
    pub profile: u32,
}

impl PathOptions {
    fn to_core(self) -> Option<CorePathOptions> {
        let goal_mode = match self.goal_mode {
            0 => CoreGoalMode::Onto,
            1 => CoreGoalMode::Adjacent,
            _ => return None,
        };
        Some(CorePathOptions { blocked_mask: self.blocked_mask, snap_radius: self.snap_radius, goal_mode, profile: self.profile })
    }
}

/// An agent's size (spec §7). Tile grids honour `footprint` (side of the square block of
/// cells the agent occupies, anchored at its node, 1–255); the rest is carried for mesh maps.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct ProfileDesc {
    pub footprint: u32,
    pub radius: f32,
    pub height: f32,
    pub step_height: f32,
}

/// Shape of a [`BoundsDesc`] (spec §5 "Bounds").
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BoundsKind {
    /// `a` = position.
    Point = 0,
    /// `a.x` = col, `a.z` = row, `b.x` = width, `b.z` = height (cells); `layer`, or
    /// `UINT32_MAX` for every layer.
    TileRect = 1,
    /// `a` = center, `b` = half-extents.
    Box = 2,
    /// `a` = center, `b.x` = radius.
    Sphere = 3,
}

/// Fixed-size bounds record, 40 bytes. `layer` is only read for `TileRect`; `ref_id` is
/// reserved for registered polygons.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BoundsDesc {
    pub kind: u32,
    pub layer: u32,
    pub ref_id: u32,
    pub _reserved: u32,
    pub a: Vec3,
    pub b: Vec3,
}

impl BoundsDesc {
    fn to_core(self) -> Option<Bounds> {
        Some(match self.kind {
            0 => Bounds::Point(self.a),
            1 => Bounds::TileRect {
                col: self.a.x as u32,
                row: self.a.z as u32,
                width: self.b.x as u32,
                height: self.b.z as u32,
                layer: if self.layer == u32::MAX { None } else { Some(self.layer) },
            },
            2 => Bounds::Box { center: self.a, extents: self.b },
            3 => Bounds::Sphere { center: self.a, radius: self.b.x },
            _ => return None,
        })
    }
}

/// Conventional entity flag bits (spec §5). Every other bit is mod-defined.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntityFlags {
    None = 0,
    /// Excluded by the default tracker filter. Convention only.
    Hidden = 1,
    /// Paths stop next to this entity instead of on it (NPCs vs. exits).
    PathfindAdjacent = 2,
}

/// An entity record (spec §5), 64 bytes. All numeric; labels live mod-side keyed by `id`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct EntityDesc {
    /// Mod-derived unique id. Never interpreted by the core.
    pub id: u64,
    /// Mod-defined; the core only groups by equality.
    pub category: u16,
    pub _reserved: u16,
    /// Blocking-group bitmask projected onto the entity's nodes. 0 = doesn't block.
    pub blocking: u32,
    /// Bitwise OR of `EntityFlags` and mod-defined bits.
    pub flags: u32,
    /// Explicit browse order within a category.
    pub sort_key: u32,
    pub bounds: BoundsDesc,
}

impl EntityDesc {
    fn to_core(self) -> Option<Entity> {
        Some(Entity {
            id: self.id,
            category: self.category,
            bounds: self.bounds.to_core()?,
            blocking: self.blocking,
            flags: self.flags,
            sort_key: self.sort_key,
        })
    }
}

/// Result of [`ls_entities_sync`].
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
    pub _reserved: u32,
}

/// Announcement geometry for an entity relative to the player (spec §8). Pure geometry.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityVector {
    /// Closest point of the entity's bounds to the player.
    pub nearest_point: Vec3,
    /// Degrees clockwise from north (-Z), in `[0, 360)`.
    pub bearing_deg: f32,
    /// Degrees above (+) or below (-) horizontal.
    pub elevation_deg: f32,
    /// Straight-line distance.
    pub distance: f32,
    /// Horizontal Manhattan distance (grid step count).
    pub steps: f32,
}

/// Kinds of [`FilterDesc`] (spec §8).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FilterKind {
    /// `p0` = profile, `p1` = blocked mask.
    Reachable = 0,
    /// `value` = max distance to the entity's nearest point.
    Range = 1,
    /// `mask` = bit per category (categories 0–63).
    Categories = 2,
    /// `p0` = required flag bits, `p1` = forbidden flag bits.
    Flags = 3,
}

/// A tracker filter, 32 bytes. Filters are data the core evaluates — never callbacks.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FilterDesc {
    pub kind: u32,
    /// Nonzero = enabled.
    pub enabled: u32,
    pub p0: u32,
    pub p1: u32,
    pub mask: u64,
    pub value: f32,
    pub _reserved: u32,
}

impl FilterDesc {
    fn to_core(self) -> Option<FilterEntry> {
        let filter = match self.kind {
            0 => Filter::Reachable { profile: self.p0, blocked_mask: self.p1 },
            1 => Filter::Range { max_distance: self.value },
            2 => Filter::Categories { mask: self.mask },
            3 => Filter::Flags { require: self.p0, forbid: self.p1 },
            _ => return None,
        };
        Some(FilterEntry { filter, enabled: self.enabled != 0 })
    }
}

/// Tracker actions (spec §8).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerAction {
    Next = 0,
    Prev = 1,
    CategoryNext = 2,
    CategoryPrev = 3,
    /// Re-report the current selection without moving.
    Current = 4,
}

/// Result of [`ls_tracker_act`]. `entity_id == 0` means nothing is selected; `category` is
/// then the tracker's current category, or `UINT32_MAX` for the all-categories view.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub entity_id: u64,
    pub category: u32,
    /// Position of the selection in cycle order.
    pub index: u32,
}

fn parse_filters(filters: *const FilterDesc, len: usize) -> Option<Vec<FilterEntry>> {
    if len == 0 {
        return Some(Vec::new());
    }
    if filters.is_null() {
        return None;
    }
    // SAFETY: non-null checked; caller guarantees `filters` is readable for `len` entries.
    unsafe { std::slice::from_raw_parts(filters, len) }.iter().map(|f| f.to_core()).collect()
}

impl From<CoreSyncStats> for SyncStats {
    fn from(s: CoreSyncStats) -> Self {
        SyncStats { added: s.added, updated: s.updated, removed: s.removed, _reserved: 0 }
    }
}

/// Run an ABI body, converting any panic into [`Status::Panic`] instead of unwinding into the host.
fn guard(f: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(Status::Panic)
}

fn status_of(r: Result<(), GridError>) -> Status {
    match r {
        Ok(()) => Status::Ok,
        Err(_) => Status::InvalidArgument,
    }
}

/// ABI version of this library. Companions must refuse to load a mismatched major.
#[unsafe(no_mangle)]
pub extern "C" fn ls_abi_version() -> u32 {
    ABI_VERSION
}

/// Smoke test for bindings: writes `a + b` into `out`. Exercises by-value struct passing and out-params.
#[unsafe(no_mangle)]
pub extern "C" fn ls_vec3_add(a: Vec3, b: Vec3, out: *mut Vec3) -> Status {
    guard(|| {
        if out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `out` points to writable Vec3 storage.
        unsafe { *out = a + b };
        Status::Ok
    })
}

// ---------------------------------------------------------------------------
// Contexts
// ---------------------------------------------------------------------------

/// Create a tile context from per-cell walkability (spec §6.1 tier 1).
///
/// `walkable` holds `width * height * layers` bytes, layer-major then row-major
/// (`index = layer * width * height + row * width + col`); nonzero = walkable.
/// On success `*out` receives a handle that must be released with [`ls_context_destroy`].
#[unsafe(no_mangle)]
pub extern "C" fn ls_tile_context_create(
    width: u32,
    height: u32,
    layers: u32,
    walkable: *const u8,
    walkable_len: usize,
    out: *mut *mut Context,
) -> Status {
    guard(|| {
        if out.is_null() || walkable.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `walkable` is readable for `walkable_len`
        // bytes for the duration of this call (the grid copies what it needs).
        let cells = unsafe { std::slice::from_raw_parts(walkable, walkable_len) };
        finish_create(TileGrid::from_walkable(width, height, layers, cells), out)
    })
}

/// Create a tile context from explicit directed edge masks (spec §6.1 tier 2).
///
/// `masks` holds one byte per node in the same layout as `ls_tile_context_create`; bit `d`
/// set means the node may be left in direction `d` (0 = south/+row, 1 = north/-row,
/// 2 = east/+col, 3 = west/-col). `overrides` (may be null when `overrides_len` is 0)
/// lists the edges that change layer or cost.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tile_context_create_edges(
    width: u32,
    height: u32,
    layers: u32,
    masks: *const u8,
    masks_len: usize,
    overrides: *const TileEdgeOverride,
    overrides_len: usize,
    out: *mut *mut Context,
) -> Status {
    guard(|| {
        if out.is_null() || masks.is_null() || (overrides.is_null() && overrides_len > 0) {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees both arrays are readable for their
        // stated lengths for the duration of this call.
        let masks = unsafe { std::slice::from_raw_parts(masks, masks_len) };
        let overrides: &[TileEdgeOverride] =
            if overrides_len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(overrides, overrides_len) } };

        let mut converted = Vec::with_capacity(overrides.len());
        for ov in overrides {
            let Some(dir) = Dir::from_u8(ov.dir) else { return Status::InvalidArgument };
            converted.push((ov.node, dir, EdgeOverride { target_layer: ov.target_layer, cost: ov.cost }));
        }
        finish_create(TileGrid::from_edges(width, height, layers, masks, &converted), out)
    })
}

fn finish_create(grid: Result<TileGrid, GridError>, out: *mut *mut Context) -> Status {
    match grid {
        Ok(grid) => {
            // SAFETY: callers check `out` for null; caller guarantees it is writable.
            unsafe { *out = Box::into_raw(Box::new(Context(TileContext::new(grid)))) };
            Status::Ok
        }
        Err(_) => Status::InvalidArgument,
    }
}

/// Release a context and everything it owns. Null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ls_context_destroy(ctx: *mut Context) {
    let _ = guard(|| {
        if !ctx.is_null() {
            // SAFETY: `ctx` came from `Box::into_raw` in a create function and the caller
            // promises not to use it after this call.
            drop(unsafe { Box::from_raw(ctx) });
        }
        Status::Ok
    });
}

// ---------------------------------------------------------------------------
// Nodes
// ---------------------------------------------------------------------------

/// Node id for `(col, row, layer)`. `InvalidArgument` if outside the grid.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_id(ctx: *const Context, col: u32, row: u32, layer: u32, out: *mut u32) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        match ctx.0.grid().node_id(col, row, layer) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(id) => unsafe {
                *out = id;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// `(col, row, layer)` of a node. Any out-pointer may be null to skip it.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_coords(
    ctx: *const Context,
    node: u32,
    out_col: *mut u32,
    out_row: *mut u32,
    out_layer: *mut u32,
) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        let Some((col, row, layer)) = ctx.0.grid().node_coords(node) else { return Status::InvalidArgument };
        // SAFETY: each out-pointer is written only if non-null; caller guarantees writability.
        unsafe {
            if !out_col.is_null() {
                *out_col = col;
            }
            if !out_row.is_null() {
                *out_row = row;
            }
            if !out_layer.is_null() {
                *out_layer = layer;
            }
        }
        Status::Ok
    })
}

/// Center position of a node. `InvalidArgument` if `node` is out of range.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_position(ctx: *const Context, node: u32, out: *mut Vec3) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        match ctx.0.grid().node_position(node) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(pos) => unsafe {
                *out = pos;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// The node containing `pos`. `InvalidArgument` if outside the grid.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_from_position(ctx: *const Context, pos: Vec3, out: *mut u32) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        match ctx.0.grid().node_from_position(pos) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(id) => unsafe {
                *out = id;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

// ---------------------------------------------------------------------------
// Edges and links (mutations)
// ---------------------------------------------------------------------------

fn with_ctx_mut(ctx: *mut Context, f: impl FnOnce(&mut TileContext) -> Status) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle, and the caller (per the thread
        // contract) is the only user of this context during the call.
        f(&mut unsafe { &mut *ctx }.0)
    })
}

fn with_grid(ctx: *mut Context, f: impl FnOnce(&mut TileGrid) -> Result<(), GridError>) -> Status {
    with_ctx_mut(ctx, |c| status_of(f(c.grid_mut())))
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// Register an agent profile; `*out` receives its id for `PathOptions.profile`.
/// `InvalidArgument` if the footprint is 0 or over 255.
#[unsafe(no_mangle)]
pub extern "C" fn ls_profile_register(ctx: *mut Context, profile: ProfileDesc, out: *mut u32) -> Status {
    with_ctx_mut(ctx, |c| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let p = Profile { footprint: profile.footprint, radius: profile.radius, height: profile.height, step_height: profile.step_height };
        match c.register_profile(p) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(id) => unsafe {
                *out = id;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// Clearance of a node: the largest square footprint that fits anchored there (0 = wall).
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_clearance(ctx: *const Context, node: u32, out: *mut u32) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let grid = unsafe { &*ctx }.0.grid();
        if grid.node_coords(node).is_none() {
            return Status::InvalidArgument;
        }
        // SAFETY: `out` non-null checked; caller guarantees it is writable.
        unsafe { *out = grid.clearance(node) as u32 };
        Status::Ok
    })
}

// ---------------------------------------------------------------------------
// Node groups (blocking overlay)
// ---------------------------------------------------------------------------

/// Group bitmask of a node. `InvalidArgument` if out of range.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_get_groups(ctx: *const Context, node: u32, out: *mut u32) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        match unsafe { &*ctx }.0.groups(node) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(mask) => unsafe {
                *out = mask;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// Replace a node's groups.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_set_groups(ctx: *mut Context, node: u32, mask: u32) -> Status {
    with_ctx_mut(ctx, |c| if c.set_groups(node, mask) { Status::Ok } else { Status::InvalidArgument })
}

/// OR `mask` into a node's groups.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_add_groups(ctx: *mut Context, node: u32, mask: u32) -> Status {
    with_ctx_mut(ctx, |c| if c.add_groups(node, mask) { Status::Ok } else { Status::InvalidArgument })
}

/// Clear `mask` bits from a node's groups.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_remove_groups(ctx: *mut Context, node: u32, mask: u32) -> Status {
    with_ctx_mut(ctx, |c| if c.remove_groups(node, mask) { Status::Ok } else { Status::InvalidArgument })
}

/// Clear `mask` bits from every node.
#[unsafe(no_mangle)]
pub extern "C" fn ls_groups_clear(ctx: *mut Context, mask: u32) -> Status {
    with_ctx_mut(ctx, |c| {
        c.clear_groups(mask);
        Status::Ok
    })
}

/// Open or close one directed edge (`node` leaving in `dir`). The reverse edge is untouched.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tile_set_edge(ctx: *mut Context, node: u32, dir: u8, passable: bool) -> Status {
    with_grid(ctx, |g| {
        let dir = Dir::from_u8(dir).ok_or(GridError::InvalidNode(node))?;
        g.set_edge(node, dir, passable)
    })
}

/// Set the layer/cost override for one directed edge (ramps, terrain costs).
#[unsafe(no_mangle)]
pub extern "C" fn ls_tile_set_edge_override(ctx: *mut Context, node: u32, dir: u8, target_layer: u32, cost: f32) -> Status {
    with_grid(ctx, |g| {
        let dir = Dir::from_u8(dir).ok_or(GridError::InvalidNode(node))?;
        g.set_edge_override(node, dir, EdgeOverride { target_layer, cost })
    })
}

/// Remove an override, restoring "same layer, cost 1". Clearing a missing override is `Ok`.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tile_clear_edge_override(ctx: *mut Context, node: u32, dir: u8) -> Status {
    with_grid(ctx, |g| {
        let dir = Dir::from_u8(dir).ok_or(GridError::InvalidNode(node))?;
        g.clear_edge_override(node, dir);
        Ok(())
    })
}

/// Add a link, or replace the one with the same id. Endpoints must be inside the grid.
#[unsafe(no_mangle)]
pub extern "C" fn ls_link_add(ctx: *mut Context, link: LinkDesc) -> Status {
    with_grid(ctx, |g| {
        let from = g.node_from_position(link.from).ok_or(GridError::InvalidNode(u32::MAX))?;
        let to = g.node_from_position(link.to).ok_or(GridError::InvalidNode(u32::MAX))?;
        g.add_link(
            link.id,
            Link {
                from,
                to,
                cost: link.cost,
                bidirectional: link.flags & LinkFlags::Bidirectional as u32 != 0,
                enabled: link.flags & LinkFlags::StartDisabled as u32 == 0,
            },
        )
    })
}

/// Remove a link. `InvalidArgument` if the id is unknown.
#[unsafe(no_mangle)]
pub extern "C" fn ls_link_remove(ctx: *mut Context, id: u64) -> Status {
    with_grid(ctx, |g| g.remove_link(id as LinkId))
}

/// Enable or disable a link without removing it (unpowered teleporter, locked door).
#[unsafe(no_mangle)]
pub extern "C" fn ls_link_set_enabled(ctx: *mut Context, id: u64, enabled: bool) -> Status {
    with_grid(ctx, |g| g.set_link_enabled(id as LinkId, enabled))
}

// ---------------------------------------------------------------------------
// Entities
// ---------------------------------------------------------------------------

/// Insert or replace one entity by id. `InvalidArgument` for an unknown bounds kind.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_upsert(ctx: *mut Context, entity: *const EntityDesc) -> Status {
    with_ctx_mut(ctx, |c| {
        if entity.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `entity` points to a readable EntityDesc.
        match unsafe { *entity }.to_core() {
            Some(e) => {
                c.upsert_entity(e);
                Status::Ok
            }
            None => Status::InvalidArgument,
        }
    })
}

/// Remove an entity. `InvalidArgument` if the id is unknown.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_remove(ctx: *mut Context, id: u64) -> Status {
    with_ctx_mut(ctx, |c| if c.remove_entity(id) { Status::Ok } else { Status::InvalidArgument })
}

/// Declarative snapshot sync (spec §5): upsert every record, remove every entity absent
/// from the snapshot. The core diffs, so re-sending unchanged records costs nothing.
/// `stats` may be null. Rejects the whole snapshot on any invalid bounds kind.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entities_sync(
    ctx: *mut Context,
    entities: *const EntityDesc,
    entities_len: usize,
    stats: *mut SyncStats,
) -> Status {
    with_ctx_mut(ctx, |c| {
        if entities.is_null() && entities_len > 0 {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; caller guarantees `entities` is readable for `entities_len`.
        let descs: &[EntityDesc] = if entities_len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(entities, entities_len) } };
        let mut converted = Vec::with_capacity(descs.len());
        for d in descs {
            let Some(e) = d.to_core() else { return Status::InvalidArgument };
            converted.push(e);
        }
        let result: SyncStats = c.sync_entities(&converted).into();
        if !stats.is_null() {
            // SAFETY: non-null checked; caller guarantees `stats` is writable.
            unsafe { *stats = result };
        }
        Status::Ok
    })
}

/// Number of entities in the registry.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_count(ctx: *const Context) -> usize {
    if ctx.is_null() {
        return 0;
    }
    // SAFETY: non-null checked; a live context handle.
    unsafe { &*ctx }.0.entity_count()
}

/// Nodes an entity occupies (its bounds projected through the grid). Same buffer convention
/// as [`ls_path_nodes`]. `InvalidArgument` for an unknown id; an off-grid entity writes zero.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_nodes(ctx: *const Context, id: u64, out: *mut u32, capacity: usize, written: *mut usize) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        match unsafe { &*ctx }.0.entity_nodes(id) {
            Some(nodes) => copy_out(nodes, out, capacity, written),
            None => Status::InvalidArgument,
        }
    })
}

/// Closest point of an entity's bounds to `from` — pure geometry, no map involved.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_nearest_point(ctx: *const Context, id: u64, from: Vec3, out: *mut Vec3) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        match unsafe { &*ctx }.0.entity_nearest_point(id, from) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(p) => unsafe {
                *out = p;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// Path to an entity. Goal mode comes from the entity's `PathfindAdjacent` flag, not `opts`.
/// `InvalidArgument` for an unknown id; otherwise the [`ls_find_path`] result convention.
#[unsafe(no_mangle)]
pub extern "C" fn ls_find_path_to_entity(ctx: *const Context, start: Vec3, id: u64, opts: PathOptions, out: *mut *mut Path) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        let Some(opts) = opts.to_core() else { return Status::InvalidArgument };
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        if ctx.0.entity(id).is_none() {
            return Status::InvalidArgument;
        }
        finish_path(ctx.0.find_path_to_entity(start, id, opts), out)
    })
}

/// Whether an entity is reachable from `start`: any of its nodes, or any node one step away
/// from them, is reachable under `opts.blocked_mask`. Runs a fresh flood fill each call.
/// `NoPath` if `start` is off-grid; `InvalidArgument` for an unknown id.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_reachable(ctx: *const Context, start: Vec3, id: u64, opts: PathOptions, out: *mut bool) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        let Some(opts) = opts.to_core() else { return Status::InvalidArgument };
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        if ctx.0.entity(id).is_none() {
            return Status::InvalidArgument;
        }
        let Some(reached) = ctx.0.reachable(start, opts) else { return Status::NoPath };
        // SAFETY: `out` non-null checked; caller guarantees it is writable.
        unsafe { *out = ctx.0.entity_reachable(&reached, id).unwrap_or(false) };
        Status::Ok
    })
}

// ---------------------------------------------------------------------------
// Player, announcement data, trackers
// ---------------------------------------------------------------------------

/// Update the player's position (each frame). A real move invalidates cached reachability.
#[unsafe(no_mangle)]
pub extern "C" fn ls_player_set(ctx: *mut Context, position: Vec3) -> Status {
    with_ctx_mut(ctx, |c| {
        c.set_player_position(position);
        Status::Ok
    })
}

/// Bearing, elevation, distance and step count from the player to an entity's nearest point.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_vector(ctx: *const Context, id: u64, out: *mut EntityVector) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        match unsafe { &*ctx }.0.entity_vector(id) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(v) => unsafe {
                *out = EntityVector {
                    nearest_point: v.nearest_point,
                    bearing_deg: v.bearing_deg,
                    elevation_deg: v.elevation_deg,
                    distance: v.distance,
                    steps: v.steps,
                };
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// [`ls_entity_reachable`] from the player's position, using the reachability cache — cheap
/// to call per announcement. `NoPath` if the player is off-grid.
#[unsafe(no_mangle)]
pub extern "C" fn ls_entity_reachable_from_player(ctx: *const Context, id: u64, profile: u32, blocked_mask: u32, out: *mut bool) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = &unsafe { &*ctx }.0;
        if ctx.entity(id).is_none() || ctx.profile(profile).is_none() {
            return Status::InvalidArgument;
        }
        match ctx.entity_reachable_from_player(id, profile, blocked_mask) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(r) => unsafe {
                *out = r;
                Status::Ok
            },
            None => Status::NoPath,
        }
    })
}

/// Create a tracker with an initial filter list (may be empty; `filters` may then be null).
/// `include_all` adds an all-categories view ahead of the per-category views.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_create(
    ctx: *mut Context,
    filters: *const FilterDesc,
    filters_len: usize,
    include_all: bool,
    out: *mut u32,
) -> Status {
    with_ctx_mut(ctx, |c| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let Some(filters) = parse_filters(filters, filters_len) else { return Status::InvalidArgument };
        let id = c.tracker_create(Tracker::new(filters, include_all));
        // SAFETY: `out` non-null checked; caller guarantees it is writable.
        unsafe { *out = id };
        Status::Ok
    })
}

/// Destroy a tracker. `InvalidArgument` if unknown.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_destroy(ctx: *mut Context, tracker: u32) -> Status {
    with_ctx_mut(ctx, |c| if c.tracker_destroy(tracker) { Status::Ok } else { Status::InvalidArgument })
}

/// Replace a tracker's filter list wholesale. The selection is re-validated on the next action.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_set_filters(ctx: *mut Context, tracker: u32, filters: *const FilterDesc, filters_len: usize) -> Status {
    with_ctx_mut(ctx, |c| {
        let Some(filters) = parse_filters(filters, filters_len) else { return Status::InvalidArgument };
        match c.tracker_mut(tracker) {
            Some(t) => {
                t.set_filters(filters);
                Status::Ok
            }
            None => Status::InvalidArgument,
        }
    })
}

/// Enable or disable one filter by index (hotkey toggles).
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_set_filter_enabled(ctx: *mut Context, tracker: u32, index: usize, enabled: bool) -> Status {
    with_ctx_mut(ctx, |c| match c.tracker_mut(tracker) {
        Some(t) => {
            if t.set_filter_enabled(index, enabled) { Status::Ok } else { Status::InvalidArgument }
        }
        None => Status::InvalidArgument,
    })
}

/// Drive a tracker. `action` is a `TrackerAction` discriminant.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_act(ctx: *mut Context, tracker: u32, action: u32, out: *mut Selection) -> Status {
    with_ctx_mut(ctx, |c| {
        if out.is_null() {
            return Status::NullPointer;
        }
        let action = match action {
            0 => CoreAction::Next,
            1 => CoreAction::Prev,
            2 => CoreAction::CategoryNext,
            3 => CoreAction::CategoryPrev,
            4 => CoreAction::Current,
            _ => return Status::InvalidArgument,
        };
        match c.tracker_act(tracker, action) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(s) => unsafe {
                *out = Selection { entity_id: s.entity_id, category: s.category, index: s.index };
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

/// Visible entity count under the tracker's filters, in `category` or (`UINT32_MAX`) all.
#[unsafe(no_mangle)]
pub extern "C" fn ls_tracker_count(ctx: *const Context, tracker: u32, category: u32, out: *mut u32) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        let category = match category {
            u32::MAX => None,
            c => match u16::try_from(c) {
                Ok(c) => Some(c),
                Err(_) => return Status::InvalidArgument,
            },
        };
        // SAFETY: non-null checked; a live context handle.
        match unsafe { &*ctx }.0.tracker_count(tracker, category) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(n) => unsafe {
                *out = n;
                Status::Ok
            },
            None => Status::InvalidArgument,
        }
    })
}

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

fn finish_path(result: Option<loadstar_core::graph::Path>, out: *mut *mut Path) -> Status {
    match result {
        // SAFETY: callers check `out` for null; caller guarantees it is writable.
        Some(path) => unsafe {
            *out = Box::into_raw(Box::new(Path(path)));
            Status::Ok
        },
        None => unsafe {
            *out = std::ptr::null_mut();
            Status::NoPath
        },
    }
}

/// Find a path from the node containing `start` to the node containing `goal`.
///
/// On `Ok`, `*out` receives a path handle (release with [`ls_path_free`]).
/// On `NoPath`, `*out` is set to null and nothing needs freeing.
#[unsafe(no_mangle)]
pub extern "C" fn ls_find_path(ctx: *const Context, start: Vec3, goal: Vec3, opts: PathOptions, out: *mut *mut Path) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        let Some(opts) = opts.to_core() else { return Status::InvalidArgument };
        // SAFETY: non-null checked; a live context handle.
        finish_path(unsafe { &*ctx }.0.find_path(start, goal, opts), out)
    })
}

/// Find a path from the node containing `start` to the cheapest of `targets` (node ids),
/// interpreted per `opts.goal_mode`. Same result convention as [`ls_find_path`].
#[unsafe(no_mangle)]
pub extern "C" fn ls_find_path_to_nodes(
    ctx: *const Context,
    start: Vec3,
    targets: *const u32,
    targets_len: usize,
    opts: PathOptions,
    out: *mut *mut Path,
) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() || (targets.is_null() && targets_len > 0) {
            return Status::NullPointer;
        }
        let Some(opts) = opts.to_core() else { return Status::InvalidArgument };
        // SAFETY: non-null checked; caller guarantees `targets` is readable for `targets_len`.
        let targets: &[u32] = if targets_len == 0 { &[] } else { unsafe { std::slice::from_raw_parts(targets, targets_len) } };
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        let n = ctx.0.grid().width() as usize * ctx.0.grid().height() as usize * ctx.0.grid().layers() as usize;
        if targets.iter().any(|&t| t as usize >= n) {
            return Status::InvalidArgument;
        }
        finish_path(ctx.0.find_path_to_nodes(start, targets, opts), out)
    })
}

/// Reachability from the node containing `start`: one byte per node (nonzero = reachable),
/// same buffer convention as [`ls_path_nodes`]. `opts.goal_mode` is ignored.
/// `NoPath` if `start` is off-grid and cannot be snapped.
#[unsafe(no_mangle)]
pub extern "C" fn ls_reachable_nodes(
    ctx: *const Context,
    start: Vec3,
    opts: PathOptions,
    out: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        let Some(opts) = opts.to_core() else { return Status::InvalidArgument };
        // SAFETY: non-null checked; a live context handle.
        let Some(reached) = unsafe { &*ctx }.0.reachable(start, opts) else { return Status::NoPath };
        let bytes: Vec<u8> = reached.iter().map(|&r| r as u8).collect();
        copy_out(&bytes, out, capacity, written)
    })
}

/// Number of nodes in a path (both endpoints included). Zero for a null handle.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_len(path: *const Path) -> usize {
    if path.is_null() {
        return 0;
    }
    // SAFETY: non-null checked; a live path handle from `ls_find_path`.
    unsafe { &*path }.0.nodes.len()
}

/// Total traversal cost of a path. Zero for a null handle.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_cost(path: *const Path) -> f32 {
    if path.is_null() {
        return 0.0;
    }
    // SAFETY: non-null checked; a live path handle.
    unsafe { &*path }.0.cost
}

/// Copy the path's node ids into `out` (capacity `capacity`). `*written` receives the count
/// needed; if it exceeds `capacity`, nothing is copied and `BufferTooSmall` is returned.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_nodes(path: *const Path, out: *mut u32, capacity: usize, written: *mut usize) -> Status {
    guard(|| {
        if path.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live path handle.
        copy_out(&unsafe { &*path }.0.nodes, out, capacity, written)
    })
}

/// Copy the path's link traversals into `out`. Same buffer convention as [`ls_path_nodes`].
/// A path with no link traversals writes zero and returns `Ok`.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_links(path: *const Path, out: *mut PathLink, capacity: usize, written: *mut usize) -> Status {
    guard(|| {
        if path.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live path handle.
        let links: Vec<PathLink> = unsafe { &*path }
            .0
            .links
            .iter()
            .map(|&(index, link_id)| PathLink { link_id, index: index as u32, _reserved: 0 })
            .collect();
        copy_out(&links, out, capacity, written)
    })
}

/// Shared buffer convention: report the needed count, copy only if it fits.
fn copy_out<T: Copy>(items: &[T], out: *mut T, capacity: usize, written: *mut usize) -> Status {
    if written.is_null() {
        return Status::NullPointer;
    }
    // SAFETY: `written` non-null checked; caller guarantees it is writable.
    unsafe { *written = items.len() };
    if items.len() > capacity {
        return Status::BufferTooSmall;
    }
    if items.is_empty() {
        return Status::Ok;
    }
    if out.is_null() {
        return Status::NullPointer;
    }
    // SAFETY: `out` non-null and the caller guarantees `capacity >= items.len()` writable slots.
    unsafe { std::ptr::copy_nonoverlapping(items.as_ptr(), out, items.len()) };
    Status::Ok
}

/// Release a path. Null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_free(path: *mut Path) {
    let _ = guard(|| {
        if !path.is_null() {
            // SAFETY: `path` came from `Box::into_raw` in `ls_find_path`; caller won't reuse it.
            drop(unsafe { Box::from_raw(path) });
        }
        Status::Ok
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn abi_struct_sizes_are_pinned() {
        assert_eq!(std::mem::size_of::<Vec3>(), 12);
        assert_eq!(std::mem::size_of::<TileEdgeOverride>(), 16);
        assert_eq!(std::mem::size_of::<LinkDesc>(), 40);
        assert_eq!(std::mem::size_of::<PathLink>(), 16);
        assert_eq!(std::mem::size_of::<Status>(), 4);
        assert_eq!(std::mem::size_of::<PathOptions>(), 16);
        assert_eq!(std::mem::size_of::<ProfileDesc>(), 16);
        assert_eq!(std::mem::size_of::<FilterDesc>(), 32);
        assert_eq!(std::mem::size_of::<Selection>(), 16);
        assert_eq!(std::mem::size_of::<EntityVector>(), 28);
        assert_eq!(std::mem::size_of::<BoundsDesc>(), 40);
        assert_eq!(std::mem::size_of::<EntityDesc>(), 64);
        assert_eq!(std::mem::size_of::<SyncStats>(), 16);
    }

    #[test]
    fn vec3_add_roundtrip() {
        let mut out = Vec3::default();
        let s = ls_vec3_add(Vec3::new(1.0, 2.0, 3.0), Vec3::new(1.0, 1.0, 1.0), &mut out);
        assert_eq!(s, Status::Ok);
        assert_eq!(out, Vec3::new(2.0, 3.0, 4.0));
    }

    #[test]
    fn null_out_is_rejected_not_dereferenced() {
        assert_eq!(ls_vec3_add(Vec3::default(), Vec3::default(), std::ptr::null_mut()), Status::NullPointer);
    }

    #[test]
    fn panics_become_status() {
        assert_eq!(guard(|| panic!("boom")), Status::Panic);
    }

    const DEFAULT_OPTS: PathOptions = PathOptions { blocked_mask: 0, snap_radius: 0, goal_mode: 0, profile: 0 };

    fn cell(col: f32, row: f32) -> Vec3 {
        Vec3::new(col + 0.5, 0.0, row + 0.5)
    }

    /// Full handle lifecycle through the C surface: 3x3 grid with a center wall.
    #[test]
    fn context_and_path_lifecycle() {
        let cells = [1u8, 1, 1, 1, 0, 1, 1, 1, 1];
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(3, 3, 1, cells.as_ptr(), cells.len(), &mut ctx), Status::Ok);
        assert!(!ctx.is_null());

        let mut path = std::ptr::null_mut();
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), cell(2.0, 2.0), DEFAULT_OPTS, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 5);
        assert_eq!(ls_path_cost(path), 4.0);

        let mut buf = [0u32; 2];
        let mut n = 0usize;
        assert_eq!(ls_path_nodes(path, buf.as_mut_ptr(), buf.len(), &mut n), Status::BufferTooSmall);
        assert_eq!(n, 5);

        let mut buf = [0u32; 8];
        assert_eq!(ls_path_nodes(path, buf.as_mut_ptr(), buf.len(), &mut n), Status::Ok);
        assert_eq!(&buf[..n], &[0, 1, 2, 5, 8]);

        let mut links = [PathLink { link_id: 0, index: 0, _reserved: 0 }; 1];
        assert_eq!(ls_path_links(path, links.as_mut_ptr(), 1, &mut n), Status::Ok);
        assert_eq!(n, 0);

        let mut pos = Vec3::default();
        assert_eq!(ls_node_position(ctx, 8, &mut pos), Status::Ok);
        assert_eq!(pos, cell(2.0, 2.0));
        assert_eq!(ls_node_position(ctx, 9, &mut pos), Status::InvalidArgument);

        let (mut c, mut r, mut l) = (0, 0, 0);
        assert_eq!(ls_node_coords(ctx, 7, &mut c, &mut r, &mut l), Status::Ok);
        assert_eq!((c, r, l), (1, 2, 0));
        let mut id = 0;
        assert_eq!(ls_node_id(ctx, 1, 2, 0, &mut id), Status::Ok);
        assert_eq!(id, 7);
        assert_eq!(ls_node_from_position(ctx, cell(1.0, 2.0), &mut id), Status::Ok);
        assert_eq!(id, 7);

        ls_path_free(path);
        ls_context_destroy(ctx);
    }

    #[test]
    fn edges_overrides_and_links_through_the_abi() {
        // 4x1, two layers, tier-2 masks: everything open on both layers.
        let open = (1 << Dir::East as u8) | (1 << Dir::West as u8);
        let masks = [open; 8];
        // Ramp: leaving node 1 eastward lands on layer 1.
        let ramp = TileEdgeOverride { node: 1, dir: Dir::East as u8, target_layer: 1, cost: 1.0 };
        let mut ctx = std::ptr::null_mut();
        assert_eq!(
            ls_tile_context_create_edges(4, 1, 2, masks.as_ptr(), masks.len(), &ramp, 1, &mut ctx),
            Status::Ok
        );

        let upper = |col: f32| Vec3::new(col + 0.5, 1.0, 0.5);
        let mut path = std::ptr::null_mut();
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), DEFAULT_OPTS, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 4);
        ls_path_free(path);

        // Close the ramp edge; the layers disconnect.
        assert_eq!(ls_tile_set_edge(ctx, 1, Dir::East as u8, false), Status::Ok);
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), DEFAULT_OPTS, &mut path), Status::NoPath);

        // A teleporter link bridges them instead, starting disabled.
        let link = LinkDesc {
            id: 9001,
            from: cell(0.0, 0.0),
            to: upper(3.0),
            cost: 2.0,
            flags: LinkFlags::Bidirectional as u32 | LinkFlags::StartDisabled as u32,
        };
        assert_eq!(ls_link_add(ctx, link), Status::Ok);
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), DEFAULT_OPTS, &mut path), Status::NoPath);
        assert_eq!(ls_link_set_enabled(ctx, 9001, true), Status::Ok);
        assert_eq!(ls_find_path(ctx, upper(3.0), cell(0.0, 0.0), DEFAULT_OPTS, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 2);
        let mut links = [PathLink { link_id: 0, index: 0, _reserved: 0 }; 2];
        let mut n = 0;
        assert_eq!(ls_path_links(path, links.as_mut_ptr(), 2, &mut n), Status::Ok);
        assert_eq!(n, 1);
        assert_eq!((links[0].link_id, links[0].index), (9001, 1));
        ls_path_free(path);

        assert_eq!(ls_link_remove(ctx, 9001), Status::Ok);
        assert_eq!(ls_link_remove(ctx, 9001), Status::InvalidArgument);
        assert_eq!(ls_tile_set_edge(ctx, 0, 7, true), Status::InvalidArgument, "bad dir");
        assert_eq!(ls_tile_set_edge_override(ctx, 0, 2, 9, 1.0), Status::InvalidArgument, "bad layer");
        ls_context_destroy(ctx);
    }

    #[test]
    fn unreachable_reports_no_path_with_null_handle() {
        let cells = [1u8, 0, 1];
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(3, 1, 1, cells.as_ptr(), cells.len(), &mut ctx), Status::Ok);
        let mut path: *mut Path = std::ptr::dangling_mut();
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), cell(2.0, 0.0), DEFAULT_OPTS, &mut path), Status::NoPath);
        assert!(path.is_null());
        ls_context_destroy(ctx);
    }

    #[test]
    fn groups_goal_modes_and_reachability_through_the_abi() {
        const NPC: u32 = 1;
        // 5x3 open corridor; NPC at (2,1).
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(5, 3, 1, [1u8; 15].as_ptr(), 15, &mut ctx), Status::Ok);
        let npc = 7;
        assert_eq!(ls_node_add_groups(ctx, npc, NPC), Status::Ok);
        let mut mask = 0;
        assert_eq!(ls_node_get_groups(ctx, npc, &mut mask), Status::Ok);
        assert_eq!(mask, NPC);
        assert_eq!(ls_node_get_groups(ctx, 99, &mut mask), Status::InvalidArgument);

        let blocked = PathOptions { blocked_mask: NPC, snap_radius: 0, goal_mode: 0, profile: 0 };
        let mut path = std::ptr::null_mut();
        assert_eq!(ls_find_path(ctx, cell(0.0, 1.0), cell(4.0, 1.0), blocked, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 7, "detours around the NPC");
        ls_path_free(path);

        let adjacent = PathOptions { blocked_mask: NPC, snap_radius: 0, goal_mode: 1, profile: 0 };
        assert_eq!(ls_find_path_to_nodes(ctx, cell(0.0, 1.0), &npc, 1, adjacent, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 2, "stops next to the NPC: (0,1) -> (1,1)");
        ls_path_free(path);
        assert_eq!(ls_find_path_to_nodes(ctx, cell(0.0, 1.0), &99, 1, adjacent, &mut path), Status::InvalidArgument);
        let bad_mode = PathOptions { blocked_mask: 0, snap_radius: 0, goal_mode: 7, profile: 0 };
        assert_eq!(ls_find_path(ctx, cell(0.0, 1.0), cell(4.0, 1.0), bad_mode, &mut path), Status::InvalidArgument);

        let mut reach = [0u8; 15];
        let mut n = 0;
        assert_eq!(ls_reachable_nodes(ctx, cell(0.0, 0.0), blocked, reach.as_mut_ptr(), 15, &mut n), Status::Ok);
        assert_eq!(n, 15);
        assert_eq!(reach[npc as usize], 0);
        assert_eq!(reach[14], 1, "corridor around the NPC is open");
        assert_eq!(ls_groups_clear(ctx, NPC), Status::Ok);
        assert_eq!(ls_node_get_groups(ctx, npc, &mut mask), Status::Ok);
        assert_eq!(mask, 0);
        ls_context_destroy(ctx);
    }

    fn tile_rect(col: u32, row: u32, w: u32, h: u32) -> BoundsDesc {
        BoundsDesc {
            kind: BoundsKind::TileRect as u32,
            layer: 0,
            ref_id: 0,
            _reserved: 0,
            a: Vec3::new(col as f32, 0.0, row as f32),
            b: Vec3::new(w as f32, 0.0, h as f32),
        }
    }

    fn entity(id: u64, bounds: BoundsDesc, blocking: u32, flags: u32) -> EntityDesc {
        EntityDesc { id, category: 1, _reserved: 0, blocking, flags, sort_key: 0, bounds }
    }

    #[test]
    fn entities_through_the_abi() {
        const NPC: u32 = 1;
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(5, 3, 1, [1u8; 15].as_ptr(), 15, &mut ctx), Status::Ok);

        let npc = entity(10, tile_rect(2, 1, 1, 1), NPC, EntityFlags::PathfindAdjacent as u32);
        let chest = entity(20, tile_rect(4, 2, 1, 1), 0, 0);
        let snapshot = [npc, chest];
        let mut stats = SyncStats::default();
        assert_eq!(ls_entities_sync(ctx, snapshot.as_ptr(), 2, &mut stats), Status::Ok);
        assert_eq!(stats, SyncStats { added: 2, updated: 0, removed: 0, _reserved: 0 });
        assert_eq!(ls_entity_count(ctx), 2);

        let mut mask = 0;
        assert_eq!(ls_node_get_groups(ctx, 7, &mut mask), Status::Ok);
        assert_eq!(mask, NPC, "projection populated the blocking overlay");

        let mut nodes = [0u32; 4];
        let mut n = 0;
        assert_eq!(ls_entity_nodes(ctx, 10, nodes.as_mut_ptr(), 4, &mut n), Status::Ok);
        assert_eq!(&nodes[..n], &[7]);
        assert_eq!(ls_entity_nodes(ctx, 99, nodes.as_mut_ptr(), 4, &mut n), Status::InvalidArgument);

        let opts = PathOptions { blocked_mask: NPC, snap_radius: 0, goal_mode: 0, profile: 0 };
        let mut path = std::ptr::null_mut();
        assert_eq!(ls_find_path_to_entity(ctx, cell(0.0, 1.0), 10, opts, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 2, "adjacent flag: stops next to the NPC");
        ls_path_free(path);
        assert_eq!(ls_find_path_to_entity(ctx, cell(0.0, 1.0), 99, opts, &mut path), Status::InvalidArgument);

        let mut reachable = false;
        assert_eq!(ls_entity_reachable(ctx, cell(0.0, 1.0), 20, opts, &mut reachable), Status::Ok);
        assert!(reachable);

        let mut p = Vec3::default();
        assert_eq!(ls_entity_nearest_point(ctx, 20, cell(0.0, 0.0), &mut p), Status::Ok);
        assert_eq!(p, Vec3::new(4.0, 0.0, 2.0));

        // Moving the NPC by upsert frees the old cell; removing the chest shrinks the registry.
        let moved = entity(10, tile_rect(2, 0, 1, 1), NPC, EntityFlags::PathfindAdjacent as u32);
        assert_eq!(ls_entity_upsert(ctx, &moved), Status::Ok);
        assert_eq!(ls_node_get_groups(ctx, 7, &mut mask), Status::Ok);
        assert_eq!(mask, 0);
        assert_eq!(ls_entity_remove(ctx, 20), Status::Ok);
        assert_eq!(ls_entity_remove(ctx, 20), Status::InvalidArgument);
        assert_eq!(ls_entity_count(ctx), 1);

        let bad = entity(30, BoundsDesc { kind: 9, ..tile_rect(0, 0, 1, 1) }, 0, 0);
        assert_eq!(ls_entity_upsert(ctx, &bad), Status::InvalidArgument);
        assert_eq!(ls_entities_sync(ctx, std::ptr::null(), 0, std::ptr::null_mut()), Status::Ok, "empty snapshot clears");
        assert_eq!(ls_entity_count(ctx), 0);
        ls_context_destroy(ctx);
    }

    #[test]
    fn profiles_through_the_abi() {
        // 4x3 open grid with a wall at (1,1).
        let mut w = [1u8; 12];
        w[5] = 0;
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(4, 3, 1, w.as_ptr(), 12, &mut ctx), Status::Ok);

        let mut clearance = 0;
        assert_eq!(ls_node_clearance(ctx, 2, &mut clearance), Status::Ok);
        assert_eq!(clearance, 2);
        assert_eq!(ls_node_clearance(ctx, 5, &mut clearance), Status::Ok);
        assert_eq!(clearance, 0);
        assert_eq!(ls_node_clearance(ctx, 99, &mut clearance), Status::InvalidArgument);

        let mut big = 0;
        let desc = ProfileDesc { footprint: 2, radius: 1.0, height: 2.0, step_height: 0.5 };
        assert_eq!(ls_profile_register(ctx, desc, &mut big), Status::Ok);
        assert_eq!(big, 1);
        assert_eq!(ls_profile_register(ctx, ProfileDesc { footprint: 0, ..desc }, &mut big), Status::InvalidArgument);

        let wide = PathOptions { blocked_mask: 0, snap_radius: 0, goal_mode: 0, profile: 1 };
        let mut path = std::ptr::null_mut();
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), cell(2.0, 1.0), wide, &mut path), Status::NoPath);
        assert_eq!(ls_find_path(ctx, cell(2.0, 0.0), cell(2.0, 1.0), wide, &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 2);
        ls_path_free(path);
        let unknown = PathOptions { profile: 7, ..wide };
        assert_eq!(ls_find_path(ctx, cell(2.0, 0.0), cell(2.0, 1.0), unknown, &mut path), Status::NoPath);
        ls_context_destroy(ctx);
    }

    #[test]
    fn trackers_through_the_abi() {
        const NPC: u32 = 1;
        // 5x3 corridor: NPC at (2,1), chest at (4,2), hidden chest at (1,2).
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(5, 3, 1, [1u8; 15].as_ptr(), 15, &mut ctx), Status::Ok);
        let mut hidden = entity(31, tile_rect(1, 2, 1, 1), 0, EntityFlags::Hidden as u32);
        hidden.category = 3;
        let mut chest = entity(30, tile_rect(4, 2, 1, 1), 0, 0);
        chest.category = 3;
        let snapshot = [entity(10, tile_rect(2, 1, 1, 1), NPC, EntityFlags::PathfindAdjacent as u32), chest, hidden];
        assert_eq!(ls_entities_sync(ctx, snapshot.as_ptr(), 3, std::ptr::null_mut()), Status::Ok);
        assert_eq!(ls_player_set(ctx, cell(0.0, 1.0)), Status::Ok);

        let no_hidden = FilterDesc { kind: 3, enabled: 1, p0: 0, p1: EntityFlags::Hidden as u32, mask: 0, value: 0.0, _reserved: 0 };
        let reachable = FilterDesc { kind: 0, enabled: 0, p0: 0, p1: NPC, mask: 0, value: 0.0, _reserved: 0 };
        let filters = [no_hidden, reachable];
        let mut t = 0;
        assert_eq!(ls_tracker_create(ctx, filters.as_ptr(), 2, true, &mut t), Status::Ok);

        let mut sel = Selection { entity_id: 0, category: 0, index: 0 };
        assert_eq!(ls_tracker_act(ctx, t, TrackerAction::Next as u32, &mut sel), Status::Ok);
        assert_eq!(sel, Selection { entity_id: 10, category: 1, index: 0 });
        assert_eq!(ls_tracker_act(ctx, t, TrackerAction::Next as u32, &mut sel), Status::Ok);
        assert_eq!(sel.entity_id, 30, "hidden chest 31 is filtered out");
        let mut n = 0;
        assert_eq!(ls_tracker_count(ctx, t, u32::MAX, &mut n), Status::Ok);
        assert_eq!(n, 2);
        assert_eq!(ls_tracker_count(ctx, t, 3, &mut n), Status::Ok);
        assert_eq!(n, 1);

        let mut v = EntityVector { nearest_point: Vec3::default(), bearing_deg: 0.0, elevation_deg: 0.0, distance: 0.0, steps: 0.0 };
        assert_eq!(ls_entity_vector(ctx, 30, &mut v), Status::Ok);
        assert_eq!(v.nearest_point, Vec3::new(4.0, 0.0, 2.0));
        assert!(v.bearing_deg > 90.0 && v.bearing_deg < 180.0, "south-east, got {}", v.bearing_deg);
        assert_eq!(v.steps, 4.0);

        let mut r = false;
        assert_eq!(ls_entity_reachable_from_player(ctx, 30, 0, NPC, &mut r), Status::Ok);
        assert!(r);
        assert_eq!(ls_entity_reachable_from_player(ctx, 30, 9, NPC, &mut r), Status::InvalidArgument, "unknown profile");

        assert_eq!(ls_tracker_act(ctx, t, TrackerAction::CategoryNext as u32, &mut sel), Status::Ok);
        assert_eq!(sel, Selection { entity_id: 0, category: 1, index: 0 });
        assert_eq!(ls_tracker_set_filter_enabled(ctx, t, 1, true), Status::Ok);
        assert_eq!(ls_tracker_set_filter_enabled(ctx, t, 5, true), Status::InvalidArgument);
        assert_eq!(ls_tracker_act(ctx, t, 9, &mut sel), Status::InvalidArgument);
        assert_eq!(ls_tracker_set_filters(ctx, t, std::ptr::null(), 0), Status::Ok);
        assert_eq!(ls_tracker_count(ctx, t, u32::MAX, &mut n), Status::Ok);
        assert_eq!(n, 3, "no filters: hidden chest visible again");
        assert_eq!(ls_tracker_destroy(ctx, t), Status::Ok);
        assert_eq!(ls_tracker_act(ctx, t, 0, &mut sel), Status::InvalidArgument);
        let bad = FilterDesc { kind: 9, ..no_hidden };
        assert_eq!(ls_tracker_create(ctx, &bad, 1, true, &mut t), Status::InvalidArgument);
        ls_context_destroy(ctx);
    }

    #[test]
    fn bad_grid_is_invalid_argument() {
        let cells = [1u8; 3];
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(2, 2, 1, cells.as_ptr(), cells.len(), &mut ctx), Status::InvalidArgument);
        ls_context_destroy(std::ptr::null_mut()); // null must be harmless
        ls_path_free(std::ptr::null_mut());
    }
}
