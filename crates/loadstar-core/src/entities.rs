//! Layer 4: the entity registry (spec §5). Numeric records only — labels and any other
//! display state live mod-side, keyed by the same id. The registry never touches the map;
//! the context projects entities into nodes through the map when it needs to.

use std::collections::{HashMap, HashSet};

use crate::geometry::Bounds;

/// Mod-derived unique identifier. Identity, forever; never interpreted by the core.
pub type EntityId = u64;

/// Conventional flag bits. Everything else is mod-defined.
pub mod flags {
    /// Excluded by the default tracker filter. Convention only — the core attaches no meaning.
    pub const HIDDEN: u32 = 1 << 0;
    /// Pathfinding stops next to this entity instead of on it (NPCs vs. exits).
    pub const PATHFIND_ADJACENT: u32 = 1 << 1;
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Entity {
    pub id: EntityId,
    /// Mod-defined; the core only groups by equality.
    pub category: u16,
    pub bounds: Bounds,
    /// Blocking-group bitmask this entity projects onto its nodes. 0 = doesn't block.
    pub blocking: u32,
    pub flags: u32,
    /// Explicit browse order within a category.
    pub sort_key: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Added,
    Updated,
    Unchanged,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub added: u32,
    pub updated: u32,
    pub removed: u32,
}

#[derive(Default)]
pub struct Registry {
    entities: HashMap<EntityId, Entity>,
}

impl Registry {
    pub fn get(&self, id: EntityId) -> Option<&Entity> {
        self.entities.get(&id)
    }

    pub fn len(&self) -> usize {
        self.entities.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = &Entity> {
        self.entities.values()
    }

    /// Insert or replace by id. Reports whether anything actually changed — the diff is a
    /// plain comparison of the numeric record, so unchanged re-sends cost nothing downstream.
    pub fn upsert(&mut self, entity: Entity) -> Change {
        match self.entities.insert(entity.id, entity) {
            None => Change::Added,
            Some(old) if old == entity => Change::Unchanged,
            Some(_) => Change::Updated,
        }
    }

    pub fn remove(&mut self, id: EntityId) -> Option<Entity> {
        self.entities.remove(&id)
    }

    /// Declarative snapshot sync (spec §5): upsert everything in `snapshot`, remove every
    /// entity not in it. Returns the stats plus the ids that changed or were removed, so the
    /// caller can re-project exactly those.
    pub fn sync(&mut self, snapshot: &[Entity]) -> (SyncStats, Vec<EntityId>, Vec<EntityId>) {
        let mut stats = SyncStats::default();
        let mut changed = Vec::new();
        let mut present: HashSet<EntityId> = HashSet::with_capacity(snapshot.len());
        for &e in snapshot {
            present.insert(e.id);
            match self.upsert(e) {
                Change::Added => {
                    stats.added += 1;
                    changed.push(e.id);
                }
                Change::Updated => {
                    stats.updated += 1;
                    changed.push(e.id);
                }
                Change::Unchanged => {}
            }
        }
        let removed: Vec<EntityId> = self
            .entities
            .keys()
            .filter(|id| !present.contains(id))
            .copied()
            .collect();
        for id in &removed {
            self.entities.remove(id);
        }
        stats.removed = removed.len() as u32;
        (stats, changed, removed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::Vec3;

    fn npc(id: u64, col: u32) -> Entity {
        Entity {
            id,
            category: 1,
            bounds: Bounds::TileRect {
                col,
                row: 0,
                width: 1,
                height: 1,
                layer: Some(0),
            },
            blocking: 1,
            flags: flags::PATHFIND_ADJACENT,
            sort_key: 0,
        }
    }

    #[test]
    fn upsert_reports_real_changes_only() {
        let mut r = Registry::default();
        assert_eq!(r.upsert(npc(1, 0)), Change::Added);
        assert_eq!(r.upsert(npc(1, 0)), Change::Unchanged);
        assert_eq!(r.upsert(npc(1, 3)), Change::Updated);
        assert_eq!(r.get(1).unwrap().bounds.center(), Vec3::new(3.5, 0.0, 0.5));
        assert!(r.remove(1).is_some());
        assert!(r.remove(1).is_none());
    }

    #[test]
    fn sync_diffs_against_the_store() {
        let mut r = Registry::default();
        let (s, changed, removed) = r.sync(&[npc(1, 0), npc(2, 0)]);
        assert_eq!(
            s,
            SyncStats {
                added: 2,
                updated: 0,
                removed: 0
            }
        );
        assert_eq!((changed.len(), removed.len()), (2, 0));

        // Entity 1 unchanged, 2 moved, 3 new, and 1 then absent in a later frame.
        let (s, mut changed, removed) = r.sync(&[npc(1, 0), npc(2, 5), npc(3, 9)]);
        changed.sort_unstable();
        assert_eq!(
            s,
            SyncStats {
                added: 1,
                updated: 1,
                removed: 0
            }
        );
        assert_eq!((changed, removed), (vec![2, 3], vec![]));

        let (s, _, removed) = r.sync(&[npc(2, 5), npc(3, 9)]);
        assert_eq!(
            s,
            SyncStats {
                added: 0,
                updated: 0,
                removed: 1
            }
        );
        assert_eq!(removed, vec![1]);
        assert_eq!(r.len(), 2);

        let (s, ..) = r.sync(&[]);
        assert_eq!(s.removed, 2);
        assert!(r.is_empty());
    }
}
