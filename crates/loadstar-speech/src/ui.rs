//! The menu tree (speech-spec §2, "The menu tree"): the voice tree arranged for a "pick a voice"
//! menu. Generic nodes — a mod walks children and shows labels without knowing what a level
//! means — so the arrangement can change here without an ABI change or a change to any mod.
//!
//! Built from a [`Tree`] snapshot, and only from its AVAILABLE leaves:
//!
//! ```text
//! root
//! ├─ Auto                              the root auto (voice 0)
//! ├─ Screen readers                    a category: its auto, then its voices by name
//! │  ├─ Auto
//! │  └─ NVDA, JAWS, …
//! ├─ AVSpeech                          a category whose voices have languages
//! │  ├─ Auto
//! │  ├─ English (United States)        one node per language tag, preferred language first
//! │  │  ├─ Auto                        that language's best voice
//! │  │  ├─ Samantha, …                 regular voices by name
//! │  │  └─ Novelty                     novelty voices last, in a group of their own
//! │  │     ├─ Auto
//! │  │     └─ Bells, …
//! │  └─ …
//! └─ Clipboard                         a single-voice category is that voice
//! ```
//!
//! Every group opens with an Auto whose voice id resolves to the group's best voice, so "any
//! English (United States) voice" can be saved in a profile like a voice can.

use std::collections::HashMap;

use crate::ids;
use crate::tree::{Category, Leaf, Tree};

#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeKind {
    Root = 0,
    /// "This group's best voice". `voice` is the auto's id (0 for the root auto).
    Auto = 1,
    /// A category. `key` is its fixed key (what a mod localizes); `label` its English name.
    Category = 2,
    /// One language's voices. `key` is the BCP-47 tag; `label` the language's display name.
    Language = 3,
    /// The novelty voices of the enclosing group. `key` is `novelty`.
    Novelty = 4,
    /// A voice. `voice` is its id; `key` its language tag (may be empty).
    Voice = 5,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Node {
    /// Stable across refreshes and sessions, never 0.
    pub id: u64,
    pub kind: NodeKind,
    /// Auto and Voice: the id to put in a profile. Otherwise 0.
    pub voice: u64,
    /// The category this node is, or sits under; 0 for the root and the root auto.
    pub category: u64,
    pub key: String,
    pub label: String,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
}

/// One arrangement of a [`Tree`] snapshot for a preferred language.
#[derive(Clone, Debug, Default)]
pub struct UiTree {
    /// The snapshot this was built from.
    pub generation: u64,
    /// The preferred language it was built for (as given).
    pub language: String,
    /// `nodes[0]` is the root.
    pub nodes: Vec<Node>,
    by_id: HashMap<u64, usize>,
    by_voice: HashMap<u64, usize>,
}

/// `en_US` / `EN-us` → `en-us`.
fn normalize(tag: &str) -> String {
    tag.trim().replace('_', "-").to_lowercase()
}

fn primary(tag: &str) -> &str {
    tag.split('-').next().unwrap_or("")
}

impl UiTree {
    /// Arrange `tree`. `preferred` is the language whose voices should come first (BCP-47, any
    /// case, `_` accepted; empty for none); `language_name` turns a voice's language tag into
    /// what a menu shows for it.
    pub fn build(tree: &Tree, preferred: &str, language_name: &dyn Fn(&str) -> String) -> UiTree {
        let mut b = Builder {
            nodes: Vec::new(),
            preferred: normalize(preferred),
            language_name,
        };
        let root = b.push(None, ids::hash("node:root"), NodeKind::Root, 0, 0, "", "");
        if tree.root_auto_leaf().is_some() {
            b.auto(root, ids::ROOT_AUTO, 0);
        }
        for c in &tree.categories {
            let leaves: Vec<&Leaf> = tree.leaves_in(c.id).collect();
            match leaves.as_slice() {
                [] => {}
                [only] if c.single => {
                    b.voice(root, only);
                }
                _ => b.category(root, c, &leaves),
            }
        }
        let by_id = b.nodes.iter().enumerate().map(|(i, n)| (n.id, i)).collect();
        let by_voice = b
            .nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Auto | NodeKind::Voice))
            .map(|(i, n)| (n.voice, i))
            .collect();
        UiTree {
            generation: tree.generation,
            language: preferred.to_string(),
            nodes: b.nodes,
            by_id,
            by_voice,
        }
    }

    pub fn root(&self) -> Option<&Node> {
        self.nodes.first()
    }

    pub fn node(&self, id: u64) -> Option<&Node> {
        self.by_id.get(&id).map(|&i| &self.nodes[i])
    }

    /// The Voice or Auto node that stands for a voice id; None when the tree does not list it
    /// (an absent voice, an id from another machine).
    pub fn find_voice(&self, voice: u64) -> Option<&Node> {
        self.by_voice.get(&voice).map(|&i| &self.nodes[i])
    }
}

