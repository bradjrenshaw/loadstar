//! A context bundles one map with everything that must stay consistent with it (spec §4):
//! the grid, the entity registry, the entities' projections into nodes, and the blocking
//! overlay derived from them. This is the only place entities and the map meet, and they
//! meet exclusively through projection (spec §2).
//!
//! The map is optional: a context without one ([`Context::without_map`]) still has the
//! registry, the player, entity vectors and trackers — everything that is pure geometry —
//! for games whose spaces the core can't (or needn't) model. Every entity then behaves as
//! off-grid: it projects to no nodes, never blocks, and never passes a reachability filter.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::entities::{Change, Entity, EntityId, Registry, SyncStats, flags};
use crate::geometry::Vec3;
use crate::graph::{NodeId, Path, Query, astar_query, flood_fill};
use crate::tile::TileGrid;
use crate::tracker::{Action, Filter, Order, Selection, Tracker};

/// Announcement data for an entity relative to the player (spec §8 `entity_vector`).
/// Pure geometry; the companion decides how to phrase it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EntityVector {
    /// Closest point of the entity's bounds to the player.
    pub nearest_point: Vec3,
    /// Compass bearing to that point, degrees clockwise from north (-Z), in `[0, 360)`.
    pub bearing_deg: f32,
    /// Angle above (+) or below (-) the horizontal plane, degrees.
    pub elevation_deg: f32,
    /// Straight-line distance.
    pub distance: f32,
    /// Horizontal Manhattan distance — the natural step count on a 4-connected grid.
    pub steps: f32,
}

/// Reachability sets keyed by `(profile, blocked_mask)`, valid for one `(grid, overlay)`
/// version pair. Cleared wholesale when either changes.
#[derive(Default)]
struct ReachCache {
    stamp: (u64, u64),
    sets: HashMap<(u32, u32), Rc<Vec<bool>>>,
}

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
        Profile {
            footprint: 1,
            radius: 0.5,
            height: 1.0,
            step_height: 0.0,
        }
    }
}

/// An entity's cached projection, remembering the blocking it contributed so a later
/// change can tell whether the overlay needs rebuilding.
struct Projection {
    blocking: u32,
    nodes: Vec<NodeId>,
}

pub struct Context {
    /// `None` for a context without a map.
    grid: Option<TileGrid>,
    /// Groups set directly through the API. OR'd with `entity_groups` for blocking.
    manual_groups: Vec<u32>,
    /// Groups derived from blocking entities' projections. Rebuilt when they change.
    entity_groups: Vec<u32>,
    registry: Registry,
    projections: HashMap<EntityId, Projection>,
    /// Index 0 is the built-in default agent.
    profiles: Vec<Profile>,
    player: Vec3,
    /// Bumped whenever groups or the player position change (the grid has its own version).
    overlay_version: u64,
    reach_cache: RefCell<ReachCache>,
    trackers: Vec<Option<Tracker>>,
}

impl Context {
    /// A tile-grid context.
    pub fn new(grid: TileGrid) -> Self {
        let n = grid.width() as usize * grid.height() as usize * grid.layers() as usize;
        Self::build(Some(grid), n)
    }

    /// A context with no map: entities, player, vectors and trackers only.
    pub fn without_map() -> Self {
        Self::build(None, 0)
    }

    fn build(grid: Option<TileGrid>, n: usize) -> Self {
        Context {
            grid,
            manual_groups: vec![0; n],
            entity_groups: vec![0; n],
            registry: Registry::default(),
            projections: HashMap::new(),
            profiles: vec![Profile::default()],
            player: Vec3::default(),
            overlay_version: 0,
            reach_cache: RefCell::new(ReachCache::default()),
            trackers: Vec::new(),
        }
    }

    // --- Player ------------------------------------------------------------------------

    /// Update the player's position (each frame). A real move invalidates cached reachability.
    pub fn set_player_position(&mut self, pos: Vec3) {
        if self.player != pos {
            self.player = pos;
            self.overlay_version += 1;
        }
    }

    pub fn player_position(&self) -> Vec3 {
        self.player
    }

    /// Reachability from the player under `(profile, blocked_mask)`, cached until the grid,
    /// the groups or the player position change. `None` if the player is off-grid or the
    /// profile is unknown.
    pub fn reachable_from_player(&self, profile: u32, blocked_mask: u32) -> Option<Rc<Vec<bool>>> {
        let stamp = (self.grid.as_ref()?.version(), self.overlay_version);
        let mut cache = self.reach_cache.borrow_mut();
        if cache.stamp != stamp {
            cache.stamp = stamp;
            cache.sets.clear();
        }
        if let Some(set) = cache.sets.get(&(profile, blocked_mask)) {
            return Some(Rc::clone(set));
        }
        let set = Rc::new(self.reachable(
            self.player,
            PathOptions {
                blocked_mask,
                profile,
                ..Default::default()
            },
        )?);
        cache.sets.insert((profile, blocked_mask), Rc::clone(&set));
        Some(set)
    }

    /// [`Self::entity_reachable`] against the cached reachability from the player.
    pub fn entity_reachable_from_player(
        &self,
        id: EntityId,
        profile: u32,
        blocked_mask: u32,
    ) -> Option<bool> {
        let reached = self.reachable_from_player(profile, blocked_mask)?;
        self.entity_reachable(&reached, id)
    }

    /// Announcement geometry for an entity relative to the player.
    pub fn entity_vector(&self, id: EntityId) -> Option<EntityVector> {
        let nearest_point = self.entity_nearest_point(id, self.player)?;
        let d = nearest_point - self.player;
        let horizontal = d.horizontal_length();
        let bearing = if horizontal == 0.0 {
            0.0
        } else {
            d.x.atan2(-d.z).to_degrees().rem_euclid(360.0)
        };
        let elevation = if horizontal == 0.0 && d.y == 0.0 {
            0.0
        } else {
            d.y.atan2(horizontal).to_degrees()
        };
        Some(EntityVector {
            nearest_point,
            bearing_deg: bearing,
            elevation_deg: elevation,
            distance: d.length(),
            steps: d.manhattan_horizontal(),
        })
    }

