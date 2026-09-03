//! Loadstar core: the computational layers behind game accessibility navigation.
//!
//! Layers (each usable without the ones above it — see `docs/spec.md`):
//! 1. [`geometry`] — Vec3, bounds, nearest-point, bearing/distance
//! 2. [`graph`] — A*, flood fill, goal candidates, segment compression
//! 3. [`tile`] — tile grids (directed edges + links); nav meshes later
//! 4. [`entities`] — numeric records, snapshot diffing
//! 5. [`tracker`] — selection state machines with declarative filters
//!
//! [`context`] ties one map to the state that must stay consistent with it, and is where
//! entities meet the map (projection into node sets).
//!
//! This crate contains no `unsafe` code and no FFI; `loadstar-ffi` is the C ABI.

#![forbid(unsafe_code)]

pub mod context;
pub mod entities;
pub mod geometry;
pub mod graph;
pub mod tile;
pub mod tracker;
