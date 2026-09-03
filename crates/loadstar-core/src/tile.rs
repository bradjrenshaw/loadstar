//! Layer 3: 2D tile grids with vertical layers and **directed** edges (spec §6.1, §6.3).
//!
//! A node is `(col, row, layer)`. Connectivity has three parts:
//! - a dense 4-bit mask per node — bit `d` set means "may step in direction `d`";
//! - a sparse **override** table for the rare edges that change layer or cost;
//! - a sparse **link** list for non-adjacent connections (teleporters, ladders, elevators).
//!
//! The mask expresses one-way ledges and asymmetric terrain rules; overrides express ramps
//! and swamp costs; links express everything that isn't cell-to-adjacent-cell. All three
//! are what a game's movement rules get baked into once per map load.

use std::collections::HashMap;

use crate::geometry::Vec3;
use crate::graph::{Edge, Graph, LinkId, NodeId};

/// Cardinal directions. The discriminant is the edge-mask bit index.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Dir {
    South = 0, // +row
    North = 1, // -row
    East = 2,  // +col
    West = 3,  // -col
}

impl Dir {
    pub const ALL: [Dir; 4] = [Dir::South, Dir::North, Dir::East, Dir::West];

    pub const fn from_u8(v: u8) -> Option<Dir> {
        match v {
            0 => Some(Dir::South),
            1 => Some(Dir::North),
            2 => Some(Dir::East),
            3 => Some(Dir::West),
            _ => None,
        }
    }

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

/// Replaces the default "same layer, cost 1" for one directed edge. Only consulted when the
/// edge's mask bit is set. A ramp is an override whose `target_layer` differs from the source.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeOverride {
    pub target_layer: u32,
    pub cost: f32,
}

/// A non-adjacent connection between two nodes (spec §6.3).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Link {
    pub from: NodeId,
    pub to: NodeId,
    pub cost: f32,
    pub bidirectional: bool,
    pub enabled: bool,
}

#[derive(Debug, PartialEq)]
pub enum GridError {
    ZeroSize,
    /// The dense array must hold exactly `width * height * layers` entries.
    LengthMismatch { expected: usize, actual: usize },
    InvalidNode(NodeId),
    InvalidLayer(u32),
    /// Costs must be finite and non-negative.
    InvalidCost(f32),
    UnknownLink(LinkId),
}

#[derive(Debug)]
pub struct TileGrid {
    width: u32,
    height: u32,
    layers: u32,
    /// One mask per node, indexed by `NodeId`.
    edges: Vec<u8>,
    overrides: HashMap<(NodeId, Dir), EdgeOverride>,
    links: HashMap<LinkId, Link>,
    /// Outgoing links per node (both endpoints for bidirectional links). Rebuilt on change.
    link_index: HashMap<NodeId, Vec<LinkId>>,
    /// `(col, row)` of every enabled link entrance — lower-bounds the heuristic.
    link_entrances: Vec<(u32, u32)>,
    /// Cheapest ordinary step; scales the Manhattan heuristic so it stays admissible.
    min_step_cost: f32,
}

