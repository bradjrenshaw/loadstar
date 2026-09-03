//! Layer 2: graph algorithms over abstract node IDs.
//!
//! Nothing here knows what a node *is* — a tile, a navmesh polygon, a voxel.
//! Map types (layer 3) implement [`Graph`]; everything above works in node sets.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Dense index into a map's node space. Meaning is defined by the map type.
pub type NodeId = u32;

/// Mod-assigned identifier for a link (spec §6.3). Never interpreted by the core.
pub type LinkId = u64;

/// One traversable connection out of a node.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Edge {
    pub to: NodeId,
    pub cost: f32,
    /// `Some` if this edge is a link rather than ordinary adjacency. Recorded in the path
    /// so guidance can say "take the teleporter" instead of compressing across it.
    pub link: Option<LinkId>,
}

impl Edge {
    pub const fn step(to: NodeId, cost: f32) -> Edge {
        Edge { to, cost, link: None }
    }

    pub const fn link(to: NodeId, cost: f32, id: LinkId) -> Edge {
        Edge { to, cost, link: Some(id) }
    }
}

pub trait Graph {
    /// Total number of nodes; every `NodeId` is `< node_count()`.
    fn node_count(&self) -> usize;

    /// Push the edges leaving `node` into `out`. The caller clears `out`.
    fn neighbors(&self, node: NodeId, out: &mut Vec<Edge>);

    /// Admissible (never over-estimating) cost estimate from `a` to `b`.
    fn heuristic(&self, a: NodeId, b: NodeId) -> f32;
}

/// A path result (spec §8): node sequence plus which arrivals happened via a link.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Path {
    /// Includes both endpoints. A path from a node to itself is `[node]`.
    pub nodes: Vec<NodeId>,
    /// `(index into nodes, link id)` for every node that was reached by taking a link.
    pub links: Vec<(usize, LinkId)>,
    /// Total traversal cost.
    pub cost: f32,
}

/// Min-heap entry ordered by f-score. Ties broken by node id for determinism.
#[derive(Clone, Copy, PartialEq)]
struct Open {
    f: f32,
    node: NodeId,
}

impl Eq for Open {}

impl Ord for Open {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .f
            .partial_cmp(&self.f)
            .unwrap_or(Ordering::Equal)
            .then_with(|| other.node.cmp(&self.node))
    }
}

impl PartialOrd for Open {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// A search request. Several goals may be given (goal candidates, spec §8); the cheapest
/// reachable one wins. `is_blocked` nodes are never entered — the start is exempt, since
/// the agent is already standing there.
pub struct Query<'a> {
    pub start: NodeId,
    pub goals: &'a [NodeId],
    pub is_blocked: &'a dyn Fn(NodeId) -> bool,
}

/// A* from `start` to `goal` with no blocking. `None` if unreachable or an id is out of range.
pub fn astar(graph: &impl Graph, start: NodeId, goal: NodeId) -> Option<Path> {
    astar_query(graph, &Query { start, goals: &[goal], is_blocked: &|_| false })
}

/// Multi-goal A* with blocking. Ties between equally cheap goals resolve deterministically.
pub fn astar_query(graph: &impl Graph, query: &Query) -> Option<Path> {
    let n = graph.node_count();
    let start = query.start;
    if (start as usize) >= n || query.goals.is_empty() || query.goals.iter().any(|&g| (g as usize) >= n) {
        return None;
    }
    let mut is_goal = vec![false; n];
    for &g in query.goals {
        is_goal[g as usize] = true;
    }
    if is_goal[start as usize] {
        return Some(Path { nodes: vec![start], links: Vec::new(), cost: 0.0 });
    }
    let h = |node: NodeId| query.goals.iter().map(|&g| graph.heuristic(node, g)).fold(f32::INFINITY, f32::min);

    let mut g_score = vec![f32::INFINITY; n];
    let mut came_from = vec![NodeId::MAX; n];
    let mut came_via: Vec<Option<LinkId>> = vec![None; n];
    let mut closed = vec![false; n];
    let mut open = BinaryHeap::new();
    let mut scratch: Vec<Edge> = Vec::with_capacity(8);

    g_score[start as usize] = 0.0;
    open.push(Open { f: h(start), node: start });

    while let Some(Open { node, .. }) = open.pop() {
        if is_goal[node as usize] {
            return Some(reconstruct(&came_from, &came_via, node, g_score[node as usize]));
        }
        if closed[node as usize] {
            continue; // stale heap entry
        }
        closed[node as usize] = true;

        scratch.clear();
        graph.neighbors(node, &mut scratch);
        for &Edge { to, cost, link } in &scratch {
            if (to as usize) >= n || closed[to as usize] || (query.is_blocked)(to) {
                continue;
            }
            let tentative = g_score[node as usize] + cost;
            if tentative < g_score[to as usize] {
                g_score[to as usize] = tentative;
                came_from[to as usize] = node;
                came_via[to as usize] = link;
                open.push(Open { f: tentative + h(to), node: to });
            }
        }
    }
    None
}

