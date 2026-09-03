//! A context bundles one map with everything that must stay consistent with it
//! (spec §4): the grid, the dynamic node-group overlay used for blocking, and — as they
//! are built — entities, profiles, trackers and caches.

use std::collections::HashSet;

use crate::geometry::Vec3;
use crate::graph::{NodeId, Path, Query, astar_query, flood_fill};
use crate::tile::TileGrid;

/// How a set of target nodes is treated as a destination (spec §8).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GoalMode {
    /// Arrive on a target node. Targets are exempt from blocking (you may path onto an
    /// exit even though exits block ordinary movement).
    #[default]
    Onto,
    /// Arrive on a node from which one ordinary step enters a target (talk to an NPC).
    /// Targets stay blocked; candidates are subject to blocking like any other node.
    Adjacent,
}

#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub struct PathOptions {
    /// Nodes carrying any of these groups are impassable.
    pub blocked_mask: u32,
    /// If the start node has no edges (position inside geometry), snap to the nearest node
    /// within this many rings that has edges and is not blocked. 0 disables snapping.
    pub snap_radius: u32,
    pub goal_mode: GoalMode,
}

pub struct TileContext {
    grid: TileGrid,
    /// Group bitmask per node. Maintained by the entity layer (projection) or directly.
    groups: Vec<u32>,
}

impl TileContext {
    pub fn new(grid: TileGrid) -> Self {
        let groups = vec![0; grid.width() as usize * grid.height() as usize * grid.layers() as usize];
        TileContext { grid, groups }
    }

    pub fn grid(&self) -> &TileGrid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut TileGrid {
        &mut self.grid
    }

    // --- Groups ------------------------------------------------------------------------

    pub fn groups(&self, node: NodeId) -> Option<u32> {
        self.groups.get(node as usize).copied()
    }

    pub fn set_groups(&mut self, node: NodeId, mask: u32) -> bool {
        match self.groups.get_mut(node as usize) {
            Some(g) => {
                *g = mask;
                true
            }
            None => false,
        }
    }

    pub fn add_groups(&mut self, node: NodeId, mask: u32) -> bool {
        match self.groups.get_mut(node as usize) {
            Some(g) => {
                *g |= mask;
                true
            }
            None => false,
        }
    }

    pub fn remove_groups(&mut self, node: NodeId, mask: u32) -> bool {
        match self.groups.get_mut(node as usize) {
            Some(g) => {
                *g &= !mask;
                true
            }
            None => false,
        }
    }

    /// Remove `mask` from every node.
    pub fn clear_groups(&mut self, mask: u32) {
        for g in &mut self.groups {
            *g &= !mask;
        }
    }

    // --- Queries -----------------------------------------------------------------------

    /// Path from the node containing `start` to the node containing `goal`.
    /// `None` if a position is off-grid or no route exists.
    pub fn find_path(&self, start: Vec3, goal: Vec3, opts: PathOptions) -> Option<Path> {
        let goal = self.grid.node_from_position(goal)?;
        self.find_path_to_nodes(start, &[goal], opts)
    }

    /// Path from the node containing `start` to the cheapest of `targets`, interpreted per
    /// `opts.goal_mode`. Target ids must be in range.
    pub fn find_path_to_nodes(&self, start: Vec3, targets: &[NodeId], opts: PathOptions) -> Option<Path> {
        if targets.iter().any(|&t| t as usize >= self.groups.len()) {
            return None;
        }
        let start = self.snap_start(self.grid.node_from_position(start)?, opts)?;
        let (goals, exempt): (Vec<NodeId>, HashSet<NodeId>) = match opts.goal_mode {
            GoalMode::Onto => (targets.to_vec(), targets.iter().copied().collect()),
            GoalMode::Adjacent => (self.grid.adjacent_candidates(targets), HashSet::new()),
        };
        let is_blocked = |n: NodeId| self.groups[n as usize] & opts.blocked_mask != 0 && !exempt.contains(&n);
        astar_query(&self.grid, &Query { start, goals: &goals, is_blocked: &is_blocked })
    }

    /// Reachability from the node containing `start`, indexed by node id.
    /// `None` if `start` is off-grid (or cannot be snapped).
    pub fn reachable(&self, start: Vec3, opts: PathOptions) -> Option<Vec<bool>> {
        let start = self.snap_start(self.grid.node_from_position(start)?, opts)?;
        let is_blocked = |n: NodeId| self.groups[n as usize] & opts.blocked_mask != 0;
        Some(flood_fill(&self.grid, start, &is_blocked))
    }

