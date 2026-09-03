//! A context bundles one map with everything that must stay consistent with it (spec §4):
//! the grid, the entity registry, the entities' projections into nodes, and the blocking
//! overlay derived from them. This is the only place entities and the map meet, and they
//! meet exclusively through projection (spec §2).

use std::collections::{HashMap, HashSet};

use crate::entities::{Change, Entity, EntityId, Registry, SyncStats, flags};
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
    /// Agent profile id from `register_profile`. 0 is the built-in one-cell agent.
    pub profile: u32,
}

/// An agent's size (spec §7). Tile grids honour `footprint` (side of the square block of
/// cells the agent occupies, anchored at its node); the other fields are carried for mesh
/// maps and future use.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Profile {
    pub footprint: u32,
    pub radius: f32,
    pub height: f32,
    pub step_height: f32,
}

impl Default for Profile {
    fn default() -> Self {
        Profile { footprint: 1, radius: 0.5, height: 1.0, step_height: 0.0 }
    }
}

/// An entity's cached projection, remembering the blocking it contributed so a later
/// change can tell whether the overlay needs rebuilding.
struct Projection {
    blocking: u32,
    nodes: Vec<NodeId>,
}

pub struct TileContext {
    grid: TileGrid,
    /// Groups set directly through the API. OR'd with `entity_groups` for blocking.
    manual_groups: Vec<u32>,
    /// Groups derived from blocking entities' projections. Rebuilt when they change.
    entity_groups: Vec<u32>,
    registry: Registry,
    projections: HashMap<EntityId, Projection>,
    /// Index 0 is the built-in default agent.
    profiles: Vec<Profile>,
}

impl TileContext {
    pub fn new(grid: TileGrid) -> Self {
        let n = grid.width() as usize * grid.height() as usize * grid.layers() as usize;
        TileContext {
            grid,
            manual_groups: vec![0; n],
            entity_groups: vec![0; n],
            registry: Registry::default(),
            projections: HashMap::new(),
            profiles: vec![Profile::default()],
        }
    }

    // --- Profiles ----------------------------------------------------------------------

    /// Register an agent profile and get its id. `None` if the footprint is 0 or over 255.
    pub fn register_profile(&mut self, profile: Profile) -> Option<u32> {
        if profile.footprint == 0 || profile.footprint > 255 {
            return None;
        }
        self.profiles.push(profile);
        Some(self.profiles.len() as u32 - 1)
    }

    pub fn profile(&self, id: u32) -> Option<&Profile> {
        self.profiles.get(id as usize)
    }

    /// Whether an agent of this footprint may occupy `node`. Footprint 1 never consults the
    /// clearance field, so the default profile costs nothing.
    fn fits(&self, node: NodeId, footprint: u32) -> bool {
        footprint <= 1 || self.grid.clearance(node) as u32 >= footprint
    }

    pub fn grid(&self) -> &TileGrid {
        &self.grid
    }

    pub fn grid_mut(&mut self) -> &mut TileGrid {
        &mut self.grid
    }

    // --- Groups ------------------------------------------------------------------------

    /// Effective groups of a node: manual OR entity-derived. This is what blocking sees.
    pub fn groups(&self, node: NodeId) -> Option<u32> {
        Some(*self.manual_groups.get(node as usize)? | self.entity_groups[node as usize])
    }

