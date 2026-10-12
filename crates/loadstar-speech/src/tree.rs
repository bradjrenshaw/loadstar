//! The voice tree (speech-spec §2): categories (an open, ranked set keyed by string) containing
//! leaves (stable ids, features), with a root auto and one auto per category. Built by the worker
//! from what the backends probe, published as an immutable snapshot the game thread resolves
//! against without probing anything.

use std::collections::HashMap;

use crate::backend::LeafSpec;
use crate::features;
use crate::ids;

/// Built-in categories and their ranks. Built-in keys are fixed so a mod can map them to its
/// own locale; the ranks are spaced so a later or third-party category slots between them.
pub struct CategoryDef {
    pub key: &'static str,
    pub name: &'static str,
    pub rank: i32,
}

pub const SCREEN_READERS: &str = "screen_readers";
pub const SAPI5: &str = "sapi5";
pub const ONECORE: &str = "onecore";
pub const SAPI4: &str = "sapi4";
pub const AVSPEECH: &str = "avspeech";
pub const SPEECH_DISPATCHER: &str = "speech_dispatcher";
pub const CLIPBOARD: &str = "clipboard";

pub const BUILTIN: &[CategoryDef] = &[
    CategoryDef {
        key: SCREEN_READERS,
        name: "Screen readers",
        rank: 0,
    },
    CategoryDef {
        key: SAPI5,
        name: "SAPI 5",
        rank: 100,
    },
    CategoryDef {
        key: ONECORE,
        name: "OneCore",
        rank: 200,
    },
    CategoryDef {
        key: SAPI4,
        name: "SAPI 4",
        rank: 300,
    },
    CategoryDef {
        key: AVSPEECH,
        name: "AVSpeech",
        rank: 400,
    },
    CategoryDef {
        key: SPEECH_DISPATCHER,
        name: "Speech Dispatcher",
        rank: 500,
    },
    CategoryDef {
        key: CLIPBOARD,
        name: "Clipboard",
        rank: 1000,
    },
];

#[derive(Clone, Debug, PartialEq)]
pub struct Category {
    pub id: u64,
    pub key: String,
    pub name: String,
    pub rank: i32,
    /// The voice id that means "this category's best".
    pub auto: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Leaf {
    pub id: u64,
    pub name: String,
    pub language: String,
    pub category: u64,
    pub features: u64,
    /// Index into the worker's backend list.
    pub backend: usize,
    pub selector: u64,
    pub is_default: bool,
    pub available: bool,
}

/// One immutable snapshot of everything that can speak.
#[derive(Clone, Debug, Default)]
pub struct Tree {
    /// 0 = never probed.
    pub generation: u64,
    /// Sorted by rank.
    pub categories: Vec<Category>,
    /// Grouped by category in `categories` order, then probe order.
    pub leaves: Vec<Leaf>,
    by_id: HashMap<u64, usize>,
    cat_by_id: HashMap<u64, usize>,
}

/// What a profile resolved to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolution {
    /// Index into `leaves`, or None when nothing at all can speak (or `no_fallback` stopped the walk).
    pub leaf: Option<usize>,
    /// The chosen voice was absent and a fallback was used.
    pub fell_back: bool,
}

