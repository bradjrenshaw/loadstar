//! Loadstar C ABI.
//!
//! Rules of this crate (see `docs/spec.md` §9):
//! - Every exported function is `extern "C"`, takes flat data, and returns a [`Status`].
//! - No callbacks. No strings. No panics escape — every entry point runs inside [`guard`].
//! - Handles are opaque pointers owned by the library; each has a matching `*_destroy`/`*_free`.
//! - This is the only crate in the workspace that may contain `unsafe`.

use std::panic::{AssertUnwindSafe, catch_unwind};

use loadstar_core::context::TileContext;
use loadstar_core::graph::NodeId;
use loadstar_core::tile::TileGrid;
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

/// Opaque: a path result, a sequence of node ids (spec §8). Exported to C as `LsPath`.
pub struct Path(Vec<NodeId>);

/// Run an ABI body, converting any panic into [`Status::Panic`] instead of unwinding into the host.
fn guard(f: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(f)).unwrap_or(Status::Panic)
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
        // SAFETY: non-null checked; caller guarantees `walkable` is readable for `walkable_len` bytes
        // for the duration of this call (the grid copies what it needs).
        let cells = unsafe { std::slice::from_raw_parts(walkable, walkable_len) };
        match TileGrid::from_walkable(width, height, layers, cells) {
            Ok(grid) => {
                let handle = Box::new(Context(TileContext::new(grid)));
                // SAFETY: `out` non-null checked; caller guarantees it is writable.
                unsafe { *out = Box::into_raw(handle) };
                Status::Ok
            }
            Err(_) => Status::InvalidArgument,
        }
    })
}

/// Release a context and everything it owns. Null is a no-op.
#[unsafe(no_mangle)]
pub extern "C" fn ls_context_destroy(ctx: *mut Context) {
    let _ = guard(|| {
        if !ctx.is_null() {
            // SAFETY: `ctx` came from `Box::into_raw` in `ls_tile_context_create` and the
            // caller promises not to use it after this call.
            drop(unsafe { Box::from_raw(ctx) });
        }
        Status::Ok
    });
}

/// Center position of a node. `InvalidArgument` if `node` is out of range.
#[unsafe(no_mangle)]
pub extern "C" fn ls_node_position(ctx: *const Context, node: u32, out: *mut Vec3) -> Status {
    guard(|| {
        if ctx.is_null() || out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live context handle from `ls_tile_context_create`.
        let ctx = unsafe { &*ctx };
        match ctx.0.grid().node_position(node) {
            Some(pos) => {
                // SAFETY: `out` non-null checked; caller guarantees it is writable.
                unsafe { *out = pos };
                Status::Ok
            }
            None => Status::InvalidArgument,
        }
    })
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
            Some(nodes) => {
                // SAFETY: `out` non-null checked; caller guarantees it is writable.
                unsafe { *out = Box::into_raw(Box::new(Path(nodes))) };
                Status::Ok
            }
            None => {
                // SAFETY: as above.
                unsafe { *out = std::ptr::null_mut() };
                Status::NoPath
            }
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
    unsafe { &*path }.0.len()
}

/// Copy the path's node ids into `out` (capacity `capacity`). `*written` receives the
/// count actually needed; if it exceeds `capacity`, nothing is copied and `BufferTooSmall` is returned.
#[unsafe(no_mangle)]
pub extern "C" fn ls_path_nodes(path: *const Path, out: *mut u32, capacity: usize, written: *mut usize) -> Status {
    guard(|| {
        if path.is_null() || written.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: non-null checked; a live path handle.
        let nodes = &unsafe { &*path }.0;
        // SAFETY: `written` non-null checked; caller guarantees it is writable.
        unsafe { *written = nodes.len() };
        if nodes.len() > capacity {
            return Status::BufferTooSmall;
        }
        if nodes.is_empty() {
            return Status::Ok;
        }
        if out.is_null() {
            return Status::NullPointer;
        }
        // SAFETY: `out` non-null and caller guarantees `capacity >= nodes.len()` writable slots.
        unsafe { std::ptr::copy_nonoverlapping(nodes.as_ptr(), out, nodes.len()) };
        Status::Ok
    })
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

    /// Full handle lifecycle through the C surface: 3x3 grid with a center wall.
    #[test]
    fn context_and_path_lifecycle() {
        let cells = [1u8, 1, 1, 1, 0, 1, 1, 1, 1];
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(3, 3, 1, cells.as_ptr(), cells.len(), &mut ctx), Status::Ok);
        assert!(!ctx.is_null());

        let mut path = std::ptr::null_mut();
        let s = ls_find_path(ctx, Vec3::new(0.5, 0.0, 0.5), Vec3::new(2.5, 0.0, 2.5), &mut path);
        assert_eq!(s, Status::Ok);
        assert_eq!(ls_path_len(path), 5);

        let mut buf = [0u32; 2];
        let mut n = 0usize;
        assert_eq!(ls_path_nodes(path, buf.as_mut_ptr(), buf.len(), &mut n), Status::BufferTooSmall);
        assert_eq!(n, 5);

        let mut buf = [0u32; 8];
        assert_eq!(ls_path_nodes(path, buf.as_mut_ptr(), buf.len(), &mut n), Status::Ok);
        assert_eq!(&buf[..n], &[0, 1, 2, 5, 8]);

        let mut pos = Vec3::default();
        assert_eq!(ls_node_position(ctx, 8, &mut pos), Status::Ok);
        assert_eq!(pos, Vec3::new(2.5, 0.0, 2.5));
        assert_eq!(ls_node_position(ctx, 9, &mut pos), Status::InvalidArgument);

        ls_path_free(path);
        ls_context_destroy(ctx);
    }

    #[test]
    fn unreachable_reports_no_path_with_null_handle() {
        let cells = [1u8, 0, 1];
        let mut ctx = std::ptr::null_mut();
        assert_eq!(ls_tile_context_create(3, 1, 1, cells.as_ptr(), cells.len(), &mut ctx), Status::Ok);
        let mut path: *mut Path = std::ptr::dangling_mut();
        assert_eq!(ls_find_path(ctx, Vec3::new(0.5, 0.0, 0.5), Vec3::new(2.5, 0.0, 0.5), &mut path), Status::NoPath);
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