    pub fn set_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.manual_groups.get_mut(node as usize).map(|g| *g = mask).is_some()
    }

    pub fn add_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.manual_groups.get_mut(node as usize).map(|g| *g |= mask).is_some()
    }

    pub fn remove_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.manual_groups.get_mut(node as usize).map(|g| *g &= !mask).is_some()
    }

    /// Remove `mask` from every node's manual groups. Entity-derived groups are unaffected.
    pub fn clear_groups(&mut self, mask: u32) {
        for g in &mut self.manual_groups {
            *g &= !mask;
        }
    }

    fn effective_groups(&self, node: NodeId) -> u32 {
        self.manual_groups[node as usize] | self.entity_groups[node as usize]
    }

    // --- Entities ----------------------------------------------------------------------

    pub fn entity(&self, id: EntityId) -> Option<&Entity> {
        self.registry.get(id)
    }

    pub fn entities(&self) -> impl Iterator<Item = &Entity> {
        self.registry.iter()
    }

    pub fn entity_count(&self) -> usize {
        self.registry.len()
    }

    /// Nodes the entity occupies (its bounds projected through the grid). Empty if it lies
    /// entirely off-grid.
    pub fn entity_nodes(&self, id: EntityId) -> Option<&[NodeId]> {
        self.projections.get(&id).map(|p| p.nodes.as_slice())
    }

    pub fn upsert_entity(&mut self, entity: Entity) -> Change {
        let change = self.registry.upsert(entity);
        if change != Change::Unchanged && self.reproject(entity.id) {
            self.rebuild_entity_groups();
        }
        change
    }

    pub fn remove_entity(&mut self, id: EntityId) -> bool {
        if self.registry.remove(id).is_none() {
            return false;
        }
        if self.projections.remove(&id).is_some_and(|p| p.blocking != 0) {
            self.rebuild_entity_groups();
        }
        true
    }

    /// Declarative snapshot sync (spec §5): the core diffs, re-projects only what changed,
    /// and rebuilds the blocking overlay at most once.
    pub fn sync_entities(&mut self, snapshot: &[Entity]) -> SyncStats {
        let (stats, changed, removed) = self.registry.sync(snapshot);
        let mut rebuild = false;
        for id in changed {
            rebuild |= self.reproject(id);
        }
        for id in removed {
            rebuild |= self.projections.remove(&id).is_some_and(|p| p.blocking != 0);
        }
        if rebuild {
            self.rebuild_entity_groups();
        }
        stats
    }

    /// Recompute one entity's projection. Returns whether the blocking overlay is affected.
    fn reproject(&mut self, id: EntityId) -> bool {
        let Some(entity) = self.registry.get(id) else { return false };
        let nodes = self.grid.project(&entity.bounds);
        let old_blocking = self.projections.get(&id).map_or(0, |p| p.blocking);
        self.projections.insert(id, Projection { blocking: entity.blocking, nodes });
        old_blocking != 0 || entity.blocking != 0
    }

    fn rebuild_entity_groups(&mut self) {
        self.entity_groups.iter_mut().for_each(|g| *g = 0);
        for p in self.projections.values().filter(|p| p.blocking != 0) {
            for &n in &p.nodes {
                self.entity_groups[n as usize] |= p.blocking;
            }
        }
    }

    /// Closest point of the entity's bounds to `from` (layer-1 geometry, no map involved).
    pub fn entity_nearest_point(&self, id: EntityId, from: Vec3) -> Option<Vec3> {
        Some(self.registry.get(id)?.bounds.nearest_point(from))
    }

    /// Path to an entity. The goal mode comes from the entity's `PATHFIND_ADJACENT` flag,
    /// not from `opts`. `None` if unknown, off-grid, or unreachable.
    pub fn find_path_to_entity(&self, start: Vec3, id: EntityId, opts: PathOptions) -> Option<Path> {
        let entity = self.registry.get(id)?;
        let nodes = self.entity_nodes(id)?;
        if nodes.is_empty() {
            return None;
        }
        let goal_mode = if entity.flags & flags::PATHFIND_ADJACENT != 0 { GoalMode::Adjacent } else { GoalMode::Onto };
        self.find_path_to_nodes(start, nodes, PathOptions { goal_mode, ..opts })
    }

    /// Whether an entity counts as reachable given a reachability set (spec §8): any of its
    /// nodes, *or any node one step away from them*, is reachable — an NPC standing on a
    /// blocked tile is reachable if you can stand next to it.
    pub fn entity_reachable(&self, reached: &[bool], id: EntityId) -> Option<bool> {
        let nodes = self.entity_nodes(id)?;
        let hit = |n: &NodeId| reached.get(*n as usize).copied().unwrap_or(false);
        Some(nodes.iter().any(hit) || self.grid.adjacent_candidates(nodes).iter().any(hit))
    }

    // --- Queries -----------------------------------------------------------------------

    /// Path from the node containing `start` to the node containing `goal`.
    /// `None` if a position is off-grid or no route exists.
    pub fn find_path(&self, start: Vec3, goal: Vec3, opts: PathOptions) -> Option<Path> {
        let goal = self.grid.node_from_position(goal)?;
        self.find_path_to_nodes(start, &[goal], opts)
    }

    /// Path from the node containing `start` to the cheapest of `targets`, interpreted per
    /// `opts.goal_mode`. Target ids must be in range; `opts.profile` must be registered.
    pub fn find_path_to_nodes(&self, start: Vec3, targets: &[NodeId], opts: PathOptions) -> Option<Path> {
        if targets.iter().any(|&t| t as usize >= self.manual_groups.len()) {
            return None;
        }
        let footprint = self.profile(opts.profile)?.footprint;
        let start = self.snap_start(self.grid.node_from_position(start)?, opts, footprint)?;
        let (goals, exempt): (Vec<NodeId>, HashSet<NodeId>) = match opts.goal_mode {
            GoalMode::Onto => (targets.to_vec(), targets.iter().copied().collect()),
            GoalMode::Adjacent => (self.grid.adjacent_candidates(targets), HashSet::new()),
        };
        // Group blocking may be exempted for targets; fitting never is.
        let is_blocked = |n: NodeId| {
            !self.fits(n, footprint) || (self.effective_groups(n) & opts.blocked_mask != 0 && !exempt.contains(&n))
        };
        astar_query(&self.grid, &Query { start, goals: &goals, is_blocked: &is_blocked })
    }

    /// Reachability from the node containing `start`, indexed by node id.
    /// `None` if `start` is off-grid (or cannot be snapped) or the profile is unknown.
    pub fn reachable(&self, start: Vec3, opts: PathOptions) -> Option<Vec<bool>> {
        let footprint = self.profile(opts.profile)?.footprint;
        let start = self.snap_start(self.grid.node_from_position(start)?, opts, footprint)?;
        let is_blocked = |n: NodeId| !self.fits(n, footprint) || self.effective_groups(n) & opts.blocked_mask != 0;
        Some(flood_fill(&self.grid, start, &is_blocked))
    }

    /// The start is usable as-is if an agent can stand there (it has edges); blocking and
    /// fit are irrelevant because the agent is already there. Otherwise snap to the nearest
    /// node that has edges, fits, and is not blocked.
    fn snap_start(&self, node: NodeId, opts: PathOptions, footprint: u32) -> Option<NodeId> {
        if self.grid.has_edges(node) {
            return Some(node);
        }
        self.grid.nearest_node(node, opts.snap_radius, |n| {
            self.grid.has_edges(n) && self.fits(n, footprint) && self.effective_groups(n) & opts.blocked_mask == 0
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Bounds;

    const NPC: u32 = 1 << 0;
    const EXIT: u32 = 1 << 1;

    fn cell(col: u32, row: u32) -> Vec3 {
        Vec3::new(col as f32 + 0.5, 0.0, row as f32 + 0.5)
    }

    fn rect(col: u32, row: u32, w: u32, h: u32) -> Bounds {
        Bounds::TileRect { col, row, width: w, height: h, layer: Some(0) }
    }

    fn npc(id: u64, col: u32, row: u32) -> Entity {
        Entity { id, category: 1, bounds: rect(col, row, 1, 1), blocking: NPC, flags: flags::PATHFIND_ADJACENT, sort_key: 0 }
    }

    fn exit(id: u64, col: u32, row: u32, h: u32) -> Entity {
        Entity { id, category: 2, bounds: rect(col, row, 1, h), blocking: EXIT, flags: 0, sort_key: 0 }
    }

    /// 5x3 open corridor grid.
    fn ctx() -> TileContext {
        TileContext::new(TileGrid::from_walkable(5, 3, 1, &[1; 15]).unwrap())
    }

    #[test]
    fn blocked_groups_force_a_detour() {
        let mut c = ctx();
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
        let other = c.grid().node_id(3, 1, 0).unwrap();
        c.add_groups(other, NPC);
        let opts = PathOptions { blocked_mask: NPC, goal_mode: GoalMode::Adjacent, ..Default::default() };
        let p = c.find_path_to_nodes(cell(0, 1), &[npc], opts).unwrap();
        let last = *p.nodes.last().unwrap();
        assert!(last == c.grid().node_id(4, 0, 0).unwrap() || last == c.grid().node_id(4, 2, 0).unwrap());
        assert!(!p.nodes.contains(&npc) && !p.nodes.contains(&other));
        assert_eq!(c.find_path_to_nodes(cell(4, 0), &[npc], opts).unwrap().nodes.len(), 1);
        assert_eq!(c.find_path_to_nodes(cell(0, 1), &[9999], opts), None);
    }

    #[test]
    fn start_inside_a_wall_snaps_to_the_nearest_floor() {
        let mut w = [1u8; 15];
        w[7] = 0;
        let c = TileContext::new(TileGrid::from_walkable(5, 3, 1, &w).unwrap());
        assert_eq!(c.find_path(cell(2, 1), cell(4, 1), PathOptions::default()), None, "no snapping by default");
        let p = c.find_path(cell(2, 1), cell(4, 1), PathOptions { snap_radius: 1, ..Default::default() }).unwrap();
        assert_ne!(p.nodes[0], 7);
        assert_eq!(*p.nodes.last().unwrap(), c.grid().node_id(4, 1, 0).unwrap());
    }

    #[test]
    fn reachability_honours_blocking() {
        let mut c = ctx();
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

    #[test]
    fn entities_project_into_blocking_groups_and_track_changes() {
        let mut c = ctx();
        let stats = c.sync_entities(&[npc(1, 2, 1), exit(2, 4, 0, 3)]);
        assert_eq!(stats, SyncStats { added: 2, updated: 0, removed: 0 });
        let mid = c.grid().node_id(2, 1, 0).unwrap();
        assert_eq!(c.groups(mid), Some(NPC));
        assert_eq!(c.entity_nodes(2).unwrap().len(), 3, "1x3 exit covers the whole last column");
        assert_eq!(c.groups(c.grid().node_id(4, 2, 0).unwrap()), Some(EXIT));

        // Unchanged re-send: nothing happens. Moving the NPC frees its old cell.
        assert_eq!(c.sync_entities(&[npc(1, 2, 1), exit(2, 4, 0, 3)]), SyncStats::default());
        let stats = c.sync_entities(&[npc(1, 2, 0), exit(2, 4, 0, 3)]);
        assert_eq!(stats, SyncStats { added: 0, updated: 1, removed: 0 });
        assert_eq!(c.groups(mid), Some(0));
        assert_eq!(c.groups(c.grid().node_id(2, 0, 0).unwrap()), Some(NPC));

        // Manual and entity groups combine; clearing manual groups leaves entity ones.
        c.add_groups(mid, EXIT);
        assert_eq!(c.groups(mid), Some(EXIT));
        c.clear_groups(EXIT);
        assert_eq!(c.groups(c.grid().node_id(4, 2, 0).unwrap()), Some(EXIT), "entity-derived survives clear");

        // Dropping the exit from the snapshot removes it and its groups.
        let stats = c.sync_entities(&[npc(1, 2, 0)]);
        assert_eq!(stats.removed, 1);
        assert_eq!(c.entity_count(), 1);
        assert_eq!(c.groups(c.grid().node_id(4, 2, 0).unwrap()), Some(0));
        assert_eq!(c.entity_nodes(2), None);

        assert_eq!(c.upsert_entity(npc(3, 9, 9)), Change::Added);
        assert_eq!(c.entity_nodes(3), Some(&[][..]), "off-grid entity projects to nothing");
        assert!(c.remove_entity(3));
        assert!(!c.remove_entity(3));
    }

    #[test]
    fn paths_to_entities_use_their_flag_and_nearest_points_are_geometric() {
        let mut c = ctx();
        c.sync_entities(&[npc(1, 4, 1), exit(2, 4, 0, 1)]);
        let opts = PathOptions { blocked_mask: NPC | EXIT, ..Default::default() };

        let talk = c.find_path_to_entity(cell(0, 1), 1, opts).unwrap();
        assert!(!talk.nodes.contains(&c.grid().node_id(4, 1, 0).unwrap()), "adjacent flag: never steps on the NPC");
        let walk_onto = c.find_path_to_entity(cell(0, 1), 2, opts).unwrap();
        assert_eq!(*walk_onto.nodes.last().unwrap(), c.grid().node_id(4, 0, 0).unwrap(), "no flag: ends on the exit");
        assert_eq!(c.find_path_to_entity(cell(0, 1), 42, opts), None);

        assert_eq!(c.entity_nearest_point(1, cell(0, 1)), Some(Vec3::new(4.0, 0.0, 1.5)));
    }

    #[test]
    fn profiles_gate_paths_by_clearance() {
        // 4x3 open grid with a wall at (1,1): a 2x2 agent only fits anchored at (2,0)/(2,1).
        let mut w = [1u8; 12];
        w[5] = 0;
        let mut c = TileContext::new(TileGrid::from_walkable(4, 3, 1, &w).unwrap());
        let big = c.register_profile(Profile { footprint: 2, ..Default::default() }).unwrap();
        assert_eq!(big, 1);
        assert_eq!(c.register_profile(Profile { footprint: 0, ..Default::default() }), None);

        let small = PathOptions::default();
        let wide = PathOptions { profile: big, ..Default::default() };
        assert_eq!(c.find_path(cell(0, 0), cell(2, 1), small).unwrap().nodes.len(), 4);
        assert_eq!(c.find_path(cell(0, 0), cell(2, 1), wide), None, "no 2x2-clear route out of the corner");
        assert_eq!(c.find_path(cell(2, 0), cell(2, 1), wide).unwrap().nodes.len(), 2, "between the two anchors that fit");
        assert_eq!(c.find_path(cell(0, 0), cell(2, 1), PathOptions { profile: 9, ..Default::default() }), None, "unknown profile");

        let reach = c.reachable(cell(2, 0), wide).unwrap();
        assert_eq!(reach.iter().filter(|&&r| r).count(), 2, "only the two fitting anchors are reachable for the big agent");
        assert_eq!(c.reachable(cell(2, 0), small).unwrap().iter().filter(|&&r| r).count(), 11);
    }

    #[test]
    fn entity_reachability_counts_adjacent_standing_room() {
        let mut c = ctx();
        c.sync_entities(&[npc(1, 2, 1)]);
        let opts = PathOptions { blocked_mask: NPC, ..Default::default() };
        let reached = c.reachable(cell(0, 1), opts).unwrap();
        assert!(!reached[c.grid().node_id(2, 1, 0).unwrap() as usize], "the NPC's own cell is blocked");
        assert_eq!(c.entity_reachable(&reached, 1), Some(true), "...but you can stand next to it");

        // Wall it in with more NPCs: nothing adjacent is reachable any more.
        c.sync_entities(&[npc(1, 2, 1), npc(2, 1, 1), npc(3, 3, 1), npc(4, 2, 0), npc(5, 2, 2)]);
        let reached = c.reachable(cell(0, 1), opts).unwrap();
        assert_eq!(c.entity_reachable(&reached, 1), Some(false));
        assert_eq!(c.entity_reachable(&reached, 99), None);
    }
}
