//! Print the menu tree (speech-spec §2, "The menu tree") for this machine, each Auto with the
//! voice it resolves to.
//!
//!     cargo run -p loadstar-speech --example voice_menu            (preferred language: en-US)
//!     cargo run -p loadstar-speech --example voice_menu -- fr      (French first, names in French)
//!     cargo run -p loadstar-speech --example voice_menu -- en-US all   (every language expanded)

use std::time::Duration;

use loadstar_speech::{Config, Node, NodeKind, Speech, UiTree};

fn main() {
    let language = std::env::args().nth(1).unwrap_or_else(|| "en-US".into());
    let all = std::env::args().nth(2).as_deref() == Some("all");
    let s = Speech::new(Config::default());
    if !s.wait_ready(Duration::from_secs(10)) {
        eprintln!("first probe never published");
        std::process::exit(1);
    }
    s.set_ui_language(&language);
    let ui = s.ui_tree();
    let tree = s.tree();
    let voices = ui
        .nodes
        .iter()
        .filter(|n| n.kind == NodeKind::Voice)
        .count();
    println!(
        "{} nodes, {voices} voices, preferred language {language}",
        ui.nodes.len()
    );
    let resolved = |n: &Node| {
        tree.resolve(n.voice, true)
            .leaf
            .map_or("nothing".to_string(), |i| tree.leaves[i].name.clone())
    };
    fn walk(ui: &UiTree, n: &Node, depth: usize, all: bool, resolved: &dyn Fn(&Node) -> String) {
        let pad = "  ".repeat(depth);
        for (i, &c) in n.children.iter().enumerate() {
            let c = &ui.nodes[c];
            match c.kind {
                NodeKind::Auto => println!("{pad}Auto -> {}", resolved(c)),
                NodeKind::Voice => println!("{pad}{}", c.label),
                _ => {
                    println!(
                        "{pad}{} [{}] ({} children)",
                        c.label,
                        c.key,
                        c.children.len()
                    );
                    // Past the first few languages, only the headings.
                    if all || c.kind != NodeKind::Language || i <= 3 {
                        walk(ui, c, depth + 1, all, resolved);
                    }
                }
            }
        }
    }
    walk(&ui, ui.root().expect("a root"), 0, all, &resolved);
}