    // --- Trackers ----------------------------------------------------------------------

    /// Add a tracker; returns its id. Ids of destroyed trackers are reused.
    pub fn tracker_create(&mut self, tracker: Tracker) -> u32 {
        if let Some(slot) = self.trackers.iter().position(Option::is_none) {
            self.trackers[slot] = Some(tracker);
            return slot as u32;
        }
        self.trackers.push(Some(tracker));
        self.trackers.len() as u32 - 1
    }

    pub fn tracker_destroy(&mut self, id: u32) -> bool {
        match self.trackers.get_mut(id as usize) {
            Some(slot) if slot.is_some() => {
                *slot = None;
                true
            }
            _ => false,
        }
    }

    pub fn tracker(&self, id: u32) -> Option<&Tracker> {
        self.trackers.get(id as usize)?.as_ref()
    }

    pub fn tracker_mut(&mut self, id: u32) -> Option<&mut Tracker> {
        self.trackers.get_mut(id as usize)?.as_mut()
    }

    /// Drive a tracker. `None` for an unknown tracker id.
    pub fn tracker_act(&mut self, id: u32, action: Action) -> Option<Selection> {
        let mut tracker = self.trackers.get_mut(id as usize)?.take()?;
        let result = self.act(&mut tracker, action);
        self.trackers[id as usize] = Some(tracker);
        Some(result)
    }

    /// Visible entity count under the tracker's filters, in one category or (`None`) all.
    pub fn tracker_count(&self, id: u32, category: Option<u16>) -> Option<u32> {
        let tracker = self.tracker(id)?;
        Some(
            self.registry
                .iter()
                .filter(|e| category.is_none_or(|c| e.category == c) && self.passes(tracker, e))
                .count() as u32,
        )
    }

    fn act(&self, t: &mut Tracker, action: Action) -> Selection {
        match action {
            Action::CategoryNext | Action::CategoryPrev => {
                let categories = self.tracker_categories(t);
                t.selected = None;
                if categories.is_empty() {
                    return Selection::none(t.current_category());
                }
                let next = match categories.iter().position(|c| *c == t.category) {
                    Some(p) if action == Action::CategoryNext => (p + 1) % categories.len(),
                    Some(p) => (p + categories.len() - 1) % categories.len(),
                    None => 0,
                };
                t.category = categories[next];
                Selection::none(t.current_category())
            }
            Action::GroupNext | Action::GroupPrev | Action::InGroupNext | Action::InGroupPrev => {
                if !t.include_all && t.category.is_none() {
                    t.category = self.tracker_categories(t).first().copied().flatten();
                }
                self.act_grouped(t, action)
            }
            Action::Next | Action::Prev | Action::Current => {
                if !t.include_all && t.category.is_none() {
                    t.category = self.tracker_categories(t).first().copied().flatten();
                }
                let list = self.tracker_visible(t);
                let position = t.selected.and_then(|s| list.iter().position(|e| e.id == s));
                let index = match (action, position) {
                    (_, _) if list.is_empty() => None,
                    (Action::Current, p) => p,
                    (Action::Next, Some(p)) => Some((p + 1) % list.len()),
                    (Action::Next, None) => Some(0),
                    (Action::Prev, Some(p)) => Some((p + list.len() - 1) % list.len()),
                    (Action::Prev, None) => Some(list.len() - 1),
                    _ => None,
                };
                match index {
                    Some(i) => {
                        let e = list[i];
                        t.selected = Some(e.id);
                        Selection {
                            entity_id: e.id,
                            category: u32::from(e.category),
                            index: i as u32,
                        }
                    }
                    None => {
                        t.selected = None;
                        Selection::none(t.current_category())
                    }
                }
            }
        }
    }

    /// Size of the selected entity's group in the tracker's current view (spec §8); 0 when
    /// nothing is selected or the selection is no longer visible. `None` for an unknown tracker.
    pub fn tracker_group_count(&self, id: u32) -> Option<u32> {
        let t = self.tracker(id)?;
        let list = self.tracker_visible(t);
        let Some(selected) = t.selected.and_then(|s| list.iter().find(|e| e.id == s)) else {
            return Some(0);
        };
        let key = group_key(selected);
        Some(list.iter().filter(|e| group_key(e) == key).count() as u32)
    }

    /// Group and in-group browsing over the visible list (spec §8).
    fn act_grouped(&self, t: &mut Tracker, action: Action) -> Selection {
        let list = self.tracker_visible(t);
        // Groups in order of their first member; each holds its members' list positions.
        let mut groups: Vec<((bool, u64), Vec<usize>)> = Vec::new();
        for (i, e) in list.iter().enumerate() {
            let key = group_key(e);
            match groups.iter_mut().find(|(k, _)| *k == key) {
                Some((_, members)) => members.push(i),
                None => groups.push((key, vec![i])),
            }
        }
        if groups.is_empty() {
            t.selected = None;
            return Selection::none(t.current_category());
        }
        let current = t
            .selected
            .and_then(|s| list.iter().position(|e| e.id == s))
            .and_then(|p| {
                groups.iter().enumerate().find_map(|(g, (_, m))| {
                    m.iter().position(|&i| i == p).map(|within| (g, within))
                })
            });
        let n = groups.len();
        let (g, within) = match (action, current) {
            (Action::GroupNext, Some((g, _))) => ((g + 1) % n, 0),
            (Action::GroupPrev, Some((g, _))) => ((g + n - 1) % n, 0),
            (Action::GroupPrev, None) => (n - 1, 0),
            (Action::InGroupNext, Some((g, w))) => (g, (w + 1) % groups[g].1.len()),
            (Action::InGroupPrev, Some((g, w))) => {
                let m = groups[g].1.len();
                (g, (w + m - 1) % m)
            }
            // GroupNext or in-group browsing with nothing selected: the first group's first.
            _ => (0, 0),
        };
        let e = list[groups[g].1[within]];
        t.selected = Some(e.id);
        Selection {
            entity_id: e.id,
            category: u32::from(e.category),
            index: within as u32,
        }
    }

