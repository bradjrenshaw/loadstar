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

use std::cell::OnceCell;
use std::collections::HashMap;

use crate::geometry::{Bounds, Vec3};
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
    /// Per-node clearance (spec §7), computed on first use and dropped whenever edges change.
    clearance: OnceCell<Vec<u8>>,
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
            clearance: OnceCell::new(),
        })
    }

    // --- Clearance (agent size) -------------------------------------------------------

    /// Largest `k` such that the `k × k` block of cells whose top-left corner is `node` is
    /// entirely standable (every cell has edges), on the node's layer. 0 for walls. Capped at
    /// 255. An agent with footprint `f` may occupy `node` iff `clearance(node) >= f`.
    pub fn clearance(&self, node: NodeId) -> u8 {
        self.clearance_field().get(node as usize).copied().unwrap_or(0)
    }

    fn clearance_field(&self) -> &[u8] {
        self.clearance.get_or_init(|| {
            let (w, h, per_layer) = (self.width as usize, self.height as usize, (self.width * self.height) as usize);
            let mut field = vec![0u8; self.edges.len()];
            for layer in 0..self.layers as usize {
                let base = layer * per_layer;
                // Bottom-right to top-left: a block fits here if this cell stands and the
                // three blocks to the east, south and south-east each fit one size smaller.
                for r in (0..h).rev() {
                    for c in (0..w).rev() {
                        let i = base + r * w + c;
                        if !self.has_edges(i as NodeId) {
                            continue;
                        }
                        let east = if c + 1 < w { field[i + 1] } else { 0 };
                        let south = if r + 1 < h { field[i + w] } else { 0 };
                        let diag = if c + 1 < w && r + 1 < h { field[i + w + 1] } else { 0 };
                        field[i] = east.min(south).min(diag).saturating_add(1);
                    }
                }
            }
            field
        })
    }

    fn invalidate_clearance(&mut self) {
        self.clearance.take();
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

    /// The projection seam (spec §2, §5): every node this geometry occupies, clipped to the
    /// grid. Sorted and deduplicated. Empty if the bounds lie entirely off-grid.
    pub fn project(&self, bounds: &Bounds) -> Vec<NodeId> {
        let mut out = Vec::new();
        match *bounds {
            Bounds::Point(p) => out.extend(self.node_from_position(p)),
            Bounds::TileRect { col, row, width, height, layer } => {
                let layers: Vec<u32> = match layer {
                    Some(l) => vec![l],
                    None => (0..self.layers).collect(),
                };
                for l in layers {
                    for r in row..row.saturating_add(height).min(self.height) {
                        for c in col..col.saturating_add(width).min(self.width) {
                            out.extend(self.node_id(c, r, l));
                        }
                    }
                }
            }
            Bounds::Box { center, extents } => {
                let (c0, c1) = cell_span(center.x - extents.x, center.x + extents.x);
                let (r0, r1) = cell_span(center.z - extents.z, center.z + extents.z);
                let (l0, l1) = cell_span(center.y - extents.y, center.y + extents.y);
                for l in l0..=l1 {
                    for r in r0..=r1 {
                        for c in c0..=c1 {
                            out.extend(self.node_id(c, r, l));
                        }
                    }
                }
            }
            Bounds::Sphere { center, radius } => {
                let (c0, c1) = cell_span(center.x - radius, center.x + radius);
                let (r0, r1) = cell_span(center.z - radius, center.z + radius);
                let l = center.y.max(0.0) as u32;
                for r in r0..=r1 {
                    for c in c0..=c1 {
                        let (dx, dz) = (c as f32 + 0.5 - center.x, r as f32 + 0.5 - center.z);
                        if dx * dx + dz * dz <= radius * radius {
                            out.extend(self.node_id(c, r, l));
                        }
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    /// Nearest node to `from` (same layer, Manhattan rings up to `max_radius`) satisfying
    /// `pred`; `from` itself if it qualifies. Ties resolve to the lowest node id, so results
    /// are deterministic. Used for start snapping (spec §8).
    pub fn nearest_node(&self, from: NodeId, max_radius: u32, pred: impl Fn(NodeId) -> bool) -> Option<NodeId> {
        if pred(from) {
            return Some(from);
        }
        let (fc, fr, layer) = self.node_coords(from)?;
        for radius in 1..=max_radius as i64 {
            let mut best: Option<NodeId> = None;
            for dc in -radius..=radius {
                let dr_abs = radius - dc.abs();
                for dr in if dr_abs == 0 { vec![0] } else { vec![-dr_abs, dr_abs] } {
                    let (c, r) = (fc as i64 + dc, fr as i64 + dr);
                    if c < 0 || r < 0 {
                        continue;
                    }
                    if let Some(node) = self.node_id(c as u32, r as u32, layer)
                        && pred(node)
                    {
                        best = Some(best.map_or(node, |b| b.min(node)));
                    }
                }
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }

    /// Nodes from which one ordinary (non-link) step enters any of `targets`, excluding the
    /// targets themselves — the "stand next to it" goal candidates (spec §8). Sorted, deduplicated.
    pub fn adjacent_candidates(&self, targets: &[NodeId]) -> Vec<NodeId> {
        let target_set: std::collections::HashSet<NodeId> = targets.iter().copied().collect();
        let mut out = Vec::new();
        let mut scratch = Vec::with_capacity(8);
        for &t in targets {
            for dir in Dir::ALL {
                let Some((c, r)) = self.adjacent(t, dir) else { continue };
                for layer in 0..self.layers {
                    let Some(candidate) = self.node_id(c, r, layer) else { continue };
                    if target_set.contains(&candidate) {
                        continue;
                    }
                    scratch.clear();
                    self.neighbors(candidate, &mut scratch);
                    if scratch.iter().any(|e| e.link.is_none() && e.to == t) {
                        out.push(candidate);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }

    // --- Edges -------------------------------------------------------------------------

    /// Whether any edge leaves this node — i.e. it is somewhere an agent can actually stand.
    pub fn has_edges(&self, node: NodeId) -> bool {
        self.edges.get(node as usize).is_some_and(|&m| m != 0) || self.link_index.contains_key(&node)
    }

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
        self.invalidate_clearance();
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
        self.invalidate_clearance(); // `has_edges` counts links
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

/// Inclusive range of cell indices overlapped by the continuous interval `[min, max]`,
/// clamped at zero. A cell is `[i, i + 1)`, so an interval ending exactly on a boundary does
/// not touch the next cell.
fn cell_span(min: f32, max: f32) -> (u32, u32) {
    let first = min.max(0.0).floor() as u32;
    let last = (max - 1e-4).max(0.0).floor() as u32;
    (first, last.max(first))
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
    fn nearest_node_searches_rings_deterministically() {
        let g = open(5, 5, 1);
        let center = g.node_id(2, 2, 0).unwrap();
        assert_eq!(g.nearest_node(center, 3, |_| true), Some(center));
        // Ring 1 has four cells; the lowest id is the north neighbour (row 1).
        assert_eq!(g.nearest_node(center, 3, |n| n != center), g.node_id(2, 1, 0));
        // Only the far corner qualifies: found at radius 4, not within 3.
        let corner = g.node_id(4, 4, 0).unwrap();
        assert_eq!(g.nearest_node(center, 3, |n| n == corner), None);
        assert_eq!(g.nearest_node(center, 4, |n| n == corner), Some(corner));
    }

    #[test]
    fn adjacent_candidates_follow_real_edges_only() {
        // 3x3 open grid; NPC occupies the center. Candidates are its four neighbours...
        let mut g = open(3, 3, 1);
        let center = g.node_id(1, 1, 0).unwrap();
        assert_eq!(g.adjacent_candidates(&[center]), vec![1, 3, 5, 7]);
        // ...unless the edge into it is closed (a counter between you and the shopkeeper),
        assert_eq!(g.set_edge(1, Dir::South, false), Ok(()));
        assert_eq!(g.adjacent_candidates(&[center]), vec![3, 5, 7]);
        // ...and a teleporter landing on it does not count as adjacency.
        g.add_link(1, Link { from: 8, to: center, cost: 1.0, bidirectional: false, enabled: true }).unwrap();
        assert_eq!(g.adjacent_candidates(&[center]), vec![3, 5, 7]);
        // A 2-cell target excludes its own cells from the candidates (1 is still cut off by the closed edge).
        assert_eq!(g.adjacent_candidates(&[3, 4]), vec![0, 5, 6, 7]);
    }

    #[test]
    fn projection_clips_and_covers_each_bounds_kind() {
        let g = open(4, 4, 2);
        assert_eq!(g.project(&Bounds::Point(Vec3::new(1.5, 1.0, 2.5))), vec![g.node_id(1, 2, 1).unwrap()]);
        assert_eq!(g.project(&Bounds::Point(Vec3::new(9.0, 0.0, 0.0))), vec![]);

        let rect = Bounds::TileRect { col: 2, row: 3, width: 3, height: 3, layer: Some(0) };
        assert_eq!(g.project(&rect), vec![g.node_id(2, 3, 0).unwrap(), g.node_id(3, 3, 0).unwrap()], "clipped at the edge");
        let all = Bounds::TileRect { col: 0, row: 0, width: 1, height: 1, layer: None };
        assert_eq!(g.project(&all), vec![0, 16], "one cell on both layers");

        // A box centered at (1.5, 0, 1.5) with half-size 1 spans cells 0..=2 in both axes.
        let bx = Bounds::Box { center: Vec3::new(1.5, 0.0, 1.5), extents: Vec3::new(1.0, 0.0, 1.0) };
        assert_eq!(g.project(&bx).len(), 9);
        // Half-size 0.5 ends exactly on cell boundaries: only the center cell.
        let tight = Bounds::Box { center: Vec3::new(1.5, 0.0, 1.5), extents: Vec3::new(0.5, 0.0, 0.5) };
        assert_eq!(g.project(&tight), vec![g.node_id(1, 1, 0).unwrap()]);

        // Radius 1 around a cell center covers the plus shape, not the diagonals.
        let sp = Bounds::Sphere { center: Vec3::new(1.5, 0.0, 1.5), radius: 1.0 };
        assert_eq!(g.project(&sp), vec![1, 4, 5, 6, 9]);
    }

    #[test]
    fn clearance_measures_square_footprints_and_tracks_edits() {
        // 4x3 open grid with a wall at (1,1).
        let mut w = [1u8; 12];
        w[5] = 0;
        let mut g = TileGrid::from_walkable(4, 3, 1, &w).unwrap();
        let at = |g: &TileGrid, c, r| g.clearance(g.node_id(c, r, 0).unwrap());
        assert_eq!(at(&g, 1, 1), 0, "wall");
        assert_eq!(at(&g, 0, 0), 1, "2x2 here would include the wall");
        assert_eq!(at(&g, 2, 0), 2, "2x2 at (2,0) covers (2..3, 0..1)");
        assert_eq!(at(&g, 2, 1), 2);
        assert_eq!(at(&g, 3, 2), 1, "grid edge");

        // Sealing off (2,0) shrinks every block that used it; the cache must notice.
        for d in Dir::ALL {
            g.set_edge(g.node_id(2, 0, 0).unwrap(), d, false).unwrap();
        }
        assert_eq!(at(&g, 2, 0), 0, "cache was invalidated by the edit");
        assert_eq!(at(&g, 2, 1), 2, "blocks that never used (2,0) are unaffected");
        assert_eq!(at(&g, 3, 0), 1);
    }

    #[test]
    fn has_edges_distinguishes_walls_from_floor() {
        let g = walled();
        assert!(g.has_edges(g.node_id(0, 0, 0).unwrap()));
        assert!(!g.has_edges(g.node_id(2, 0, 0).unwrap()));
    }

    #[test]
    fn layers_without_layer_edges_are_disconnected() {
        let g = open(2, 2, 2);
        assert_eq!(astar(&g, g.node_id(0, 0, 0).unwrap(), g.node_id(0, 0, 1).unwrap()), None);
    }
}
