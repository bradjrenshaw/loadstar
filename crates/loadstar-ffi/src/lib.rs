//! Loadstar C ABI.
//!
//! Rules of this crate (see `docs/spec.md` §9):
//! - Every exported function is `extern "C"`, takes flat data, and returns an [`Status`].
//! - No callbacks. No strings. No panics escape — every entry point runs inside [`guard`].
//! - This is the only crate in the workspace that may contain `unsafe`.

use std::panic::{AssertUnwindSafe, catch_unwind};

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
    /// Internal panic caught at the boundary. The context may be inconsistent; the host is intact.
    Panic = 100,
}

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
        // SAFETY: non-null checked above; caller guarantees `out` points to writable Vec3 storage.
        unsafe { *out = a + b };
        Status::Ok
    })
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
}