struct Builder<'a> {
    nodes: Vec<Node>,
    preferred: String,
    language_name: &'a dyn Fn(&str) -> String,
}

impl Builder<'_> {
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        parent: Option<usize>,
        id: u64,
        kind: NodeKind,
        voice: u64,
        category: u64,
        key: &str,
        label: &str,
    ) -> usize {
        let i = self.nodes.len();
        self.nodes.push(Node {
            id,
            kind,
            voice,
            category,
            key: key.to_string(),
            label: label.to_string(),
            parent,
            children: Vec::new(),
        });
        if let Some(p) = parent {
            self.nodes[p].children.push(i);
        }
        i
    }

    /// The Auto that opens a group.
    fn auto(&mut self, parent: usize, voice: u64, category: u64) {
        let id = ids::hash(&format!("node:auto:{:016x}", self.nodes[parent].id));
        self.push(
            Some(parent),
            id,
            NodeKind::Auto,
            voice,
            category,
            "auto",
            "Auto",
        );
    }

    fn voice(&mut self, parent: usize, leaf: &Leaf) {
        self.push(
            Some(parent),
            leaf.id,
            NodeKind::Voice,
            leaf.id,
            leaf.category,
            &leaf.language,
            &leaf.name,
        );
    }

    /// Regular voices by name, then the novelty ones in a group of their own.
    fn voices(&mut self, parent: usize, c: &Category, language: &str, leaves: &[&Leaf]) {
        let mut sorted: Vec<&Leaf> = leaves.to_vec();
        sorted.sort_by_key(|l| (l.name.to_lowercase(), l.id));
        for l in sorted.iter().filter(|l| !l.novelty) {
            self.voice(parent, l);
        }
        if sorted.iter().any(|l| l.novelty) {
            let id = ids::hash(&format!("node:{}/{language}/novelty", c.key));
            let group = self.push(
                Some(parent),
                id,
                NodeKind::Novelty,
                0,
                c.id,
                "novelty",
                "Novelty",
            );
            self.auto(group, ids::group_auto(&c.key, language, true), c.id);
            for l in sorted.iter().filter(|l| l.novelty) {
                self.voice(group, l);
            }
        }
    }

    fn category(&mut self, root: usize, c: &Category, leaves: &[&Leaf]) {
        let node = self.push(
            Some(root),
            c.id,
            NodeKind::Category,
            0,
            c.id,
            &c.key,
            &c.name,
        );
        self.auto(node, c.auto, c.id);
        // Voices that carry no language sit directly in the category.
        let plain: Vec<&Leaf> = leaves
            .iter()
            .copied()
            .filter(|l| l.language.is_empty())
            .collect();
        self.voices(node, c, "", &plain);
        // One group per language tag: the preferred tag first, then the rest of that language,
        // then everything else; by display name within each.
        let mut tags: Vec<&str> = Vec::new();
        for l in leaves {
            if !l.language.is_empty() && !tags.contains(&l.language.as_str()) {
                tags.push(&l.language);
            }
        }
        let mut groups: Vec<(u8, String, &str)> = tags
            .into_iter()
            .map(|tag| {
                let norm = normalize(tag);
                let tier = if self.preferred.is_empty() {
                    2
                } else if norm == self.preferred {
                    0
                } else if primary(&norm) == primary(&self.preferred) {
                    1
                } else {
                    2
                };
                (tier, (self.language_name)(tag), tag)
            })
            .collect();
        groups.sort_by_key(|(tier, label, tag)| (*tier, label.to_lowercase(), tag.to_string()));
        for (_, label, tag) in groups {
            let id = ids::hash(&format!("node:{}/{tag}", c.key));
            let group = self.push(Some(node), id, NodeKind::Language, 0, c.id, tag, &label);
            self.auto(group, ids::group_auto(&c.key, tag, false), c.id);
            let members: Vec<&Leaf> = leaves
                .iter()
                .copied()
                .filter(|l| l.language == tag)
                .collect();
            self.voices(group, c, tag, &members);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::LeafSpec;
    use crate::features;
    use crate::tree::{AVSPEECH, CLIPBOARD, SCREEN_READERS};

    fn spec(category: &str, name: &str, language: &str, novelty: bool) -> LeafSpec {
        LeafSpec {
            category: category.into(),
            category_name: None,
            category_rank: None,
            name: name.into(),
            language: language.into(),
            native_id: format!("{}:{language}", name.to_lowercase()),
            dedupe: None,
            features: features::SPEAK,
            selector: 1,
            is_default: name == "Samantha",
            available: name != "JAWS",
            novelty,
            language_default: matches!(name, "Samantha" | "Daniel" | "Thomas"),
        }
    }

    fn sample() -> Tree {
        let v = |c, n, l, nov| (0usize, "t", spec(c, n, l, nov));
        Tree::build(
            7,
            vec![
                v(SCREEN_READERS, "NVDA", "", false),
                v(SCREEN_READERS, "JAWS", "", false),
                v(AVSPEECH, "Zarvox", "en-US", true),
                v(AVSPEECH, "Samantha", "en-US", false),
                v(AVSPEECH, "Bells", "en-US", true),
                v(AVSPEECH, "Fred", "en-US", false),
                v(AVSPEECH, "Daniel", "en-GB", false),
                v(AVSPEECH, "Thomas", "fr-FR", false),
                v(AVSPEECH, "Anna", "de-DE", false),
                v(CLIPBOARD, "Clipboard", "", false),
            ],
        )
    }

    fn names(tag: &str) -> String {
        match tag {
            "en-US" => "English (United States)",
            "en-GB" => "English (United Kingdom)",
            "fr-FR" => "French (France)",
            "de-DE" => "German (Germany)",
            other => other,
        }
        .to_string()
    }

    /// `label` of each child, groups shown with a trailing `/`.
    fn children(ui: &UiTree, node: &Node) -> Vec<String> {
        node.children
            .iter()
            .map(|&i| {
                let n = &ui.nodes[i];
                if n.children.is_empty() {
                    n.label.clone()
                } else {
                    format!("{}/", n.label)
                }
            })
            .collect()
    }

    fn child<'a>(ui: &'a UiTree, node: &Node, label: &str) -> &'a Node {
        node.children
            .iter()
            .map(|&i| &ui.nodes[i])
            .find(|n| n.label == label)
            .unwrap_or_else(|| panic!("no child {label}"))
    }

    #[test]
    fn the_arrangement() {
        let tree = sample();
        let ui = UiTree::build(&tree, "en_us", &names);
        let root = ui.root().unwrap();
        assert_eq!(root.kind, NodeKind::Root);
        assert_eq!(
            children(&ui, root),
            ["Auto", "Screen readers/", "AVSpeech/", "Clipboard"],
            "auto first; a single-voice category is its voice"
        );
        let readers = child(&ui, root, "Screen readers");
        assert_eq!(
            children(&ui, readers),
            ["Auto", "NVDA"],
            "no languages: flat; JAWS is not running, so not listed"
        );
        let av = child(&ui, root, "AVSpeech");
        assert_eq!(
            children(&ui, av),
            [
                "Auto",
                "English (United States)/",
                "English (United Kingdom)/",
                "French (France)/",
                "German (Germany)/"
            ],
            "the preferred tag, then the rest of its language, then the others by name"
        );
        let us = child(&ui, av, "English (United States)");
        assert_eq!((us.kind, us.key.as_str()), (NodeKind::Language, "en-US"));
        assert_eq!(
            children(&ui, us),
            ["Auto", "Fred", "Samantha", "Novelty/"],
            "auto, regular voices by name, novelty last"
        );
        let novelty = child(&ui, us, "Novelty");
        assert_eq!(novelty.kind, NodeKind::Novelty);
        assert_eq!(children(&ui, novelty), ["Auto", "Bells", "Zarvox"]);
        let clip = child(&ui, root, "Clipboard");
        assert_eq!(clip.kind, NodeKind::Voice);
        assert_eq!(clip.category, ids::category_id(CLIPBOARD));
        // No preference: purely by name.
        let ui = UiTree::build(&tree, "", &names);
        let av = child(&ui, ui.root().unwrap(), "AVSpeech");
        assert_eq!(
            children(&ui, av)[1..],
            [
                "English (United Kingdom)/",
                "English (United States)/",
                "French (France)/",
                "German (Germany)/"
            ]
        );
    }

    #[test]
    fn every_auto_resolves_inside_its_group() {
        let tree = sample();
        let ui = UiTree::build(&tree, "en-US", &names);
        let speaks = |n: &Node| {
            let r = tree.resolve(n.voice, true);
            tree.leaves[r.leaf.expect("resolves")].name.clone()
        };
        let auto = |group: &Node| {
            let first = &ui.nodes[group.children[0]];
            assert_eq!(first.kind, NodeKind::Auto, "auto opens {}", group.label);
            speaks(first)
        };
        let root = ui.root().unwrap();
        assert_eq!(auto(root), "NVDA", "the root auto: a running reader");
        let av = child(&ui, root, "AVSpeech");
        assert_eq!(auto(av), "Samantha", "the system default");
        assert_eq!(auto(child(&ui, av, "English (United Kingdom)")), "Daniel");
        assert_eq!(auto(child(&ui, av, "French (France)")), "Thomas");
        assert_eq!(
            auto(child(&ui, av, "German (Germany)")),
            "Anna",
            "no default marked: the first voice"
        );
        let us = child(&ui, av, "English (United States)");
        assert_eq!(
            auto(us),
            "Samantha",
            "never a novelty voice while a regular one exists"
        );
        assert_eq!(
            auto(child(&ui, us, "Novelty")),
            "Zarvox",
            "the first novelty voice probed"
        );
        // A group auto whose voices are gone falls back up the tree like a voice does.
        let gone = ids::group_auto(AVSPEECH, "xx-XX", false);
        assert_eq!(tree.resolve(gone, true).leaf, None);
    }

    #[test]
    fn ids_are_stable_and_voices_are_found() {
        let tree = sample();
        let a = UiTree::build(&tree, "en-US", &names);
        let b = UiTree::build(&tree, "fr", &names);
        let mut ids_a: Vec<u64> = a.nodes.iter().map(|n| n.id).collect();
        let mut ids_b: Vec<u64> = b.nodes.iter().map(|n| n.id).collect();
        assert!(ids_a.iter().all(|&i| i != 0));
        ids_a.sort_unstable();
        ids_b.sort_unstable();
        assert_eq!(ids_a, ids_b, "the same nodes whatever the order");
        ids_a.dedup();
        assert_eq!(ids_a.len(), a.nodes.len(), "no two nodes share an id");
        // French first for a French player.
        let av = child(&b, b.root().unwrap(), "AVSpeech");
        assert_eq!(children(&b, av)[1], "French (France)/");
        // A saved voice leads back to its node, and so does a saved auto.
        let fred = tree.leaves.iter().find(|l| l.name == "Fred").unwrap();
        let node = a.find_voice(fred.id).unwrap();
        assert_eq!(
            a.nodes[node.parent.unwrap()].label,
            "English (United States)"
        );
        assert_eq!(a.find_voice(ids::ROOT_AUTO).unwrap().kind, NodeKind::Auto);
        let uk_auto = ids::group_auto(AVSPEECH, "en-GB", false);
        let node = a.find_voice(uk_auto).unwrap();
        assert_eq!(
            a.nodes[node.parent.unwrap()].label,
            "English (United Kingdom)"
        );
        let jaws = tree.leaves.iter().find(|l| l.name == "JAWS").unwrap();
        assert!(
            a.find_voice(jaws.id).is_none(),
            "an absent voice has no node"
        );
        assert_eq!(a.node(node.id), Some(node));
    }

    #[test]
    fn an_empty_tree_is_a_bare_root() {
        let ui = UiTree::build(&Tree::default(), "en", &names);
        assert_eq!(ui.nodes.len(), 1);
        assert!(ui.root().unwrap().children.is_empty());
    }
}
