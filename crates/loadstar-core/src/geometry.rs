//! Layer 1: pure geometry. No map or entity knowledge.
//!
//! Coordinate convention: for tile maps, `x` = column, `z` = row, `y` = layer.
//! For 3D maps, `y` is world height.

use std::ops::{Add, Sub};

/// A position or displacement. `#[repr(C)]` so the FFI layer can pass it by value unchanged.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vec3 {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

impl Vec3 {
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Vec3 { x, y, z }
    }

    /// Euclidean length.
    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y + self.z * self.z).sqrt()
    }

    /// Length ignoring the vertical axis.
    pub fn horizontal_length(self) -> f32 {
        (self.x * self.x + self.z * self.z).sqrt()
    }

    /// Manhattan distance in the horizontal plane — the natural step count on a 4-connected grid.
    pub fn manhattan_horizontal(self) -> f32 {
        self.x.abs() + self.z.abs()
    }
}

impl Add for Vec3 {
    type Output = Vec3;
    fn add(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Vec3;
    fn sub(self, o: Vec3) -> Vec3 {
        Vec3::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

/// Geometry an entity occupies (spec §5 "Bounds"). Every variant is defined in 3D space;
/// tile contexts read `y` as the layer. Pure math — projection into nodes is the map's job.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Bounds {
    Point(Vec3),
    /// Grid-native rectangle of cells. `layer: None` = present on every layer.
    TileRect { col: u32, row: u32, width: u32, height: u32, layer: Option<u32> },
    /// Axis-aligned box; `extents` are half-sizes.
    Box { center: Vec3, extents: Vec3 },
    Sphere { center: Vec3, radius: f32 },
}

impl Bounds {
    pub fn center(&self) -> Vec3 {
        match *self {
            Bounds::Point(p) => p,
            Bounds::TileRect { col, row, width, height, layer } => Vec3::new(
                col as f32 + width as f32 / 2.0,
                layer.unwrap_or(0) as f32,
                row as f32 + height as f32 / 2.0,
            ),
            Bounds::Box { center, .. } | Bounds::Sphere { center, .. } => center,
        }
    }

    /// Closest point of the bounds to `from`. For an all-layers rect, the result keeps `from.y`.
    pub fn nearest_point(&self, from: Vec3) -> Vec3 {
        match *self {
            Bounds::Point(p) => p,
            Bounds::TileRect { col, row, width, height, layer } => Vec3::new(
                from.x.clamp(col as f32, (col + width) as f32),
                layer.map_or(from.y, |l| l as f32),
                from.z.clamp(row as f32, (row + height) as f32),
            ),
            Bounds::Box { center, extents } => Vec3::new(
                from.x.clamp(center.x - extents.x, center.x + extents.x),
                from.y.clamp(center.y - extents.y, center.y + extents.y),
                from.z.clamp(center.z - extents.z, center.z + extents.z),
            ),
            Bounds::Sphere { center, radius } => {
                let d = from - center;
                let len = d.length();
                if len <= radius || len == 0.0 {
                    from
                } else {
                    let s = radius / len;
                    Vec3::new(center.x + d.x * s, center.y + d.y * s, center.z + d.z * s)
                }
            }
        }
    }

    pub fn contains(&self, p: Vec3) -> bool {
        match *self {
            Bounds::Point(q) => (p - q).length() < 1e-4,
            Bounds::TileRect { col, row, width, height, layer } => {
                p.x >= col as f32
                    && p.x < (col + width) as f32
                    && p.z >= row as f32
                    && p.z < (row + height) as f32
                    && layer.is_none_or(|l| p.y.floor() == l as f32)
            }
            Bounds::Box { center, extents } => {
                (p.x - center.x).abs() <= extents.x && (p.y - center.y).abs() <= extents.y && (p.z - center.z).abs() <= extents.z
            }
            Bounds::Sphere { center, radius } => (p - center).length() <= radius,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tile_rect_nearest_point_and_containment() {
        let r = Bounds::TileRect { col: 2, row: 3, width: 2, height: 1, layer: Some(0) };
        assert_eq!(r.center(), Vec3::new(3.0, 0.0, 3.5));
        assert_eq!(r.nearest_point(Vec3::new(0.0, 7.0, 0.0)), Vec3::new(2.0, 0.0, 3.0));
        assert_eq!(r.nearest_point(Vec3::new(3.5, 0.0, 3.5)), Vec3::new(3.5, 0.0, 3.5));
        assert!(r.contains(Vec3::new(3.9, 0.5, 3.9)));
        assert!(!r.contains(Vec3::new(4.0, 0.5, 3.5)));
        assert!(!r.contains(Vec3::new(3.0, 1.5, 3.5)), "wrong layer");
        let all = Bounds::TileRect { col: 2, row: 3, width: 2, height: 1, layer: None };
        assert_eq!(all.nearest_point(Vec3::new(0.0, 1.0, 0.0)).y, 1.0, "all-layers keeps the caller's layer");
    }

    #[test]
    fn box_and_sphere_nearest_points() {
        let b = Bounds::Box { center: Vec3::new(0.0, 0.0, 0.0), extents: Vec3::new(1.0, 1.0, 1.0) };
        assert_eq!(b.nearest_point(Vec3::new(5.0, 0.0, -5.0)), Vec3::new(1.0, 0.0, -1.0));
        assert!(b.contains(Vec3::new(1.0, 1.0, 1.0)));
        let s = Bounds::Sphere { center: Vec3::new(0.0, 0.0, 0.0), radius: 2.0 };
        assert_eq!(s.nearest_point(Vec3::new(4.0, 0.0, 0.0)), Vec3::new(2.0, 0.0, 0.0));
        assert_eq!(s.nearest_point(Vec3::new(1.0, 0.0, 0.0)), Vec3::new(1.0, 0.0, 0.0), "inside: itself");
    }

    #[test]
    fn manhattan_ignores_vertical() {
        let d = Vec3::new(3.0, 7.0, -4.0);
        assert_eq!(d.manhattan_horizontal(), 7.0);
        assert_eq!(d.horizontal_length(), 5.0);
    }

    #[test]
    fn vec3_is_c_layout() {
        assert_eq!(std::mem::size_of::<Vec3>(), 12);
        assert_eq!(std::mem::align_of::<Vec3>(), 4);
    }
}