impl TileGrid {
    /// Build from per-cell walkability (tier-1 upload): an edge exists in both directions
    /// between adjacent walkable cells. Layout of `walkable` is layer-major, then row-major:
    /// index = `layer * width * height + row * width + col`; nonzero = walkable.
    pub fn from_walkable(width: u32, height: u32, layers: u32, walkable: &[u8]) -> Result<TileGrid, GridError> {
        let mut grid = Self::empty(width, height, layers, walkable.len())?;
        for node in 0..walkable.len() as NodeId {
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

    /// Build from explicit directed edge masks (tier-2 upload) plus overrides. Same layout
    /// as `from_walkable`; each byte is the 4-bit mask for that node (bit index = `Dir`).
    pub fn from_edges(
        width: u32,
        height: u32,
        layers: u32,
        masks: &[u8],
        overrides: &[(NodeId, Dir, EdgeOverride)],
    ) -> Result<TileGrid, GridError> {
        let mut grid = Self::empty(width, height, layers, masks.len())?;
        for (dst, &src) in grid.edges.iter_mut().zip(masks) {
            *dst = src & 0x0F;
        }
        for &(node, dir, ov) in overrides {
            grid.set_edge_override(node, dir, ov)?;
        }
        Ok(grid)
    }

    fn empty(width: u32, height: u32, layers: u32, actual_len: usize) -> Result<TileGrid, GridError> {
        if width == 0 || height == 0 || layers == 0 {
            return Err(GridError::ZeroSize);
        }
        let expected = width as usize * height as usize * layers as usize;
        if actual_len != expected {
            return Err(GridError::LengthMismatch { expected, actual: actual_len });
        }
        Ok(TileGrid {
            width,
            height,
            layers,
            edges: vec![0; expected],
            overrides: HashMap::new(),
            links: HashMap::new(),
            link_index: HashMap::new(),
            link_entrances: Vec::new(),
            min_step_cost: 1.0,
        })
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

    // --- Node addressing ---------------------------------------------------------------

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

    /// `(col, row)` of the cell one step in `dir`, ignoring layer and passability.
    fn adjacent(&self, node: NodeId, dir: Dir) -> Option<(u32, u32)> {
        let (col, row, _) = self.node_coords(node)?;
        let (dc, dr) = dir.delta();
        let nc = col as i64 + dc as i64;
        let nr = row as i64 + dr as i64;
        if nc < 0 || nr < 0 || nc >= self.width as i64 || nr >= self.height as i64 {
            return None;
        }
        Some((nc as u32, nr as u32))
    }

    /// Adjacent node in `dir` on the same layer, ignoring passability.
    fn step(&self, node: NodeId, dir: Dir) -> Option<NodeId> {
        let (_, _, layer) = self.node_coords(node)?;
        let (nc, nr) = self.adjacent(node, dir)?;
        self.node_id(nc, nr, layer)
    }

    // --- Edges -------------------------------------------------------------------------

    pub fn can_move(&self, node: NodeId, dir: Dir) -> bool {
        self.edges.get(node as usize).is_some_and(|m| m & dir.bit() != 0)
    }

    /// Patch one directed edge (doors, switches). Only affects `node -> neighbor`, not the reverse.
    pub fn set_edge(&mut self, node: NodeId, dir: Dir, passable: bool) -> Result<(), GridError> {
        let mask = self.edges.get_mut(node as usize).ok_or(GridError::InvalidNode(node))?;
        if passable {
            *mask |= dir.bit();
        } else {
            *mask &= !dir.bit();
        }
        Ok(())
    }

    /// Set the layer/cost override for one directed edge. Does not change whether the edge
    /// is passable — set the mask bit separately.
    pub fn set_edge_override(&mut self, node: NodeId, dir: Dir, ov: EdgeOverride) -> Result<(), GridError> {
        if node as usize >= self.edges.len() {
            return Err(GridError::InvalidNode(node));
        }
        if ov.target_layer >= self.layers {
            return Err(GridError::InvalidLayer(ov.target_layer));
        }
        check_cost(ov.cost)?;
        self.overrides.insert((node, dir), ov);
        self.min_step_cost = self.min_step_cost.min(ov.cost);
        Ok(())
    }

    pub fn clear_edge_override(&mut self, node: NodeId, dir: Dir) -> bool {
        let removed = self.overrides.remove(&(node, dir)).is_some();
        if removed {
            self.min_step_cost = self.overrides.values().map(|o| o.cost).fold(1.0, f32::min);
        }
        removed
    }

    pub fn edge_override(&self, node: NodeId, dir: Dir) -> Option<EdgeOverride> {
        self.overrides.get(&(node, dir)).copied()
    }

    // --- Links -------------------------------------------------------------------------

    /// Add or replace a link. Ids are mod-assigned; re-adding an id replaces the old link.
    pub fn add_link(&mut self, id: LinkId, link: Link) -> Result<(), GridError> {
        for node in [link.from, link.to] {
            if node as usize >= self.edges.len() {
                return Err(GridError::InvalidNode(node));
            }
        }
        check_cost(link.cost)?;
        self.links.insert(id, link);
        self.rebuild_link_index();
        Ok(())
    }

    pub fn remove_link(&mut self, id: LinkId) -> Result<(), GridError> {
        self.links.remove(&id).ok_or(GridError::UnknownLink(id))?;
        self.rebuild_link_index();
        Ok(())
    }

    pub fn set_link_enabled(&mut self, id: LinkId, enabled: bool) -> Result<(), GridError> {
        let link = self.links.get_mut(&id).ok_or(GridError::UnknownLink(id))?;
        if link.enabled != enabled {
            link.enabled = enabled;
            self.rebuild_link_index();
        }
        Ok(())
    }

    pub fn link(&self, id: LinkId) -> Option<Link> {
        self.links.get(&id).copied()
    }

    fn rebuild_link_index(&mut self) {
        self.link_index.clear();
        self.link_entrances.clear();
        for (&id, link) in &self.links {
            if !link.enabled {
                continue;
            }
            self.link_index.entry(link.from).or_default().push(id);
            if link.bidirectional {
                self.link_index.entry(link.to).or_default().push(id);
            }
        }
        // Deterministic neighbor order regardless of HashMap iteration.
        for ids in self.link_index.values_mut() {
            ids.sort_unstable();
        }
        let mut entrances: Vec<(u32, u32)> = self
            .link_index
            .keys()
            .filter_map(|&n| self.node_coords(n).map(|(c, r, _)| (c, r)))
            .collect();
        entrances.sort_unstable();
        entrances.dedup();
        self.link_entrances = entrances;
    }
}

fn check_cost(cost: f32) -> Result<(), GridError> {
    if cost.is_finite() && cost >= 0.0 { Ok(()) } else { Err(GridError::InvalidCost(cost)) }
}

fn manhattan((ac, ar): (u32, u32), (bc, br): (u32, u32)) -> f32 {
    (ac as i64 - bc as i64).unsigned_abs() as f32 + (ar as i64 - br as i64).unsigned_abs() as f32
}

impl Graph for TileGrid {
    fn node_count(&self) -> usize {
        self.edges.len()
    }

    fn neighbors(&self, node: NodeId, out: &mut Vec<Edge>) {
        let Some(&mask) = self.edges.get(node as usize) else { return };
        let Some((_, _, layer)) = self.node_coords(node) else { return };

        for dir in Dir::ALL {
            if mask & dir.bit() == 0 {
                continue;
            }
            let Some((nc, nr)) = self.adjacent(node, dir) else { continue };
            let (target_layer, cost) = match self.overrides.get(&(node, dir)) {
                Some(ov) => (ov.target_layer, ov.cost),
                None => (layer, 1.0),
            };
            if let Some(to) = self.node_id(nc, nr, target_layer) {
                out.push(Edge::step(to, cost));
            }
        }

        if let Some(ids) = self.link_index.get(&node) {
            for &id in ids {
                let link = self.links[&id];
                if link.from == node {
                    out.push(Edge::link(link.to, link.cost, id));
                }
                if link.bidirectional && link.to == node {
                    out.push(Edge::link(link.from, link.cost, id));
                }
            }
        }
    }

    /// Manhattan distance on the grid plane, scaled by the cheapest step, and additionally
    /// bounded by the distance to the nearest link entrance — a path may teleport, so the
    /// plain straight-line estimate would over-estimate and break A*'s optimality.
    /// Layer differences are ignored (still admissible).
    fn heuristic(&self, a: NodeId, b: NodeId) -> f32 {
        let (Some((ac, ar, _)), Some((bc, br, _))) = (self.node_coords(a), self.node_coords(b)) else {
            return 0.0;
        };
        let mut steps = manhattan((ac, ar), (bc, br));
        for &entrance in &self.link_entrances {
            steps = steps.min(manhattan((ac, ar), entrance));
        }
        steps * self.min_step_cost
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::astar;

    fn open(width: u32, height: u32, layers: u32) -> TileGrid {
        TileGrid::from_walkable(width, height, layers, &vec![1; (width * height * layers) as usize]).unwrap()
    }

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
        let g = open(3, 2, 2);
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

        let path = astar(&g, g.node_id(0, 0, 0).unwrap(), g.node_id(4, 0, 0).unwrap()).expect("path around the wall");
        // Down 4, across 4, up 4 = 12 steps = 13 nodes, and it must pass through the gap.
        assert_eq!(path.nodes.len(), 13);
        assert_eq!(path.cost, 12.0);
        assert!(path.nodes.contains(&g.node_id(2, 4, 0).unwrap()));
    }

    #[test]
    fn one_way_edge_is_honored() {
        let mut g = open(2, 1, 1);
        let (a, b) = (0, 1);
        g.set_edge(b, Dir::West, false).unwrap(); // ledge: can go east, can't come back
        assert_eq!(astar(&g, a, b).unwrap().nodes, vec![a, b]);
        assert_eq!(astar(&g, b, a), None);
        assert_eq!(g.set_edge(99, Dir::West, true), Err(GridError::InvalidNode(99)));
    }

    #[test]
    fn from_edges_takes_masks_verbatim() {
        // Two cells; only 0 -> 1 (East from node 0) is open.
        let g = TileGrid::from_edges(2, 1, 1, &[1 << Dir::East as u8, 0], &[]).unwrap();
        assert!(g.can_move(0, Dir::East));
        assert!(!g.can_move(1, Dir::West));
        assert_eq!(astar(&g, 1, 0), None);
    }

    #[test]
    fn ramp_override_changes_layer_one_way() {
        // 3x1, two layers. Layer 0 is open; layer 1 is open. Stepping East off node 1 (layer 0)
        // lands on layer 1 — a CSTW-style ramp. Without the ramp the layers are disconnected.
        let mut g = open(3, 1, 2);
        let top_right = g.node_id(2, 0, 1).unwrap();
        assert_eq!(astar(&g, 0, top_right), None);

        g.set_edge_override(1, Dir::East, EdgeOverride { target_layer: 1, cost: 1.0 }).unwrap();
        let p = astar(&g, 0, top_right).unwrap();
        assert_eq!(p.nodes, vec![0, 1, top_right]);
        // The reverse edge (West from the upper cell) still stays on layer 1.
        assert_eq!(astar(&g, top_right, 0), None);

        assert_eq!(
            g.set_edge_override(1, Dir::East, EdgeOverride { target_layer: 5, cost: 1.0 }),
            Err(GridError::InvalidLayer(5))
        );
        assert!(g.clear_edge_override(1, Dir::East));
        assert!(!g.clear_edge_override(1, Dir::East));
    }

    #[test]
    fn edge_costs_steer_the_path() {
        // 3x2: going along the top row costs 10 per step (swamp); the bottom row is free.
        let mut g = open(3, 2, 1);
        g.set_edge_override(0, Dir::East, EdgeOverride { target_layer: 0, cost: 10.0 }).unwrap();
        g.set_edge_override(1, Dir::East, EdgeOverride { target_layer: 0, cost: 10.0 }).unwrap();
        let p = astar(&g, 0, 2).unwrap();
        assert_eq!(p.nodes, vec![0, 3, 4, 5, 2]);
        assert_eq!(p.cost, 4.0);
        assert!(matches!(
            g.set_edge_override(0, Dir::East, EdgeOverride { target_layer: 0, cost: f32::NAN }),
            Err(GridError::InvalidCost(c)) if c.is_nan()
        ));
    }

    #[test]
    fn teleporter_link_is_used_recorded_and_toggleable() {
        // 1x10 line with the middle cell walled off: only a link can cross.
        let mut w = vec![1u8; 10];
        w[5] = 0;
        let mut g = TileGrid::from_walkable(10, 1, 1, &w).unwrap();
        assert_eq!(astar(&g, 0, 9), None);

        g.add_link(42, Link { from: 2, to: 7, cost: 1.0, bidirectional: false, enabled: true }).unwrap();
        let p = astar(&g, 0, 9).unwrap();
        assert_eq!(p.nodes, vec![0, 1, 2, 7, 8, 9]);
        assert_eq!(p.links, vec![(3, 42)]);
        assert_eq!(astar(&g, 9, 0), None, "one-way link");

        g.add_link(42, Link { from: 2, to: 7, cost: 1.0, bidirectional: true, enabled: true }).unwrap();
        assert_eq!(astar(&g, 9, 0).unwrap().links, vec![(3, 42)]);

        g.set_link_enabled(42, false).unwrap();
        assert_eq!(astar(&g, 0, 9), None, "disabled link is invisible");
        g.set_link_enabled(42, true).unwrap();
        assert!(astar(&g, 0, 9).is_some());

        g.remove_link(42).unwrap();
        assert_eq!(g.remove_link(42), Err(GridError::UnknownLink(42)));
        assert_eq!(astar(&g, 0, 9), None);
        assert_eq!(
            g.add_link(1, Link { from: 0, to: 99, cost: 1.0, bidirectional: false, enabled: true }),
            Err(GridError::InvalidNode(99))
        );
    }

    #[test]
    fn heuristic_stays_admissible_with_a_teleporter_behind_the_start() {
        // 1x20 line, start at 5, goal at 19. A free teleporter at 0 -> 19 makes the optimal
        // route go *away* from the goal first (5 steps + 0) instead of walking 14. A plain
        // Manhattan heuristic over-estimates near the entrance and A* would return the walk.
        let mut g = open(20, 1, 1);
        g.add_link(7, Link { from: 0, to: 19, cost: 0.0, bidirectional: false, enabled: true }).unwrap();
        let p = astar(&g, 5, 19).unwrap();
        assert_eq!(p.nodes, vec![5, 4, 3, 2, 1, 0, 19]);
        assert_eq!(p.cost, 5.0);
        assert_eq!(p.links, vec![(6, 7)]);
    }

    #[test]
    fn layers_without_layer_edges_are_disconnected() {
        let g = open(2, 2, 2);
        assert_eq!(astar(&g, g.node_id(0, 0, 0).unwrap(), g.node_id(0, 0, 1).unwrap()), None);
    }
}
