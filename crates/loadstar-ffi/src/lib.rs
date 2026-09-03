//! Loadstar C ABI.
//!
//! Rules of this crate (see `docs/spec.md` §9):
//! - Every exported function is `extern "C"`, takes flat data, and returns a [`Status`].
//! - No callbacks. No strings. No panics escape — every entry point runs inside [`guard`].
//! - Handles are opaque pointers owned by the library; each has a matching `*_destroy`/`*_free`.
//! - `bool` parameters are C `_Bool` (one byte); bindings must marshal them as such.
//! - This is the only crate in the workspace that may contain `unsafe`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use loadstar_core::context::TileContext;
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

fn with_grid(ctx: *mut Context, f: impl FnOnce(&mut TileGrid) -> Result<(), GridError>) -> Status {
    guard(|| {
        if ctx.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle, and the caller (per the thread
        // contract) is the only user of this context during the call.
        let ctx = unsafe { &mut *ctx };
        status_of(f(ctx.0.grid_mut()))
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
// Paths
// ---------------------------------------------------------------------------

/// Find a path between the nodes containing `start` and `goal`.
///
/// On `Ok`, `*out` receives a path handle (release with [`ls_path_free`]).
/// On `NoPath`, `*out` is set to null and nothing needs freeing.
#[unsafe(no_mangle)]
pub extern "C" fn ls_find_path(ctx: *const Context, start: Vec3, goal: Vec3, out: *mut *mut Path) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle.
        let ctx = unsafe { &*ctx };
        match ctx.0.find_path(start, goal) {
            // SAFETY: `out` non-null checked; caller guarantees it is writable.
            Some(path) => unsafe {
                *out = Box::into_raw(Box::new(Path(path)));
                Status::Ok
            },
            None => unsafe {
                *out = std::ptr::null_mut();
                Status::NoPath
            },
        }
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
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), cell(2.0, 2.0), &mut path), Status::Ok);
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
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), &mut path), Status::Ok);
        assert_eq!(ls_path_len(path), 4);
        ls_path_free(path);

        // Close the ramp edge; the layers disconnect.
        assert_eq!(ls_tile_set_edge(ctx, 1, Dir::East as u8, false), Status::Ok);
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), &mut path), Status::NoPath);

        // A teleporter link bridges them instead, starting disabled.
        let link = LinkDesc {
            id: 9001,
            from: cell(0.0, 0.0),
            to: upper(3.0),
            cost: 2.0,
            flags: LinkFlags::Bidirectional as u32 | LinkFlags::StartDisabled as u32,
        };
        assert_eq!(ls_link_add(ctx, link), Status::Ok);
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), upper(3.0), &mut path), Status::NoPath);
        assert_eq!(ls_link_set_enabled(ctx, 9001, true), Status::Ok);
        assert_eq!(ls_find_path(ctx, upper(3.0), cell(0.0, 0.0), &mut path), Status::Ok);
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
        assert_eq!(ls_find_path(ctx, cell(0.0, 0.0), cell(2.0, 0.0), &mut path), Status::NoPath);
        assert!(path.is_null());
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
