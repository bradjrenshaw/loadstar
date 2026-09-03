//! Layer 3: 2D tile grids with vertical layers and **directed** edges.
//!
//! A node is `(col, row, layer)`. Connectivity is stored per node as a 4-bit mask:
//! bit `d` set means "may step in direction `d` to the adjacent cell on the same layer".
//! That single representation expresses one-way ledges, asymmetric terrain rules,
//! and walls, and is what a game's movement rules get baked into once per map load.
//! Layer-changing edges, per-edge costs, and links are later additions (spec §6.1, §6.3).

use crate::geometry::Vec3;
use crate::graph::{Graph, NodeId};

/// Cardinal directions. The discriminant is the edge-mask bit index.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    South = 0, // +row
    North = 1, // -row
    East = 2,  // +col
    West = 3,  // -col
}

impl Dir {
    pub const ALL: [Dir; 4] = [Dir::South, Dir::North, Dir::East, Dir::West];

    /// `(dcol, drow)` for one step.
    pub const fn delta(self) -> (i32, i32) {
        match self {
            Dir::South => (0, 1),
            Dir::North => (0, -1),
            Dir::East => (1, 0),
            Dir::West => (-1, 0),
        }
    }

    const fn bit(self) -> u8 {
        1 << (self as u8)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum GridError {
    ZeroSize,
    /// `walkable` must hold exactly `width * height * layers` bytes.
    LengthMismatch { expected: usize, actual: usize },
}

#[derive(Debug)]
pub struct TileGrid {
    width: u32,
    height: u32,
    layers: u32,
    /// One mask per node, indexed by `NodeId`.
    edges: Vec<u8>,
}

impl TileGrid {
    /// Build from per-cell walkability (tier-1 upload, spec §6.1): an edge exists in both
    /// directions between adjacent walkable cells. Layout of `walkable` is layer-major,
    /// then row-major: index = `layer * width * height + row * width + col`; nonzero = walkable.
    pub fn from_walkable(width: u32, height: u32, layers: u32, walkable: &[u8]) -> Result<TileGrid, GridError> {
        if width == 0 || height == 0 || layers == 0 {
            return Err(GridError::ZeroSize);
        }
        let expected = width as usize * height as usize * layers as usize;
        if walkable.len() != expected {
            return Err(GridError::LengthMismatch { expected, actual: walkable.len() });
        }

        let mut grid = TileGrid { width, height, layers, edges: vec![0; expected] };
        for node in 0..expected as NodeId {
            if walkable[node as usize] == 0 {
                continue;
            }
            let mut mask = 0;
            for dir in Dir::ALL {
                if let Some(next) = grid.step(node, dir)
                    && walkable[next as usize] != 0
                {
                    mask |= dir.bit();
                }
            }
            grid.edges[node as usize] = mask;
        }
        Ok(grid)
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn layers(&self) -> u32 {
        self.layers
    }

    pub fn node_id(&self, col: u32, row: u32, layer: u32) -> Option<NodeId> {
        if col >= self.width || row >= self.height || layer >= self.layers {
            return None;
        }
        Some((layer * self.height + row) * self.width + col)
    }

    /// `(col, row, layer)` for a node.
    pub fn node_coords(&self, node: NodeId) -> Option<(u32, u32, u32)> {
        if node as usize >= self.edges.len() {
            return None;
        }
        let per_layer = self.width * self.height;
        let layer = node / per_layer;
        let rem = node % per_layer;
        Some((rem % self.width, rem / self.width, layer))
    }

    /// Cell center: `(col + 0.5, layer, row + 0.5)`.
    pub fn node_position(&self, node: NodeId) -> Option<Vec3> {
        let (col, row, layer) = self.node_coords(node)?;
        Some(Vec3::new(col as f32 + 0.5, layer as f32, row as f32 + 0.5))
    }

    /// The node containing a position (floor on x/z, layer from y). `None` if outside the grid.
    pub fn node_from_position(&self, pos: Vec3) -> Option<NodeId> {
        if pos.x < 0.0 || pos.z < 0.0 || pos.y < 0.0 {
            return None;
        }
        self.node_id(pos.x as u32, pos.z as u32, pos.y as u32)
    }

    pub fn can_move(&self, node: NodeId, dir: Dir) -> bool {
        self.edges.get(node as usize).is_some_and(|m| m & dir.bit() != 0)
    }

    /// Patch one directed edge (doors, switches). Only affects `node -> neighbor`, not the reverse.
    pub fn set_edge(&mut self, node: NodeId, dir: Dir, passable: bool) {
        if let Some(mask) = self.edges.get_mut(node as usize) {
            if passable {
                *mask |= dir.bit();
            } else {
                *mask &= !dir.bit();
            }
        }
    }

    /// Adjacent node in `dir` on the same layer, ignoring passability.
    fn step(&self, node: NodeId, dir: Dir) -> Option<NodeId> {
        let (col, row, layer) = self.node_coords(node)?;
        let (dc, dr) = dir.delta();
        let nc = col as i64 + dc as i64;
        let nr = row as i64 + dr as i64;
        if nc < 0 || nr < 0 {
            return None;
        }
        self.node_id(nc as u32, nr as u32, layer)
    }
}

impl Graph for TileGrid {
    fn node_count(&self) -> usize {
        self.edges.len()
    }

    fn neighbors(&self, node: NodeId, out: &mut Vec<(NodeId, f32)>) {
        let Some(&mask) = self.edges.get(node as usize) else { return };
        for dir in Dir::ALL {
            if mask & dir.bit() != 0
                && let Some(next) = self.step(node, dir)
            {
                out.push((next, 1.0));
            }
        }
    }

    /// Manhattan distance on the grid plane. Layer differences are ignored (still admissible).
    fn heuristic(&self, a: NodeId, b: NodeId) -> f32 {
        match (self.node_coords(a), self.node_coords(b)) {
            (Some((ac, ar, _)), Some((bc, br, _))) => {
                (ac as i64 - bc as i64).unsigned_abs() as f32 + (ar as i64 - br as i64).unsigned_abs() as f32
            }
            _ => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::astar;

    /// 5x5 open grid with a vertical wall down column 2, gap at the bottom row.
    fn walled() -> TileGrid {
        let mut w = vec![1u8; 25];
        for row in 0..4 {
            w[row * 5 + 2] = 0;
        }
        TileGrid::from_walkable(5, 5, 1, &w).unwrap()
    }

    #[test]
    fn rejects_bad_dimensions() {
        assert_eq!(TileGrid::from_walkable(0, 5, 1, &[]).unwrap_err(), GridError::ZeroSize);
        assert_eq!(
            TileGrid::from_walkable(2, 2, 1, &[1, 1, 1]).unwrap_err(),
            GridError::LengthMismatch { expected: 4, actual: 3 }
        );
    }

    #[test]
    fn node_ids_round_trip_across_layers() {
        let g = TileGrid::from_walkable(3, 2, 2, &[1; 12]).unwrap();
        let id = g.node_id(2, 1, 1).unwrap();
        assert_eq!(id, 11);
        assert_eq!(g.node_coords(id), Some((2, 1, 1)));
        assert_eq!(g.node_position(id), Some(Vec3::new(2.5, 1.0, 1.5)));
        assert_eq!(g.node_from_position(Vec3::new(2.9, 1.2, 1.1)), Some(id));
        assert_eq!(g.node_id(3, 0, 0), None);
        assert_eq!(g.node_from_position(Vec3::new(-0.1, 0.0, 0.0)), None);
    }

    #[test]
    fn walls_have_no_edges_and_are_routed_around() {
        let g = walled();
        let wall = g.node_id(2, 0, 0).unwrap();
        assert!(!g.can_move(wall, Dir::East));
        assert!(!g.can_move(g.node_id(1, 0, 0).unwrap(), Dir::East));

        let start = g.node_id(0, 0, 0).unwrap();
        let goal = g.node_id(4, 0, 0).unwrap();
        let path = astar(&g, start, goal).expect("path around the wall");
        // Down 4, across 4, up 4 = 12 steps = 13 nodes, and it must pass through the gap.
        assert_eq!(path.len(), 13);
        assert!(path.contains(&g.node_id(2, 4, 0).unwrap()));
    }

    #[test]
    fn one_way_edge_is_honored() {
        let mut g = TileGrid::from_walkable(2, 1, 1, &[1, 1]).unwrap();
        let (a, b) = (g.node_id(0, 0, 0).unwrap(), g.node_id(1, 0, 0).unwrap());
        g.set_edge(b, Dir::West, false); // ledge: can go east, can't come back
        assert_eq!(astar(&g, a, b), Some(vec![a, b]));
        assert_eq!(astar(&g, b, a), None);
    }

    #[test]
    fn layers_without_layer_edges_are_disconnected() {
        let g = TileGrid::from_walkable(2, 2, 2, &[1; 8]).unwrap();
        assert_eq!(astar(&g, g.node_id(0, 0, 0).unwrap(), g.node_id(0, 0, 1).unwrap()), None);
    }
}
