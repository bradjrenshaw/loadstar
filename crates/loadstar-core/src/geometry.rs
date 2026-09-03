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

#[cfg(test)]
mod tests {
    use super::*;

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