impl Tree {
    /// Build from probed specs in backend-priority order; the first spec per dedupe key wins.
    pub fn build(generation: u64, specs: Vec<(usize, &'static str, LeafSpec)>) -> Tree {
        let mut categories: Vec<Category> = BUILTIN
            .iter()
            .map(|d| Category {
                id: ids::category_id(d.key),
                key: d.key.to_string(),
                name: d.name.to_string(),
                rank: d.rank,
                auto: ids::category_auto(d.key),
            })
            .collect();
        let mut seen = std::collections::HashSet::new();
        let mut pending: Vec<Leaf> = Vec::new();
        for (backend, backend_name, spec) in specs {
            let dedupe = spec
                .dedupe
                .clone()
                .unwrap_or_else(|| format!("{backend_name}:{}", spec.native_id));
            if !seen.insert(dedupe) {
                continue;
            }
            let cat_id = ids::category_id(&spec.category);
            if !categories.iter().any(|c| c.id == cat_id) {
                categories.push(Category {
                    id: cat_id,
                    key: spec.category.clone(),
                    name: spec
                        .category_name
                        .clone()
                        .unwrap_or_else(|| spec.category.clone()),
                    rank: spec.category_rank.unwrap_or(900),
                    auto: ids::category_auto(&spec.category),
                });
            }
            pending.push(Leaf {
                id: ids::voice_id(backend_name, &spec.native_id),
                name: spec.name,
                language: spec.language,
                category: cat_id,
                features: spec.features,
                backend,
                selector: spec.selector,
                is_default: spec.is_default,
                available: spec.available,
            });
        }
        categories.sort_by_key(|c| c.rank);
        // Only categories with leaves are published; the built-in table is a vocabulary, not a promise.
        categories.retain(|c| pending.iter().any(|l| l.category == c.id));
        let mut leaves = Vec::with_capacity(pending.len());
        for c in &categories {
            leaves.extend(pending.iter().filter(|l| l.category == c.id).cloned());
        }
        let by_id = leaves.iter().enumerate().map(|(i, l)| (l.id, i)).collect();
        let cat_by_id = categories
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect();
        Tree {
            generation,
            categories,
            leaves,
            by_id,
            cat_by_id,
        }
    }

    pub fn category(&self, id: u64) -> Option<&Category> {
        self.cat_by_id.get(&id).map(|&i| &self.categories[i])
    }

    pub fn leaf(&self, id: u64) -> Option<&Leaf> {
        self.by_id.get(&id).map(|&i| &self.leaves[i])
    }

    pub fn leaf_index(&self, id: u64) -> Option<usize> {
        self.by_id.get(&id).copied()
    }

    /// The voices a UI should list under a category: the AVAILABLE ones (a reader that is not
    /// running is not offered). An absent leaf is still addressable by id (`leaf`), so a saved pick
    /// keeps its name and the library's fallback decides what speaks.
    pub fn leaves_in(&self, category: u64) -> impl Iterator<Item = &Leaf> + '_ {
        self.leaves
            .iter()
            .filter(move |l| l.category == category && l.available)
    }