    /// Categories in cycle order: the ALL view first if configured, then every category
    /// that currently has a visible entity, ascending.
    fn tracker_categories(&self, t: &Tracker) -> Vec<Option<u16>> {
        let mut cats: Vec<u16> = self
            .registry
            .iter()
            .filter(|e| self.passes(t, e))
            .map(|e| e.category)
            .collect();
        cats.sort_unstable();
        cats.dedup();
        let mut out = Vec::with_capacity(cats.len() + 1);
        if t.include_all {
            out.push(None);
        }
        out.extend(cats.into_iter().map(Some));
        out
    }

    /// Visible entities in the tracker's current category (or all), in cycle order: per
    /// [`Order`], ties broken by `(category, sort_key, id)`.
    fn tracker_visible(&self, t: &Tracker) -> Vec<&Entity> {
        let visible = self
            .registry
            .iter()
            .filter(|e| t.category.is_none_or(|c| e.category == c) && self.passes(t, e));
        let key = |e: &Entity| (e.category, e.sort_key, e.id);
        match t.order {
            Order::SortKey => {
                let mut list: Vec<&Entity> = visible.collect();
                list.sort_by_key(|e| key(e));
                list
            }
            Order::Distance => {
                let mut list: Vec<(f32, &Entity)> = visible
                    .map(|e| {
                        (
                            (e.bounds.nearest_point(self.player) - self.player).length(),
                            e,
                        )
                    })
                    .collect();
                list.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| key(a.1).cmp(&key(b.1))));
                list.into_iter().map(|(_, e)| e).collect()
            }
        }
    }

    fn passes(&self, t: &Tracker, e: &Entity) -> bool {
        t.filters
            .iter()
            .filter(|f| f.enabled)
            .all(|f| match f.filter {
                Filter::Reachable {
                    profile,
                    blocked_mask,
                } => self.entity_reachable_from_player(e.id, profile, blocked_mask) == Some(true),
                Filter::Range { max_distance } => {
                    (e.bounds.nearest_point(self.player) - self.player).length() <= max_distance
                }
                Filter::Categories { mask } => e.category < 64 && mask & (1u64 << e.category) != 0,
                Filter::Flags { require, forbid } => {
                    e.flags & require == require && e.flags & forbid == 0
                }
            })
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
    fn fits(grid: &TileGrid, node: NodeId, footprint: u32) -> bool {
        footprint <= 1 || grid.clearance(node) as u32 >= footprint
    }

    /// The map, or `None` for a context without one.
    pub fn grid(&self) -> Option<&TileGrid> {
        self.grid.as_ref()
    }

    pub fn grid_mut(&mut self) -> Option<&mut TileGrid> {
        self.grid.as_mut()
    }

    // --- Groups ------------------------------------------------------------------------

    /// Effective groups of a node: manual OR entity-derived. This is what blocking sees.
    pub fn groups(&self, node: NodeId) -> Option<u32> {
        Some(*self.manual_groups.get(node as usize)? | self.entity_groups[node as usize])
    }

    pub fn set_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.overlay_version += 1;
        self.manual_groups
            .get_mut(node as usize)
            .map(|g| *g = mask)
            .is_some()
    }

    pub fn add_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.overlay_version += 1;
        self.manual_groups
            .get_mut(node as usize)
            .map(|g| *g |= mask)
            .is_some()
    }

    pub fn remove_groups(&mut self, node: NodeId, mask: u32) -> bool {
        self.overlay_version += 1;
        self.manual_groups
            .get_mut(node as usize)
            .map(|g| *g &= !mask)
            .is_some()
    }

    /// Remove `mask` from every node's manual groups. Entity-derived groups are unaffected.
    pub fn clear_groups(&mut self, mask: u32) {
        self.overlay_version += 1;
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
        if self
            .projections
            .remove(&id)
            .is_some_and(|p| p.blocking != 0)
        {
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
            rebuild |= self
                .projections
                .remove(&id)
                .is_some_and(|p| p.blocking != 0);
        }
        if rebuild {
            self.rebuild_entity_groups();
        }
        stats
    }

    /// Recompute one entity's projection. Returns whether the blocking overlay is affected.
    fn reproject(&mut self, id: EntityId) -> bool {
        let Some(entity) = self.registry.get(id) else {
            return false;
        };
        let nodes = self
            .grid
            .as_ref()
            .map_or_else(Vec::new, |g| g.project(&entity.bounds));
        let old_blocking = self.projections.get(&id).map_or(0, |p| p.blocking);
        self.projections.insert(
            id,
            Projection {
                blocking: entity.blocking,
                nodes,
            },
        );
        old_blocking != 0 || entity.blocking != 0
    }

    fn rebuild_entity_groups(&mut self) {
        self.overlay_version += 1;
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
    pub fn find_path_to_entity(
        &self,
        start: Vec3,
        id: EntityId,
        opts: PathOptions,
    ) -> Option<Path> {
        let entity = self.registry.get(id)?;
        let nodes = self.entity_nodes(id)?;
        if nodes.is_empty() {
            return None;
        }
        let goal_mode = if entity.flags & flags::PATHFIND_ADJACENT != 0 {
            GoalMode::Adjacent
        } else {
            GoalMode::Onto
        };
        self.find_path_to_nodes(start, nodes, PathOptions { goal_mode, ..opts })
    }

    /// Whether an entity counts as reachable given a reachability set (spec §8): any of its
    /// nodes, *or any node one step away from them*, is reachable — an NPC standing on a
    /// blocked tile is reachable if you can stand next to it.
    pub fn entity_reachable(&self, reached: &[bool], id: EntityId) -> Option<bool> {
        let nodes = self.entity_nodes(id)?;
        let hit = |n: &NodeId| reached.get(*n as usize).copied().unwrap_or(false);
        let adjacent = self
            .grid
            .as_ref()
            .map_or_else(Vec::new, |g| g.adjacent_candidates(nodes));
        Some(nodes.iter().any(hit) || adjacent.iter().any(hit))
    }

    // --- Queries -----------------------------------------------------------------------

    /// Path from the node containing `start` to the node containing `goal`.
    /// `None` if a position is off-grid or no route exists.
    pub fn find_path(&self, start: Vec3, goal: Vec3, opts: PathOptions) -> Option<Path> {
        let goal = self.grid.as_ref()?.node_from_position(goal)?;
        self.find_path_to_nodes(start, &[goal], opts)
    }

    /// Path from the node containing `start` to the cheapest of `targets`, interpreted per
    /// `opts.goal_mode`. Target ids must be in range; `opts.profile` must be registered.
    pub fn find_path_to_nodes(
        &self,
        start: Vec3,
        targets: &[NodeId],
        opts: PathOptions,
    ) -> Option<Path> {
        if targets
            .iter()
            .any(|&t| t as usize >= self.manual_groups.len())
        {
            return None;
        }
        let grid = self.grid.as_ref()?;
        let footprint = self.profile(opts.profile)?.footprint;
        let start = self.snap_start(grid, grid.node_from_position(start)?, opts, footprint)?;
        let (goals, exempt): (Vec<NodeId>, HashSet<NodeId>) = match opts.goal_mode {
            GoalMode::Onto => (targets.to_vec(), targets.iter().copied().collect()),
            GoalMode::Adjacent => (grid.adjacent_candidates(targets), HashSet::new()),
        };
        // Group blocking may be exempted for targets; fitting never is.
        let is_blocked = |n: NodeId| {
            !Self::fits(grid, n, footprint)
                || (self.effective_groups(n) & opts.blocked_mask != 0 && !exempt.contains(&n))
        };
        astar_query(
            grid,
            &Query {
                start,
                goals: &goals,
                is_blocked: &is_blocked,
            },
        )
    }

    /// Reachability from the node containing `start`, indexed by node id.
    /// `None` if `start` is off-grid (or cannot be snapped), the profile is unknown, or the
    /// context has no map.
    pub fn reachable(&self, start: Vec3, opts: PathOptions) -> Option<Vec<bool>> {
        let grid = self.grid.as_ref()?;
        let footprint = self.profile(opts.profile)?.footprint;
        let start = self.snap_start(grid, grid.node_from_position(start)?, opts, footprint)?;
        let is_blocked = |n: NodeId| {
            !Self::fits(grid, n, footprint) || self.effective_groups(n) & opts.blocked_mask != 0
        };
        Some(flood_fill(grid, start, &is_blocked))
    }

    /// The start is usable as-is if an agent can stand there (it has edges); blocking and
    /// fit are irrelevant because the agent is already there. Otherwise snap to the nearest
    /// node that has edges, fits, and is not blocked.
    fn snap_start(
        &self,
        grid: &TileGrid,
        node: NodeId,
        opts: PathOptions,
        footprint: u32,
    ) -> Option<NodeId> {
        if grid.has_edges(node) {
            return Some(node);
        }
        grid.nearest_node(node, opts.snap_radius, |n| {
            grid.has_edges(n)
                && Self::fits(grid, n, footprint)
                && self.effective_groups(n) & opts.blocked_mask == 0
        })
    }
}

