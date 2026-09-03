//! Layer 2: graph algorithms over abstract node IDs.
//!
//! Nothing here knows what a node *is* — a tile, a navmesh polygon, a voxel.
//! Map types (layer 3) implement [`Graph`]; everything above works in node sets.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

/// Dense index into a map's node space. Meaning is defined by the map type.
pub type NodeId = u32;

pub trait Graph {
    /// Total number of nodes; every `NodeId` is `< node_count()`.
    fn node_count(&self) -> usize;

    /// Push `(neighbor, edge_cost)` pairs for `node` into `out`. The caller clears `out`.
    fn neighbors(&self, node: NodeId, out: &mut Vec<(NodeId, f32)>);

    /// Admissible (never over-estimating) cost estimate from `a` to `b`.
    fn heuristic(&self, a: NodeId, b: NodeId) -> f32;
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

/// A* from `start` to `goal`. Returns the node sequence including both endpoints,
/// or `None` if unreachable. A path from a node to itself is `[start]`.
pub fn astar(graph: &impl Graph, start: NodeId, goal: NodeId) -> Option<Vec<NodeId>> {
    let n = graph.node_count();
    if (start as usize) >= n || (goal as usize) >= n {
        return None;
    }
    if start == goal {
        return Some(vec![start]);
    }

    let mut g_score = vec![f32::INFINITY; n];
    let mut came_from = vec![NodeId::MAX; n];
    let mut closed = vec![false; n];
    let mut open = BinaryHeap::new();
    let mut scratch: Vec<(NodeId, f32)> = Vec::with_capacity(8);

    g_score[start as usize] = 0.0;
    open.push(Open { f: graph.heuristic(start, goal), node: start });

    while let Some(Open { node, .. }) = open.pop() {
        if node == goal {
            return Some(reconstruct(&came_from, goal));
        }
        if closed[node as usize] {
            continue; // stale heap entry
        }
        closed[node as usize] = true;

        scratch.clear();
        graph.neighbors(node, &mut scratch);
        for &(next, cost) in &scratch {
            if closed[next as usize] {
                continue;
            }
            let tentative = g_score[node as usize] + cost;
            if tentative < g_score[next as usize] {
                g_score[next as usize] = tentative;
                came_from[next as usize] = node;
                open.push(Open { f: tentative + graph.heuristic(next, goal), node: next });
            }
        }
    }
    None
}

fn reconstruct(came_from: &[NodeId], goal: NodeId) -> Vec<NodeId> {
    let mut path = vec![goal];
    let mut current = goal;
    while came_from[current as usize] != NodeId::MAX {
        current = came_from[current as usize];
        path.push(current);
    }
    path.reverse();
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A tiny explicit graph: 0 -> 1 -> 2 -> 3, plus a costly shortcut 0 -> 3.
    struct Line;
    impl Graph for Line {
        fn node_count(&self) -> usize {
            4
        }
        fn neighbors(&self, node: NodeId, out: &mut Vec<(NodeId, f32)>) {
            match node {
                0 => {
                    out.push((1, 1.0));
                    out.push((3, 10.0));
                }
                1 => out.push((2, 1.0)),
                2 => out.push((3, 1.0)),
                _ => {}
            }
        }
        fn heuristic(&self, _: NodeId, _: NodeId) -> f32 {
            0.0
        }
    }

    #[test]
    fn prefers_cheaper_longer_route_over_expensive_shortcut() {
        assert_eq!(astar(&Line, 0, 3), Some(vec![0, 1, 2, 3]));
    }

    #[test]
    fn unreachable_and_degenerate_cases() {
        assert_eq!(astar(&Line, 3, 0), None); // edges are directed
        assert_eq!(astar(&Line, 2, 2), Some(vec![2]));
        assert_eq!(astar(&Line, 0, 99), None);
    }
}