    /// The start is usable as-is if an agent can stand there (it has edges); blocking is
    /// irrelevant because the agent is already there. Otherwise snap to the nearest node
    /// that has edges and is not blocked.
    fn snap_start(&self, node: NodeId, opts: PathOptions) -> Option<NodeId> {
        if self.grid.has_edges(node) {
            return Some(node);
        }
        self.grid.nearest_node(node, opts.snap_radius, |n| {
            self.grid.has_edges(n) && self.groups[n as usize] & opts.blocked_mask == 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NPC: u32 = 1 << 0;
    const EXIT: u32 = 1 << 1;

    fn cell(col: u32, row: u32) -> Vec3 {
        Vec3::new(col as f32 + 0.5, 0.0, row as f32 + 0.5)
    }

    /// 5x3 open corridor grid.
    fn ctx() -> TileContext {
        TileContext::new(TileGrid::from_walkable(5, 3, 1, &[1; 15]).unwrap())
    }

    #[test]
    fn blocked_groups_force_a_detour() {
        let mut c = ctx();
        // NPC standing at (2,1), the middle of the middle row.
        let npc = c.grid().node_id(2, 1, 0).unwrap();
        assert!(c.add_groups(npc, NPC));
        let straight = c.find_path(cell(0, 1), cell(4, 1), PathOptions::default()).unwrap();
        assert_eq!(straight.nodes.len(), 5, "ignoring groups walks straight through");
        let around = c.find_path(cell(0, 1), cell(4, 1), PathOptions { blocked_mask: NPC, ..Default::default() }).unwrap();
        assert_eq!(around.nodes.len(), 7);
        assert!(!around.nodes.contains(&npc));
        assert!(!c.set_groups(999, NPC));
    }

    #[test]
    fn onto_exempts_targets_from_blocking() {
        let mut c = ctx();
        let exit = c.grid().node_id(4, 1, 0).unwrap();
        c.add_groups(exit, EXIT);
        let opts = PathOptions { blocked_mask: EXIT | NPC, ..Default::default() };
        let p = c.find_path(cell(0, 1), cell(4, 1), opts).unwrap();
        assert_eq!(*p.nodes.last().unwrap(), exit);
    }

    #[test]
    fn adjacent_stops_next_to_the_target_and_avoids_other_blockers() {
        let mut c = ctx();
        let npc = c.grid().node_id(4, 1, 0).unwrap();
        c.add_groups(npc, NPC);
        // Another NPC stands on the obvious candidate west of the target.
        let other = c.grid().node_id(3, 1, 0).unwrap();
        c.add_groups(other, NPC);
        let opts = PathOptions { blocked_mask: NPC, goal_mode: GoalMode::Adjacent, ..Default::default() };
        let p = c.find_path_to_nodes(cell(0, 1), &[npc], opts).unwrap();
        let last = *p.nodes.last().unwrap();
        assert!(last == c.grid().node_id(4, 0, 0).unwrap() || last == c.grid().node_id(4, 2, 0).unwrap());
        assert!(!p.nodes.contains(&npc) && !p.nodes.contains(&other));
        // Already adjacent: zero-length path.
        assert_eq!(c.find_path_to_nodes(cell(4, 0), &[npc], opts).unwrap().nodes.len(), 1);
        assert_eq!(c.find_path_to_nodes(cell(0, 1), &[9999], opts), None);
    }

    #[test]
    fn start_inside_a_wall_snaps_to_the_nearest_floor() {
        let mut w = [1u8; 15];
        w[7] = 0; // (2,1) is solid
        let c = TileContext::new(TileGrid::from_walkable(5, 3, 1, &w).unwrap());
        assert_eq!(c.find_path(cell(2, 1), cell(4, 1), PathOptions::default()), None, "no snapping by default");
        let p = c.find_path(cell(2, 1), cell(4, 1), PathOptions { snap_radius: 1, ..Default::default() }).unwrap();
        assert_ne!(p.nodes[0], 7);
        assert_eq!(*p.nodes.last().unwrap(), c.grid().node_id(4, 1, 0).unwrap());
    }

    #[test]
    fn reachability_honours_blocking() {
        let mut c = ctx();
        // Wall of NPCs across column 2.
        for row in 0..3 {
            c.add_groups(c.grid().node_id(2, row, 0).unwrap(), NPC);
        }
        let open = c.reachable(cell(0, 0), PathOptions::default()).unwrap();
        assert!(open.iter().all(|&r| r));
        let blocked = c.reachable(cell(0, 0), PathOptions { blocked_mask: NPC, ..Default::default() }).unwrap();
        assert!(blocked[c.grid().node_id(1, 2, 0).unwrap() as usize]);
        assert!(!blocked[c.grid().node_id(3, 0, 0).unwrap() as usize]);
        c.clear_groups(NPC);
        assert!(c.reachable(cell(0, 0), PathOptions { blocked_mask: NPC, ..Default::default() }).unwrap().iter().all(|&r| r));
    }
}
