//! `deps` (ADR 0001, 0002, 0008): crate layering, forbidden edges, and dependency closures.
//!
//! Inputs are plain data ([`Workspace`] from `cargo metadata --no-deps`, [`Resolve`] from the
//! full `cargo metadata`, [`Layers`] from `arch/layers.toml`), so every check is testable
//! without running cargo.
//!
//! Findings, each counted once under its baseline key:
//!
//! * `unplaced <crate>`: a workspace crate that `arch/layers.toml` does not place.
//! * `<from> -> <to>`: an internal edge (normal or build dependency; dev-dependencies are
//!   ignored) that either matches a `[[forbid]]` entry, points at a higher layer, or stays in
//!   the same layer without a `[[same_layer]]` entry. A forbidden edge is reported with its
//!   ADR; target rules that the tree does not meet yet are baselined this way.
//! * `crate <name>`: a crate that `[[forbid_crate]]` says must not exist.
//! * `closure <crate> -> <package>`: a core crate (every workspace crate not matching
//!   `closure.gui_crates`) whose normal dependency closure reaches a package matching
//!   `closure.forbidden` (GUI, window system, async runtime), or a `closure.headless` crate
//!   whose closure reaches a `closure.headless_forbidden` package.

use crate::config::{glob_match, Layers};
use crate::meta::{Resolve, Workspace};
use crate::report::{RuleOutput, Unit, Violation};

/// Rule name.
pub const RULE: &str = "deps";

/// Run the rule. `resolve` is `None` when the dependency closure should be skipped.
pub fn check(ws: &Workspace, resolve: Option<&Resolve>, layers: &Layers) -> RuleOutput {
    let mut found = Vec::new();
    unplaced(ws, layers, &mut found);
    edges(ws, layers, &mut found);
    forbidden_crates(ws, layers, &mut found);
    if let Some(resolve) = resolve {
        closures(ws, resolve, layers, &mut found);
    }
    RuleOutput::new(RULE, found, Unit::Count)
}

fn unplaced(ws: &Workspace, layers: &Layers, out: &mut Vec<Violation>) {
    for p in &ws.packages {
        if layers.layer_of(&p.name).is_none() {
            out.push(
                Violation::new(
                    RULE,
                    &p.manifest,
                    1,
                    format!("crate `{}` is not placed in arch/layers.toml", p.name),
                )
                .keyed(format!("unplaced {}", p.name)),
            );
        }
    }
}

fn edges(ws: &Workspace, layers: &Layers, out: &mut Vec<Violation>) {
    for (from, to) in ws.internal_edges() {
        let Some(pkg) = ws.get(&from) else { continue };
        let line = pkg.dep_line(&to);
        let key = format!("{from} -> {to}");
        let mut push = |message: String| {
            out.push(Violation::new(RULE, &pkg.manifest, line, message).keyed(key.clone()));
        };
        if let Some(rule) = layers
            .forbid
            .iter()
            .find(|f| glob_match(&f.from, &from) && glob_match(&f.to, &to))
        {
            push(format!(
                "forbidden edge {from} -> {to} (ADR {}): {}",
                rule.adr, rule.reason
            ));
            continue;
        }
        let (Some(a), Some(b)) = (layers.layer_of(&from), layers.layer_of(&to)) else {
            continue;
        };
        if b > a {
            push(format!(
                "`{from}` (layer {} `{}`) depends on `{to}` in a higher layer (`{}`)",
                a, layers.layers[a].name, layers.layers[b].name
            ));
        } else if a == b
            && !layers
                .same_layer
                .iter()
                .any(|(f, t)| f == &from && t == &to)
        {
            push(format!(
                "`{from}` depends on `{to}` in the same layer (`{}`); list it under [[same_layer]] or move a crate",
                layers.layers[a].name
            ));
        }
    }
}