/// Every node reachable from `start` without entering a blocked node (the start itself is
/// always reachable). Indexed by `NodeId`. Backs the reachability filter (spec §8).
pub fn flood_fill(graph: &impl Graph, start: NodeId, is_blocked: &dyn Fn(NodeId) -> bool) -> Vec<bool> {
    let n = graph.node_count();
    let mut reached = vec![false; n];
    if (start as usize) >= n {
        return reached;
    }
    reached[start as usize] = true;
    let mut stack = vec![start];
    let mut scratch: Vec<Edge> = Vec::with_capacity(8);
    while let Some(node) = stack.pop() {
        scratch.clear();
        graph.neighbors(node, &mut scratch);
        for &Edge { to, .. } in &scratch {
            if (to as usize) < n && !reached[to as usize] && !is_blocked(to) {
                reached[to as usize] = true;
                stack.push(to);
            }
        }
    }
    reached
}

fn reconstruct(came_from: &[NodeId], came_via: &[Option<LinkId>], goal: NodeId, cost: f32) -> Path {
    let mut nodes = vec![goal];
    let mut via = vec![came_via[goal as usize]];
    let mut current = goal;
    while came_from[current as usize] != NodeId::MAX {
        current = came_from[current as usize];
        nodes.push(current);
        via.push(came_via[current as usize]);
    }
    nodes.reverse();
    via.reverse();
    let links = via.iter().enumerate().filter_map(|(i, v)| v.map(|id| (i, id))).collect();
    Path { nodes, links, cost }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0 -> 1 -> 2 -> 3 at unit cost, an expensive shortcut 0 -> 3, and a link 1 -> 3.
    struct Line {
        link_cost: f32,
    }
    impl Graph for Line {
        fn node_count(&self) -> usize {
            4
        }
        fn neighbors(&self, node: NodeId, out: &mut Vec<Edge>) {
            match node {
                0 => {
                    out.push(Edge::step(1, 1.0));
                    out.push(Edge::step(3, 10.0));
                }
                1 => {
                    out.push(Edge::step(2, 1.0));
                    out.push(Edge::link(3, self.link_cost, 77));
                }
                2 => out.push(Edge::step(3, 1.0)),
                _ => {}
            }
        }
        fn heuristic(&self, _: NodeId, _: NodeId) -> f32 {
            0.0
        }
    }

    #[test]
    fn prefers_cheaper_route_and_ignores_expensive_link() {
        let p = astar(&Line { link_cost: 5.0 }, 0, 3).unwrap();
        assert_eq!(p.nodes, vec![0, 1, 2, 3]);
        assert!(p.links.is_empty());
        assert_eq!(p.cost, 3.0);
    }

    #[test]
    fn cheap_link_is_taken_and_recorded() {
        let p = astar(&Line { link_cost: 0.5 }, 0, 3).unwrap();
        assert_eq!(p.nodes, vec![0, 1, 3]);
        assert_eq!(p.links, vec![(2, 77)]);
        assert_eq!(p.cost, 1.5);
    }

    #[test]
    fn unreachable_and_degenerate_cases() {
        let g = Line { link_cost: 1.0 };
        assert_eq!(astar(&g, 3, 0), None); // edges are directed
        assert_eq!(astar(&g, 2, 2).unwrap().nodes, vec![2]);
        assert_eq!(astar(&g, 0, 99), None);
    }

    #[test]
    fn blocked_nodes_are_never_entered_but_start_is_exempt() {
        let g = Line { link_cost: 5.0 };
        let blocked = |n: NodeId| n == 1;
        // 1 is blocked, so the only way to 3 is the expensive direct edge.
        let p = astar_query(&g, &Query { start: 0, goals: &[3], is_blocked: &blocked }).unwrap();
        assert_eq!(p.nodes, vec![0, 3]);
        assert_eq!(p.cost, 10.0);
        // Starting *on* the blocked node is fine — you're already there.
        let p = astar_query(&g, &Query { start: 1, goals: &[2], is_blocked: &blocked }).unwrap();
        assert_eq!(p.nodes, vec![1, 2]);
    }

    #[test]
    fn multi_goal_picks_the_cheapest_candidate() {
        let g = Line { link_cost: 5.0 };
        let p = astar_query(&g, &Query { start: 0, goals: &[3, 2], is_blocked: &|_| false }).unwrap();
        assert_eq!(p.nodes, vec![0, 1, 2]);
        assert_eq!(astar_query(&g, &Query { start: 0, goals: &[], is_blocked: &|_| false }), None);
    }

    #[test]
    fn flood_fill_respects_blocking() {
        let g = Line { link_cost: 5.0 };
        assert_eq!(flood_fill(&g, 0, &|_| false), vec![true, true, true, true]);
        assert_eq!(flood_fill(&g, 0, &|n| n == 1), vec![true, false, false, true]);
        assert_eq!(flood_fill(&g, 3, &|_| false), vec![false, false, false, true]);
    }
}
