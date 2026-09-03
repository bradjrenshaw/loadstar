//! Loadstar core: the computational layers behind game accessibility navigation.
//!
//! Layers (each usable without the ones above it — see `docs/spec.md`):
//! 1. [`geometry`] — Vec3, bounds, nearest-point, bearing/distance
//! 2. graph algorithms — A*, flood fill, goal candidates, segment compression
//! 3. map types — tile grids (directed edges + links), nav meshes
//! 4. entity registry — numeric records, snapshot diffing
//! 5. guide — trackers, filters, reachability
//!
//! This crate contains no `unsafe` code and no FFI; `loadstar-ffi` is the C ABI.

#![forbid(unsafe_code)]

pub mod geometry;