/// An entity's group identity: its nonzero `group`, or (ungrouped) the entity itself.
fn group_key(e: &Entity) -> (bool, u64) {
    if e.group == 0 {
        (false, e.id)
    } else {
        (true, u64::from(e.group))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Bounds;
    use crate::tracker::{ALL, Action, Filter, Selection, Tracker};

    const NPC: u32 = 1 << 0;
    const EXIT: u32 = 1 << 1;

    fn cell(col: u32, row: u32) -> Vec3 {
        Vec3::new(col as f32 + 0.5, 0.0, row as f32 + 0.5)
    }

    fn rect(col: u32, row: u32, w: u32, h: u32) -> Bounds {
        Bounds::TileRect {
            col,
            row,
            width: w,
            height: h,
            layer: Some(0),
        }
    }

    fn npc(id: u64, col: u32, row: u32) -> Entity {
        Entity {
            id,
            category: 1,
            bounds: rect(col, row, 1, 1),
            blocking: NPC,
            flags: flags::PATHFIND_ADJACENT,
            sort_key: 0,
            group: 0,
        }
    }

    fn exit(id: u64, col: u32, row: u32, h: u32) -> Entity {
        Entity {
            id,
            category: 2,
            bounds: rect(col, row, 1, h),
            blocking: EXIT,
            flags: 0,
            sort_key: 0,
            group: 0,
        }
    }

    /// 5x3 open corridor grid.
    fn ctx() -> Context {
        Context::new(TileGrid::from_walkable(5, 3, 1, &[1; 15]).unwrap())
    }

    #[test]
    fn blocked_groups_force_a_detour() {
        let mut c = ctx();
        let npc = c.grid().unwrap().node_id(2, 1, 0).unwrap();
        assert!(c.add_groups(npc, NPC));
        let straight = c
            .find_path(cell(0, 1), cell(4, 1), PathOptions::default())
            .unwrap();
        assert_eq!(
            straight.nodes.len(),
            5,
            "ignoring groups walks straight through"
        );
        let around = c
            .find_path(
                cell(0, 1),
                cell(4, 1),
                PathOptions {
                    blocked_mask: NPC,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_eq!(around.nodes.len(), 7);
        assert!(!around.nodes.contains(&npc));
        assert!(!c.set_groups(999, NPC));
    }

    #[test]
    fn onto_exempts_targets_from_blocking() {
        let mut c = ctx();
        let exit = c.grid().unwrap().node_id(4, 1, 0).unwrap();
        c.add_groups(exit, EXIT);
        let opts = PathOptions {
            blocked_mask: EXIT | NPC,
            ..Default::default()
        };
        let p = c.find_path(cell(0, 1), cell(4, 1), opts).unwrap();
        assert_eq!(*p.nodes.last().unwrap(), exit);
    }

    #[test]
    fn adjacent_stops_next_to_the_target_and_avoids_other_blockers() {
        let mut c = ctx();
        let npc = c.grid().unwrap().node_id(4, 1, 0).unwrap();
        c.add_groups(npc, NPC);
        let other = c.grid().unwrap().node_id(3, 1, 0).unwrap();
        c.add_groups(other, NPC);
        let opts = PathOptions {
            blocked_mask: NPC,
            goal_mode: GoalMode::Adjacent,
            ..Default::default()
        };
        let p = c.find_path_to_nodes(cell(0, 1), &[npc], opts).unwrap();
        let last = *p.nodes.last().unwrap();
        assert!(
            last == c.grid().unwrap().node_id(4, 0, 0).unwrap()
                || last == c.grid().unwrap().node_id(4, 2, 0).unwrap()
        );
        assert!(!p.nodes.contains(&npc) && !p.nodes.contains(&other));
        assert_eq!(
            c.find_path_to_nodes(cell(4, 0), &[npc], opts)
                .unwrap()
                .nodes
                .len(),
            1
        );
        assert_eq!(c.find_path_to_nodes(cell(0, 1), &[9999], opts), None);
    }

    #[test]
    fn start_inside_a_wall_snaps_to_the_nearest_floor() {
        let mut w = [1u8; 15];
        w[7] = 0;
        let c = Context::new(TileGrid::from_walkable(5, 3, 1, &w).unwrap());
        assert_eq!(
            c.find_path(cell(2, 1), cell(4, 1), PathOptions::default()),
            None,
            "no snapping by default"
        );
        let p = c
            .find_path(
                cell(2, 1),
                cell(4, 1),
                PathOptions {
                    snap_radius: 1,
                    ..Default::default()
                },
            )
            .unwrap();
        assert_ne!(p.nodes[0], 7);
        assert_eq!(
            *p.nodes.last().unwrap(),
            c.grid().unwrap().node_id(4, 1, 0).unwrap()
        );
    }

    #[test]
    fn reachability_honours_blocking() {
        let mut c = ctx();
        for row in 0..3 {
            c.add_groups(c.grid().unwrap().node_id(2, row, 0).unwrap(), NPC);
        }
        let open = c.reachable(cell(0, 0), PathOptions::default()).unwrap();
        assert!(open.iter().all(|&r| r));
        let blocked = c
            .reachable(
                cell(0, 0),
                PathOptions {
                    blocked_mask: NPC,
                    ..Default::default()
                },
            )
            .unwrap();
        assert!(blocked[c.grid().unwrap().node_id(1, 2, 0).unwrap() as usize]);
        assert!(!blocked[c.grid().unwrap().node_id(3, 0, 0).unwrap() as usize]);
        c.clear_groups(NPC);
        assert!(
            c.reachable(
                cell(0, 0),
                PathOptions {
                    blocked_mask: NPC,
                    ..Default::default()
                }
            )
            .unwrap()
            .iter()
            .all(|&r| r)
        );
    }

    #[test]
    fn entities_project_into_blocking_groups_and_track_changes() {
        let mut c = ctx();
        let stats = c.sync_entities(&[npc(1, 2, 1), exit(2, 4, 0, 3)]);
        assert_eq!(
            stats,
            SyncStats {
                added: 2,
                updated: 0,
                removed: 0
            }
        );
        let mid = c.grid().unwrap().node_id(2, 1, 0).unwrap();
        assert_eq!(c.groups(mid), Some(NPC));
        assert_eq!(
            c.entity_nodes(2).unwrap().len(),
            3,
            "1x3 exit covers the whole last column"
        );
        assert_eq!(
            c.groups(c.grid().unwrap().node_id(4, 2, 0).unwrap()),
            Some(EXIT)
        );

        // Unchanged re-send: nothing happens. Moving the NPC frees its old cell.
        assert_eq!(
            c.sync_entities(&[npc(1, 2, 1), exit(2, 4, 0, 3)]),
            SyncStats::default()
        );
        let stats = c.sync_entities(&[npc(1, 2, 0), exit(2, 4, 0, 3)]);
        assert_eq!(
            stats,
            SyncStats {
                added: 0,
                updated: 1,
                removed: 0
            }
        );
        assert_eq!(c.groups(mid), Some(0));
        assert_eq!(
            c.groups(c.grid().unwrap().node_id(2, 0, 0).unwrap()),
            Some(NPC)
        );

        // Manual and entity groups combine; clearing manual groups leaves entity ones.
        c.add_groups(mid, EXIT);
        assert_eq!(c.groups(mid), Some(EXIT));
        c.clear_groups(EXIT);
        assert_eq!(
            c.groups(c.grid().unwrap().node_id(4, 2, 0).unwrap()),
            Some(EXIT),
            "entity-derived survives clear"
        );

        // Dropping the exit from the snapshot removes it and its groups.
        let stats = c.sync_entities(&[npc(1, 2, 0)]);
        assert_eq!(stats.removed, 1);
        assert_eq!(c.entity_count(), 1);
        assert_eq!(
            c.groups(c.grid().unwrap().node_id(4, 2, 0).unwrap()),
            Some(0)
        );
        assert_eq!(c.entity_nodes(2), None);

        assert_eq!(c.upsert_entity(npc(3, 9, 9)), Change::Added);
        assert_eq!(
            c.entity_nodes(3),
            Some(&[][..]),
            "off-grid entity projects to nothing"
        );
        assert!(c.remove_entity(3));
        assert!(!c.remove_entity(3));
    }

    #[test]
    fn paths_to_entities_use_their_flag_and_nearest_points_are_geometric() {
        let mut c = ctx();
        c.sync_entities(&[npc(1, 4, 1), exit(2, 4, 0, 1)]);
        let opts = PathOptions {
            blocked_mask: NPC | EXIT,
            ..Default::default()
        };

        let talk = c.find_path_to_entity(cell(0, 1), 1, opts).unwrap();
        assert!(
            !talk
                .nodes
                .contains(&c.grid().unwrap().node_id(4, 1, 0).unwrap()),
            "adjacent flag: never steps on the NPC"
        );
        let walk_onto = c.find_path_to_entity(cell(0, 1), 2, opts).unwrap();
        assert_eq!(
            *walk_onto.nodes.last().unwrap(),
            c.grid().unwrap().node_id(4, 0, 0).unwrap(),
            "no flag: ends on the exit"
        );
        assert_eq!(c.find_path_to_entity(cell(0, 1), 42, opts), None);

        assert_eq!(
            c.entity_nearest_point(1, cell(0, 1)),
            Some(Vec3::new(4.0, 0.0, 1.5))
        );
    }

    #[test]
    fn profiles_gate_paths_by_clearance() {
        // 4x3 open grid with a wall at (1,1): a 2x2 agent only fits anchored at (2,0)/(2,1).
        let mut w = [1u8; 12];
        w[5] = 0;
        let mut c = Context::new(TileGrid::from_walkable(4, 3, 1, &w).unwrap());
        let big = c
            .register_profile(Profile {
                footprint: 2,
                ..Default::default()
            })
            .unwrap();
        assert_eq!(big, 1);
        assert_eq!(
            c.register_profile(Profile {
                footprint: 0,
                ..Default::default()
            }),
            None
        );

        let small = PathOptions::default();
        let wide = PathOptions {
            profile: big,
            ..Default::default()
        };
        assert_eq!(
            c.find_path(cell(0, 0), cell(2, 1), small)
                .unwrap()
                .nodes
                .len(),
            4
        );
        assert_eq!(
            c.find_path(cell(0, 0), cell(2, 1), wide),
            None,
            "no 2x2-clear route out of the corner"
        );
        assert_eq!(
            c.find_path(cell(2, 0), cell(2, 1), wide)
                .unwrap()
                .nodes
                .len(),
            2,
            "between the two anchors that fit"
        );
        assert_eq!(
            c.find_path(
                cell(0, 0),
                cell(2, 1),
                PathOptions {
                    profile: 9,
                    ..Default::default()
                }
            ),
            None,
            "unknown profile"
        );

        let reach = c.reachable(cell(2, 0), wide).unwrap();
        assert_eq!(
            reach.iter().filter(|&&r| r).count(),
            2,
            "only the two fitting anchors are reachable for the big agent"
        );
        assert_eq!(
            c.reachable(cell(2, 0), small)
                .unwrap()
                .iter()
                .filter(|&&r| r)
                .count(),
            11
        );
    }

    fn chest(id: u64, col: u32, row: u32, sort_key: u32) -> Entity {
        Entity {
            id,
            category: 3,
            bounds: rect(col, row, 1, 1),
            blocking: 0,
            flags: 0,
            sort_key,
            group: 0,
        }
    }

    fn filters(list: &[Filter]) -> Vec<crate::tracker::FilterEntry> {
        list.iter()
            .map(|&filter| crate::tracker::FilterEntry {
                filter,
                enabled: true,
            })
            .collect()
    }

    #[test]
    fn entity_vector_reports_bearing_distance_and_steps() {
        let mut c = ctx();
        c.sync_entities(&[chest(1, 4, 1, 0), exit(2, 0, 0, 1)]);
        c.set_player_position(cell(0, 1));
        let v = c.entity_vector(1).unwrap();
        assert_eq!(v.nearest_point, Vec3::new(4.0, 0.0, 1.5));
        assert_eq!((v.bearing_deg, v.elevation_deg), (90.0, 0.0), "due east");
        assert_eq!((v.distance, v.steps), (3.5, 3.5));
        let north = c.entity_vector(2).unwrap();
        assert!(
            (north.bearing_deg - 0.0).abs() < 1e-3,
            "due north, got {}",
            north.bearing_deg
        );
        assert_eq!(c.entity_vector(9), None);
    }

    #[test]
    fn reachability_cache_tracks_every_kind_of_change() {
        let mut c = ctx();
        c.set_player_position(cell(0, 1));
        let a = c.reachable_from_player(0, NPC).unwrap();
        assert!(
            Rc::ptr_eq(&a, &c.reachable_from_player(0, NPC).unwrap()),
            "same stamp: shared set"
        );
        assert!(a.iter().all(|&r| r));

        c.sync_entities(&[npc(1, 2, 0), npc(2, 2, 1), npc(3, 2, 2)]);
        let b = c.reachable_from_player(0, NPC).unwrap();
        assert!(!Rc::ptr_eq(&a, &b), "entity groups changed → recomputed");
        assert!(!b[c.grid().unwrap().node_id(4, 1, 0).unwrap() as usize]);

        c.grid_mut()
            .unwrap()
            .add_link(
                1,
                crate::tile::Link {
                    from: 5,
                    to: 9,
                    cost: 1.0,
                    bidirectional: false,
                    enabled: true,
                },
            )
            .unwrap();
        assert!(
            c.reachable_from_player(0, NPC).unwrap()[9],
            "grid change → recomputed, teleporter crosses the wall"
        );

        c.set_player_position(cell(4, 1));
        assert!(
            c.reachable_from_player(0, NPC).unwrap()[14],
            "player moved → recomputed from the new side"
        );
        assert_eq!(
            c.reachable_from_player(0, NPC)
                .unwrap()
                .iter()
                .filter(|&&r| r)
                .count(),
            6,
            "the east side only"
        );
    }

    #[test]
    fn tracker_cycles_entities_in_sort_order_and_categories_ascending() {
        let mut c = ctx();
        c.sync_entities(&[
            chest(30, 3, 0, 1),
            chest(31, 1, 0, 0),
            npc(10, 4, 2),
            exit(20, 4, 0, 1),
        ]);
        let t = c.tracker_create(Tracker::new(Vec::new(), true));
        assert_eq!(t, 0);

        // ALL view first: entities ordered by (category, sort_key, id) → npc(1), exit(2), chests(3).
        assert_eq!(
            c.tracker_act(t, Action::Next),
            Some(Selection {
                entity_id: 10,
                category: 1,
                index: 0
            })
        );
        assert_eq!(
            c.tracker_act(t, Action::Next),
            Some(Selection {
                entity_id: 20,
                category: 2,
                index: 1
            })
        );
        assert_eq!(
            c.tracker_act(t, Action::Next).unwrap().entity_id,
            31,
            "sort_key 0 before sort_key 1"
        );
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 30);
        assert_eq!(
            c.tracker_act(t, Action::Next).unwrap().entity_id,
            10,
            "wraps"
        );
        assert_eq!(
            c.tracker_act(t, Action::Prev).unwrap().entity_id,
            30,
            "wraps backwards"
        );
        assert_eq!(c.tracker_act(t, Action::Current).unwrap().index, 3);
        assert_eq!(c.tracker_count(t, None), Some(4));

        // Category cycling: ALL → 1 → 2 → 3 → ALL; selection resets.
        assert_eq!(
            c.tracker_act(t, Action::CategoryNext),
            Some(Selection::none(1))
        );
        assert_eq!(
            c.tracker_act(t, Action::CategoryNext),
            Some(Selection::none(2))
        );
        assert_eq!(
            c.tracker_act(t, Action::CategoryNext),
            Some(Selection::none(3))
        );
        assert_eq!(c.tracker_count(t, Some(3)), Some(2));
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 31);
        assert_eq!(
            c.tracker_act(t, Action::CategoryNext),
            Some(Selection::none(ALL))
        );
        assert_eq!(
            c.tracker_act(t, Action::CategoryPrev),
            Some(Selection::none(3))
        );

        // The selected entity vanishing: Current reports nothing, Next restarts.
        c.sync_entities(&[chest(30, 3, 0, 1), npc(10, 4, 2), exit(20, 4, 0, 1)]);
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 30);
        c.sync_entities(&[npc(10, 4, 2), exit(20, 4, 0, 1)]);
        assert_eq!(c.tracker_act(t, Action::Current), Some(Selection::none(3)));
        assert_eq!(
            c.tracker_act(t, Action::CategoryNext),
            Some(Selection::none(ALL)),
            "category 3 is gone"
        );
        assert_eq!(c.tracker_act(9, Action::Next), None);
        assert!(c.tracker_destroy(t) && !c.tracker_destroy(t));
        assert_eq!(
            c.tracker_create(Tracker::new(Vec::new(), false)),
            0,
            "slot reused"
        );
    }

    #[test]
    fn filters_are_data_and_combine_with_and() {
        let mut c = ctx();
        c.set_player_position(cell(0, 1));
        // Two chests east of a wall of NPCs; one chest is hidden by convention.
        let mut hidden = chest(32, 4, 2, 0);
        hidden.flags = flags::HIDDEN;
        c.sync_entities(&[
            npc(1, 2, 0),
            npc(2, 2, 1),
            npc(3, 2, 2),
            chest(30, 4, 0, 0),
            chest(31, 1, 2, 0),
            hidden,
        ]);

        let no_filters = c.tracker_create(Tracker::new(Vec::new(), true));
        assert_eq!(
            c.tracker_count(no_filters, None),
            Some(6),
            "zero built-in rules: hidden is visible without a filter"
        );

        let default = c.tracker_create(Tracker::new(
            filters(&[Filter::Flags {
                require: 0,
                forbid: flags::HIDDEN,
            }]),
            true,
        ));
        assert_eq!(c.tracker_count(default, None), Some(5));

        let reachable = c.tracker_create(Tracker::new(
            filters(&[
                Filter::Flags {
                    require: 0,
                    forbid: flags::HIDDEN,
                },
                Filter::Reachable {
                    profile: 0,
                    blocked_mask: NPC,
                },
            ]),
            true,
        ));
        assert_eq!(
            c.tracker_count(reachable, None),
            Some(4),
            "chest 30 is behind the NPC wall; the NPCs themselves are adjacent-reachable"
        );
        assert_eq!(c.tracker_count(reachable, Some(3)), Some(1));
        assert!(
            c.tracker_mut(reachable)
                .unwrap()
                .set_filter_enabled(1, false)
        );
        assert_eq!(
            c.tracker_count(reachable, None),
            Some(5),
            "toggled off → plain default view"
        );
        assert!(
            !c.tracker_mut(reachable)
                .unwrap()
                .set_filter_enabled(7, true)
        );

        let nearby = c.tracker_create(Tracker::new(
            filters(&[Filter::Range { max_distance: 1.5 }]),
            true,
        ));
        assert_eq!(
            c.tracker_count(nearby, None),
            Some(2),
            "npc(2,1) at 1.5 and chest 31 at ~1.4"
        );

        let chests_only = c.tracker_create(Tracker::new(
            filters(&[Filter::Categories { mask: 1 << 3 }]),
            false,
        ));
        assert_eq!(
            c.tracker_act(chests_only, Action::Next).unwrap().category,
            3,
            "without ALL, starts in the first available category"
        );
        assert_eq!(c.tracker_count(chests_only, None), Some(3));
    }

    #[test]
    fn distance_order_follows_the_player() {
        let mut c = ctx();
        c.sync_entities(&[chest(1, 0, 1, 0), chest(2, 4, 1, 0), chest(3, 2, 1, 0)]);
        let t = c.tracker_create(Tracker::new(Vec::new(), true));
        c.tracker_mut(t).unwrap().order = Order::Distance;
        c.set_player_position(cell(0, 1));
        let order = |c: &mut Context| {
            c.tracker_mut(t).unwrap().set_category(None);
            (0..3)
                .map(|_| c.tracker_act(t, Action::Next).unwrap().entity_id)
                .collect::<Vec<_>>()
        };
        assert_eq!(order(&mut c), vec![1, 3, 2], "west end: nearest first");
        c.set_player_position(cell(4, 1));
        assert_eq!(order(&mut c), vec![2, 3, 1], "re-sorted after moving east");

        // Equal distances fall back to (category, sort_key, id).
        c.set_player_position(cell(2, 1));
        c.tracker_mut(t).unwrap().set_category(None);
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 3);
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 1);
    }

    #[test]
    fn group_browsing_steps_groups_nearest_first_and_cycles_within() {
        // Along the corridor from the west: mushroom 1 (x0), chest 9 (x1, ungrouped),
        // mushroom 2 (x2), ore 3 (x3), mushroom 4 (x4).
        let mut c = ctx();
        let item = |id: u64, col: u32, group: u16| Entity {
            group,
            ..chest(id, col, 1, 0)
        };
        c.sync_entities(&[
            item(1, 0, 7),
            item(9, 1, 0),
            item(2, 2, 7),
            item(3, 3, 5),
            item(4, 4, 7),
        ]);
        c.set_player_position(cell(0, 1));
        let t = c.tracker_create(Tracker::new(Vec::new(), true));
        c.tracker_mut(t).unwrap().order = Order::Distance;
        let act = |c: &mut Context, a| {
            let s = c.tracker_act(t, a).unwrap();
            (s.entity_id, s.index)
        };

        assert_eq!(
            act(&mut c, Action::InGroupNext),
            (1, 0),
            "nothing selected: first group"
        );
        assert_eq!(c.tracker_group_count(t), Some(3), "three mushrooms");
        assert_eq!(act(&mut c, Action::InGroupNext), (2, 1));
        assert_eq!(act(&mut c, Action::InGroupNext), (4, 2));
        assert_eq!(
            act(&mut c, Action::InGroupNext),
            (1, 0),
            "wraps within the group"
        );
        assert_eq!(act(&mut c, Action::InGroupPrev), (4, 2), "and backwards");

        // Groups by nearest member: mushrooms, the lone chest, ore; each landing on its nearest.
        assert_eq!(act(&mut c, Action::GroupNext), (9, 0));
        assert_eq!(
            c.tracker_group_count(t),
            Some(1),
            "ungrouped: a group of one"
        );
        assert_eq!(act(&mut c, Action::GroupNext), (3, 0));
        assert_eq!(
            act(&mut c, Action::GroupNext),
            (1, 0),
            "wraps to the first group"
        );
        assert_eq!(act(&mut c, Action::GroupPrev), (3, 0));

        // From the east end the mushroom group starts at the nearest one and leads the order.
        c.set_player_position(cell(4, 1));
        c.tracker_mut(t).unwrap().set_category(None);
        assert_eq!(act(&mut c, Action::GroupNext), (4, 0));
        assert_eq!(act(&mut c, Action::InGroupNext), (2, 1));
        assert_eq!(
            act(&mut c, Action::GroupNext),
            (3, 0),
            "ore at x3 is the next nearest group"
        );
        c.tracker_mut(t).unwrap().set_category(None);
        assert_eq!(
            act(&mut c, Action::GroupPrev),
            (9, 0),
            "nothing selected: last group"
        );

        assert_eq!(c.tracker_group_count(99), None);
        c.sync_entities(&[]);
        assert_eq!(act(&mut c, Action::GroupNext), (0, 0), "empty view");
        assert_eq!(c.tracker_group_count(t), Some(0));
    }

    #[test]
    fn set_category_jumps_and_clears_the_selection() {
        let mut c = ctx();
        c.sync_entities(&[chest(30, 1, 0, 0), npc(10, 4, 2)]);
        let t = c.tracker_create(Tracker::new(Vec::new(), true));
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 10);
        c.tracker_mut(t).unwrap().set_category(Some(3));
        assert_eq!(c.tracker_act(t, Action::Current), Some(Selection::none(3)));
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 30);
        c.tracker_mut(t).unwrap().set_category(Some(7));
        assert_eq!(
            c.tracker_act(t, Action::Next),
            Some(Selection::none(7)),
            "empty category"
        );
        c.tracker_mut(t).unwrap().set_category(None);
        assert_eq!(
            c.tracker_act(t, Action::Next).unwrap().category,
            1,
            "back to ALL"
        );
    }

    #[test]
    fn a_context_without_a_map_keeps_entities_and_trackers() {
        let mut c = Context::without_map();
        assert!(c.grid().is_none());
        c.set_player_position(Vec3::new(-10.0, 0.0, -10.0));
        let stats = c.sync_entities(&[
            Entity {
                id: 1,
                category: 0,
                bounds: Bounds::Point(Vec3::new(-10.0, 0.0, -13.0)),
                blocking: 1,
                flags: 0,
                sort_key: 0,
                group: 0,
            },
            Entity {
                id: 2,
                category: 1,
                bounds: Bounds::Box {
                    center: Vec3::new(-5.0, 0.0, -10.0),
                    extents: Vec3::new(1.0, 0.0, 1.0),
                },
                blocking: 0,
                flags: 0,
                sort_key: 0,
                group: 0,
            },
        ]);
        assert_eq!(stats.added, 2);
        assert_eq!(c.entity_nodes(1), Some(&[][..]), "nothing to project into");

        let v = c.entity_vector(1).unwrap();
        assert_eq!((v.distance, v.bearing_deg), (3.0, 0.0), "due north (-Z)");
        assert_eq!(
            c.entity_vector(2).unwrap().distance,
            4.0,
            "nearest face of the box"
        );

        let t = c.tracker_create(Tracker::new(
            filters(&[Filter::Range { max_distance: 3.5 }]),
            true,
        ));
        assert_eq!(c.tracker_count(t, None), Some(1));
        assert_eq!(c.tracker_act(t, Action::Next).unwrap().entity_id, 1);

        let reach = c.tracker_create(Tracker::new(
            filters(&[Filter::Reachable {
                profile: 0,
                blocked_mask: 0,
            }]),
            true,
        ));
        assert_eq!(
            c.tracker_count(reach, None),
            Some(0),
            "no map: nothing is reachable"
        );
        assert_eq!(c.reachable_from_player(0, 0), None);
        assert_eq!(
            c.find_path(Vec3::default(), Vec3::default(), PathOptions::default()),
            None
        );
        assert_eq!(c.groups(0), None);
    }

    #[test]
    fn entity_reachability_counts_adjacent_standing_room() {
        let mut c = ctx();
        c.sync_entities(&[npc(1, 2, 1)]);
        let opts = PathOptions {
            blocked_mask: NPC,
            ..Default::default()
        };
        let reached = c.reachable(cell(0, 1), opts).unwrap();
        assert!(
            !reached[c.grid().unwrap().node_id(2, 1, 0).unwrap() as usize],
            "the NPC's own cell is blocked"
        );
        assert_eq!(
            c.entity_reachable(&reached, 1),
            Some(true),
            "...but you can stand next to it"
        );

        // Wall it in with more NPCs: nothing adjacent is reachable any more.
        c.sync_entities(&[
            npc(1, 2, 1),
            npc(2, 1, 1),
            npc(3, 3, 1),
            npc(4, 2, 0),
            npc(5, 2, 2),
        ]);
        let reached = c.reachable(cell(0, 1), opts).unwrap();
        assert_eq!(c.entity_reachable(&reached, 1), Some(false));
        assert_eq!(c.entity_reachable(&reached, 99), None);
    }
}