    /// Every leaf in a category, available or not.
    pub fn all_leaves_in(&self, category: u64) -> impl Iterator<Item = &Leaf> + '_ {
        self.leaves.iter().filter(move |l| l.category == category)
    }

    /// The category whose auto id this is.
    pub fn category_of_auto(&self, voice: u64) -> Option<&Category> {
        self.categories.iter().find(|c| c.auto == voice)
    }

    /// A category's best available leaf: the one its backend marks default, else the first.
    pub fn category_auto_leaf(&self, category: u64) -> Option<usize> {
        let mut first = None;
        for (i, l) in self.leaves.iter().enumerate() {
            if l.category != category || !l.available {
                continue;
            }
            if l.is_default {
                return Some(i);
            }
            first.get_or_insert(i);
        }
        first
    }

    /// Root auto: the first category (by rank) with anything available — screen readers first,
    /// so a running reader always wins.
    pub fn root_auto_leaf(&self) -> Option<usize> {
        self.categories
            .iter()
            .find_map(|c| self.category_auto_leaf(c.id))
    }

    /// The leaf a voice id names right now, if present and available.
    fn pick(&self, voice: u64) -> Option<usize> {
        if voice == ids::ROOT_AUTO {
            return self.root_auto_leaf();
        }
        if let Some(c) = self.category_of_auto(voice) {
            return self.category_auto_leaf(c.id);
        }
        self.leaf_index(voice).filter(|&i| self.leaves[i].available)
    }

    /// Resolve for absence only (speech-spec §2, "Resolution and fallback"): the chosen leaf,
    /// else its category's auto, else the root auto, else clipboard. Features never enter here.
    pub fn resolve(&self, voice: u64, no_fallback: bool) -> Resolution {
        if let Some(i) = self.pick(voice) {
            return Resolution {
                leaf: Some(i),
                fell_back: false,
            };
        }
        if no_fallback {
            return Resolution {
                leaf: None,
                fell_back: false,
            };
        }
        let category = self
            .leaf(voice)
            .map(|l| l.category)
            .or_else(|| self.category_of_auto(voice).map(|c| c.id));
        let leaf = category
            .and_then(|c| self.category_auto_leaf(c))
            .or_else(|| self.root_auto_leaf())
            .or_else(|| self.category_auto_leaf(ids::category_id(CLIPBOARD)));
        Resolution {
            leaf,
            fell_back: leaf.is_some(),
        }
    }

    /// For `RENDER_ELSEWHERE`: the first category (by rank) whose auto leaf can render.
    pub fn render_elsewhere(&self) -> Option<usize> {
        self.categories
            .iter()
            .filter_map(|c| self.category_auto_leaf(c.id))
            .find(|&i| self.leaves[i].features & features::RENDER != 0)
    }

    /// Same categories and leaves (ignoring generation)? Drives the `changed` flag.
    pub fn same_shape(&self, other: &Tree) -> bool {
        self.categories == other.categories && self.leaves == other.leaves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(
        category: &str,
        name: &str,
        features: u64,
        available: bool,
        is_default: bool,
    ) -> LeafSpec {
        LeafSpec {
            category: category.into(),
            category_name: None,
            category_rank: None,
            name: name.into(),
            language: String::new(),
            native_id: name.to_lowercase(),
            dedupe: if category == SCREEN_READERS {
                Some(format!("reader:{}", name.to_lowercase()))
            } else {
                None
            },
            features,
            selector: 1,
            is_default,
            available,
        }
    }

    fn tree(reader_running: bool) -> Tree {
        Tree::build(
            1,
            vec![
                (
                    0,
                    "nvda",
                    spec(
                        SCREEN_READERS,
                        "NVDA",
                        features::SPEAK | features::SCREEN_READER,
                        reader_running,
                        false,
                    ),
                ),
                (
                    1,
                    "prism",
                    spec(
                        SCREEN_READERS,
                        "NVDA",
                        features::SPEAK | features::SCREEN_READER | features::BRAILLE,
                        reader_running,
                        false,
                    ),
                ),
                (
                    1,
                    "prism",
                    spec(
                        SCREEN_READERS,
                        "JAWS",
                        features::SPEAK | features::SCREEN_READER,
                        false,
                        false,
                    ),
                ),
                (
                    2,
                    "sapi",
                    spec(
                        SAPI5,
                        "Zira",
                        features::SPEAK | features::RENDER | features::SET_RATE,
                        true,
                        false,
                    ),
                ),
                (
                    2,
                    "sapi",
                    spec(
                        SAPI5,
                        "David",
                        features::SPEAK | features::RENDER | features::SET_RATE,
                        true,
                        true,
                    ),
                ),
                (
                    3,
                    "clipboard",
                    spec(CLIPBOARD, "Clipboard", features::SPEAK, true, true),
                ),
            ],
        )
    }

    #[test]
    fn categories_are_ranked_and_leaves_deduped_native_first() {
        let t = tree(true);
        let keys: Vec<&str> = t.categories.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, vec![SCREEN_READERS, SAPI5, CLIPBOARD]);
        let readers: Vec<(&str, usize)> = t
            .leaves_in(ids::category_id(SCREEN_READERS))
            .map(|l| (l.name.as_str(), l.backend))
            .collect();
        assert_eq!(
            readers,
            vec![("NVDA", 0)],
            "native NVDA wins over Prism's; JAWS is absent so it is not listed"
        );
        let all: Vec<&str> = t
            .all_leaves_in(ids::category_id(SCREEN_READERS))
            .map(|l| l.name.as_str())
            .collect();
        assert_eq!(all, vec!["NVDA", "JAWS"]);
        let jaws = t
            .leaf(ids::voice_id("prism", "jaws"))
            .expect("an absent leaf stays addressable by id");
        assert!(!jaws.available);
    }

    #[test]
    fn autos_prefer_a_running_reader_then_the_default_synth_voice() {
        let t = tree(true);
        assert_eq!(t.leaves[t.root_auto_leaf().unwrap()].name, "NVDA");
        let t = tree(false);
        assert_eq!(
            t.leaves[t.root_auto_leaf().unwrap()].name,
            "David",
            "no reader running: SAPI default voice"
        );
        assert_eq!(
            t.leaves[t.category_auto_leaf(ids::category_id(SAPI5)).unwrap()].name,
            "David"
        );
    }

    #[test]
    fn fallback_walks_up_for_absence_only() {
        let t = tree(false);
        let nvda = t.leaf(ids::voice_id("nvda", "nvda")).unwrap().id;
        let r = t.resolve(nvda, false);
        assert!(r.fell_back);
        assert_eq!(
            t.leaves[r.leaf.unwrap()].name,
            "David",
            "reader absent → its category has nothing → root auto"
        );
        assert_eq!(
            t.resolve(nvda, true),
            Resolution {
                leaf: None,
                fell_back: false
            },
            "NO_FALLBACK stops the walk"
        );
        let zira = t.leaf(ids::voice_id("sapi", "zira")).unwrap();
        let r = t.resolve(zira.id, false);
        assert_eq!(
            (t.leaves[r.leaf.unwrap()].name.as_str(), r.fell_back),
            ("Zira", false)
        );
        assert_eq!(
            t.leaves[t.resolve(9999, false).leaf.unwrap()].name,
            "David",
            "unknown id → root auto"
        );
        assert_eq!(t.leaves[t.render_elsewhere().unwrap()].name, "David");
    }

    #[test]
    fn a_tree_with_nothing_resolves_to_none() {
        let t = Tree::default();
        assert_eq!(t.resolve(0, false).leaf, None);
    }
}
