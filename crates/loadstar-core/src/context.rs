//! A context bundles one map with everything that must stay consistent with it
//! (spec §4). Right now that is just the grid; entities, profiles, trackers and
//! caches attach here as they are built.

use crate::geometry::Vec3;
use crate::graph::{Path, astar};
use crate::tile::TileGrid;

pub struct TileContext {
    grid: TileGrid,
}

impl TileContext {
    pub fn new(grid: TileGrid) -> Self {
        TileContext { grid }
    }

    pub fn grid(&self) -> &TileGrid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut TileGrid {
        &mut self.grid
    }

    /// Path between the nodes containing `start` and `goal` (spec §8).
    /// `None` if either position is off-grid or no route exists.
    pub fn find_path(&self, start: Vec3, goal: Vec3) -> Option<Path> {
        let s = self.grid.node_from_position(start)?;
        let g = self.grid.node_from_position(goal)?;
        astar(&self.grid, s, g)
    }
}
