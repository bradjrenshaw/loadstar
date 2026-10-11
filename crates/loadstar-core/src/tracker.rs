//! Layer 5: trackers (spec §8). A tracker is a selection state machine over the entity
//! registry: it cycles categories and entities, filtered by a declarative, AND-combined
//! filter list. It computes no spatial data and presumes no announcement policy — the core
//! has **zero built-in visibility rules**; visibility is exactly the filter list.
//!
//! The data types live here; evaluation lives on the context, which owns the registry,
//! the player position and the reachability cache the filters consult.

use crate::entities::EntityId;

/// Category value meaning "every category" in results and counts.
pub const ALL: u32 = u32::MAX;

/// A visibility rule. Filters are data the core evaluates — never callbacks. `Flags` is the
/// escape hatch for game-specific predicates: the mod computes any condition, stamps it as
/// an entity flag bit during sync, and selects on it here.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Filter {
    /// Entity (or a node next to it) is reachable from the player under this profile/mask.
    Reachable { profile: u32, blocked_mask: u32 },
    /// Nearest point of the entity is within this distance of the player.
    Range { max_distance: f32 },
    /// Category must be one of the (at most 64) categories set in the mask.
    Categories { mask: u64 },
    /// All `require` bits set and no `forbid` bit set.
    Flags { require: u32, forbid: u32 },
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FilterEntry {
    pub filter: Filter,
    pub enabled: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Next,
    Prev,
    CategoryNext,
    CategoryPrev,
    /// Re-report the current selection without moving.
    Current,
}

/// What a tracker action produced. `entity_id == 0` means nothing is selected; `category`
/// is then the tracker's current category (or [`ALL`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selection {
    pub entity_id: EntityId,
    pub category: u32,
    /// Position of the selection in cycle order.
    pub index: u32,
}

impl Selection {
    pub const fn none(category: u32) -> Selection {
        Selection {
            entity_id: 0,
            category,
            index: 0,
        }
    }
}

#[derive(Clone, Debug)]
pub struct Tracker {
    pub filters: Vec<FilterEntry>,
    /// Whether an "all categories" view precedes the per-category views.
    pub include_all: bool,
    /// `None` = the ALL view (or, without `include_all`, "not yet chosen").
    pub(crate) category: Option<u16>,
    pub(crate) selected: Option<EntityId>,
}

impl Tracker {
    pub fn new(filters: Vec<FilterEntry>, include_all: bool) -> Tracker {
        Tracker {
            filters,
            include_all,
            category: None,
            selected: None,
        }
    }

    pub fn set_filter_enabled(&mut self, index: usize, enabled: bool) -> bool {
        match self.filters.get_mut(index) {
            Some(f) => {
                f.enabled = enabled;
                true
            }
            None => false,
        }
    }

    /// Replace the filter list. Selection is kept; it is re-validated on the next action.
    pub fn set_filters(&mut self, filters: Vec<FilterEntry>) {
        self.filters = filters;
    }

    pub fn current_category(&self) -> u32 {
        self.category.map_or(ALL, u32::from)
    }
}