fn forbidden_crates(ws: &Workspace, layers: &Layers, out: &mut Vec<Violation>) {
    for rule in &layers.forbid_crates {
        if let Some(p) = ws.get(&rule.name) {
            out.push(
                Violation::new(
                    RULE,
                    &p.manifest,
                    1,
                    format!(
                        "crate `{}` must not exist (ADR {}): {}",
                        rule.name, rule.adr, rule.reason
                    ),
                )
                .keyed(format!("crate {}", rule.name)),
            );
        }
    }
}

fn matches_any(patterns: &[String], name: &str) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

fn closures(ws: &Workspace, resolve: &Resolve, layers: &Layers, out: &mut Vec<Violation>) {
    let cfg = &layers.closure;
    for p in &ws.packages {
        let core = !matches_any(&cfg.gui_crates, &p.name);
        let headless = cfg.headless.contains(&p.name);
        if !core && !headless {
            continue;
        }
        for (dep, path) in resolve.closure_paths(&p.name) {
            let by_core = core && matches_any(&cfg.forbidden, &dep);
            let by_headless = headless && matches_any(&cfg.headless_forbidden, &dep);
            if !(by_core || by_headless) {
                continue;
            }
            let why = if by_core {
                "core crates must not depend on GUI, window-system or async-runtime crates"
            } else {
                "the headless CLI/MCP closure must stay GUI-free"
            };
            out.push(
                Violation::new(
                    RULE,
                    &p.manifest,
                    1,
                    format!(
                        "`{}` reaches `{dep}` through {} ({why})",
                        p.name,
                        path.join(" -> ")
                    ),
                )
                .keyed(format!("closure {} -> {dep}", p.name)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meta::{DepKind, Package};
    use std::collections::BTreeMap;

    const LAYERS: &str = r#"
[[layer]]
name = "base"
crates = ["a", "b"]
[[layer]]
name = "middle"
crates = ["c", "d"]
[[layer]]
name = "top"
crates = ["gui", "bin"]
[[same_layer]]
from = "d"
to = "c"
[[forbid]]
from = "gui*"
to = "proc"
adr = "0002"
reason = "UI must not touch processes"
[[forbid_crate]]
name = "old"
adr = "0001"
reason = "removed"
[closure]
gui_crates = ["gui", "bin"]
forbidden = ["gpui*", "tokio"]
headless = ["d"]
headless_forbidden = ["gpui*"]
"#;

    fn layers() -> Layers {
        Layers::parse(LAYERS).unwrap()
    }

    fn pkg(name: &str, deps: &[&str]) -> Package {
        deps.iter()
            .fold(Package::bare(name), |p, d| p.with_dep(d, DepKind::Normal))
    }

    fn run(pkgs: Vec<Package>) -> Vec<String> {
        let ws = Workspace { packages: pkgs };
        check(&ws, None, &layers())
            .violations
            .into_iter()
            .map(|v| v.key)
            .collect()
    }

    #[test]
    fn clean_graph_passes() {
        let keys = run(vec![
            pkg("a", &[]),
            pkg("b", &[]),
            pkg("c", &["a", "b"]),
            pkg("d", &["c"]),
            pkg("gui", &["c", "d"]),
            pkg("bin", &[]),
        ]);
        assert!(keys.is_empty(), "{keys:?}");
    }

    #[test]
    fn lower_layer_edges_are_fine_and_dev_edges_are_ignored() {
        let mut c = pkg("c", &["a"]);
        c.deps.push(crate::meta::Dependency {
            name: "gui".into(),
            kind: DepKind::Dev,
        });
        let keys = run(vec![
            pkg("a", &[]),
            pkg("b", &[]),
            c,
            pkg("d", &["c"]),
            pkg("gui", &["c"]),
            pkg("bin", &[]),
            pkg("proc", &[]),
        ]);
        // `proc` is unplaced; everything else is fine (d -> c is a listed same-layer edge).
        assert_eq!(keys, ["unplaced proc"]);
    }

    #[test]
    fn higher_layer_and_unlisted_same_layer_edges_fail() {
        let keys = run(vec![
            pkg("a", &["c"]),
            pkg("c", &["d"]),
            pkg("d", &[]),
            pkg("b", &[]),
            pkg("gui", &[]),
            pkg("bin", &[]),
        ]);
        assert_eq!(keys, ["a -> c", "c -> d"]);
    }

    #[test]
    fn forbidden_edges_match_globs_and_name_the_adr() {
        let ws = Workspace {
            packages: vec![
                pkg("a", &[]),
                pkg("b", &[]),
                pkg("c", &[]),
                pkg("d", &[]),
                pkg("gui", &["proc"]),
                pkg("bin", &[]),
                pkg("proc", &[]),
            ],
        };
        let mut l = layers();
        l.layers[0].crates.push("proc".into());
        let out = check(&ws, None, &l);
        assert_eq!(out.violations.len(), 1);
        assert!(out.violations[0].message.contains("ADR 0002"));
        assert_eq!(out.entries["gui -> proc"], 1);
    }

    #[test]
    fn forbidden_crates() {
        let mut l = layers();
        l.layers[0].crates.push("old".into());
        let ws = Workspace {
            packages: vec![
                pkg("old", &[]),
                pkg("a", &[]),
                pkg("b", &[]),
                pkg("c", &[]),
                pkg("d", &[]),
                pkg("gui", &[]),
                pkg("bin", &[]),
            ],
        };
        let out = check(&ws, None, &l);
        assert_eq!(out.entries.keys().collect::<Vec<_>>(), ["crate old"]);
        assert_eq!(out.violations[0].file, "crates/old/Cargo.toml");
    }

    fn resolve() -> Resolve {
        let mut r = Resolve::default();
        for (id, name) in [
            ("a", "a"),
            ("d", "d"),
            ("gui", "gui"),
            ("gpui", "gpui"),
            ("gpui_macros", "gpui_macros"),
            ("tokio", "tokio"),
            ("serde", "serde"),
        ] {
            r.names.insert(id.into(), name.into());
        }
        for m in ["a", "d", "gui"] {
            r.members.insert(m.into(), m.into());
        }
        let edges: BTreeMap<&str, Vec<&str>> = [
            ("a", vec!["serde"]),
            ("d", vec!["a", "tokio"]),
            ("gui", vec!["gpui", "tokio"]),
            ("gpui", vec!["gpui_macros"]),
        ]
        .into_iter()
        .collect();
        for (k, v) in edges {
            r.edges
                .insert(k.into(), v.into_iter().map(String::from).collect());
        }
        r
    }

    #[test]
    fn closure_finds_forbidden_packages_in_core_crates_only() {
        let ws = Workspace {
            packages: vec![pkg("a", &[]), pkg("d", &[]), pkg("gui", &[])],
        };
        let mut l = layers();
        l.layers[0].crates.push("b".into());
        let out = check(&ws, Some(&resolve()), &l);
        let closure: Vec<&String> = out
            .entries
            .keys()
            .filter(|k| k.starts_with("closure"))
            .collect();
        assert_eq!(closure, ["closure d -> tokio"]);
        assert!(out
            .violations
            .iter()
            .any(|v| v.message.contains("d -> tokio")));
    }

    #[test]
    fn headless_closure_is_checked_with_its_own_list() {
        let mut r = resolve();
        r.edges.insert("d".into(), vec!["gpui".into()]);
        r.edges.insert("gpui".into(), vec![]);
        let ws = Workspace {
            packages: vec![pkg("d", &[])],
        };
        let out = check(&ws, Some(&r), &layers());
        let keys: Vec<&String> = out
            .entries
            .keys()
            .filter(|k| k.starts_with("closure"))
            .collect();
        assert_eq!(keys, ["closure d -> gpui"]);
        // Reported once even though both the core and the headless list match.
        assert_eq!(
            out.violations
                .iter()
                .filter(|v| v.key == "closure d -> gpui")
                .count(),
            1
        );
    }
}
