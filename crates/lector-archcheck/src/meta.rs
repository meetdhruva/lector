//! `cargo metadata` as plain data: workspace members with their direct dependencies, and the
//! resolved graph (for dependency closures).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde_json::Value;

use crate::error::{ArchError, Result};

/// How a dependency is used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DepKind {
    /// A normal `[dependencies]` entry.
    Normal,
    /// A `[build-dependencies]` entry.
    Build,
    /// A `[dev-dependencies]` entry.
    Dev,
}

/// A direct dependency as declared in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    /// Package name of the dependency.
    pub name: String,
    /// Normal, build or dev.
    pub kind: DepKind,
}

/// A compilation target (`lib`, `bin`, ...).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// Target kinds as cargo reports them (`lib`, `bin`, `test`, ...).
    pub kinds: Vec<String>,
    /// Root source file, relative to the repository root.
    pub src_path: String,
}

/// A workspace member.
#[derive(Debug, Clone)]
pub struct Package {
    /// Package name.
    pub name: String,
    /// `crates/<dir>/Cargo.toml`, relative to the repository root.
    pub manifest: String,
    /// `crates/<dir>`.
    pub dir: String,
    /// Direct dependencies (all kinds, internal and external).
    pub deps: Vec<Dependency>,
    /// Targets.
    pub targets: Vec<Target>,
    /// Manifest text (used only to give violations a line number).
    pub manifest_text: String,
}

impl Package {
    /// A package with no dependencies or targets (tests).
    pub fn bare(name: &str) -> Package {
        Package {
            name: name.to_string(),
            manifest: format!("crates/{name}/Cargo.toml"),
            dir: format!("crates/{name}"),
            deps: Vec::new(),
            targets: Vec::new(),
            manifest_text: String::new(),
        }
    }

    /// Builder helper: add a dependency (tests).
    pub fn with_dep(mut self, name: &str, kind: DepKind) -> Package {
        self.deps.push(Dependency {
            name: name.to_string(),
            kind,
        });
        self
    }

    /// 1-based line of the `name = ...` entry in the manifest (1 when not found).
    pub fn dep_line(&self, name: &str) -> u32 {
        for (i, line) in self.manifest_text.lines().enumerate() {
            let t = line.trim_start();
            if let Some(rest) = t.strip_prefix(name) {
                if rest.starts_with([' ', '=', '.']) {
                    return (i + 1) as u32;
                }
            }
        }
        1
    }
}

/// The workspace members, sorted by name.
#[derive(Debug, Clone, Default)]
pub struct Workspace {
    /// Members.
    pub packages: Vec<Package>,
}

impl Workspace {
    /// Member by name.
    pub fn get(&self, name: &str) -> Option<&Package> {
        self.packages.iter().find(|p| p.name == name)
    }

    /// Internal edges `(from, to)` over normal and build dependencies, sorted and unique.
    pub fn internal_edges(&self) -> Vec<(String, String)> {
        let names: BTreeSet<&str> = self.packages.iter().map(|p| p.name.as_str()).collect();
        let mut edges = BTreeSet::new();
        for p in &self.packages {
            for d in &p.deps {
                if d.kind != DepKind::Dev && names.contains(d.name.as_str()) && d.name != p.name {
                    edges.insert((p.name.clone(), d.name.clone()));
                }
            }
        }
        edges.into_iter().collect()
    }
}

/// The resolved dependency graph of the whole build (all platforms), by package id.
#[derive(Debug, Clone, Default)]
pub struct Resolve {
    /// Package id to package name.
    pub names: BTreeMap<String, String>,
    /// Package id to the ids of its normal dependencies.
    pub edges: BTreeMap<String, Vec<String>>,
    /// Workspace member name to package id.
    pub members: BTreeMap<String, String>,
}

impl Resolve {
    /// For every package name in the normal-dependency closure of `member`, the shortest path
    /// from the member to it (names, member first). The member itself is not included.
    pub fn closure_paths(&self, member: &str) -> BTreeMap<String, Vec<String>> {
        let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let Some(start) = self.members.get(member) else {
            return out;
        };
        let mut parent: BTreeMap<&str, &str> = BTreeMap::new();
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        let mut queue = std::collections::VecDeque::new();
        seen.insert(start.as_str());
        queue.push_back(start.as_str());
        while let Some(id) = queue.pop_front() {
            for dep in self.edges.get(id).map(Vec::as_slice).unwrap_or(&[]) {
                if seen.insert(dep.as_str()) {
                    parent.insert(dep.as_str(), id);
                    queue.push_back(dep.as_str());
                }
            }
        }
        for id in &seen {
            if *id == start.as_str() {
                continue;
            }
            let name = self
                .names
                .get(*id)
                .cloned()
                .unwrap_or_else(|| (*id).to_string());
            let mut path = vec![name.clone()];
            let mut cur = *id;
            while let Some(p) = parent.get(cur) {
                path.push(
                    self.names
                        .get(*p)
                        .cloned()
                        .unwrap_or_else(|| (*p).to_string()),
                );
                cur = p;
            }
            path.reverse();
            let better = out.get(&name).is_none_or(|old| old.len() > path.len());
            if better {
                out.insert(name, path);
            }
        }
        out
    }
}

/// Run `cargo metadata --format-version 1 [--no-deps]` for the workspace at `root`.
pub fn run_cargo_metadata(root: &Path, no_deps: bool) -> Result<String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let mut cmd = Command::new(cargo);
    cmd.args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(root.join("Cargo.toml"));
    if no_deps {
        cmd.arg("--no-deps");
    }
    let out = cmd
        .output()
        .map_err(|e| ArchError::Cargo(format!("cannot run cargo: {e}")))?;
    if !out.status.success() {
        return Err(ArchError::Cargo(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| ArchError::Cargo(e.to_string()))
}

fn normalize(p: &str) -> String {
    let s = p.replace('\\', "/");
    s.strip_prefix("//?/").unwrap_or(&s).to_string()
}

fn relative_to(root_forms: &[String], path: &str) -> String {
    let p = normalize(path);
    for r in root_forms {
        if let Some(rest) = p.strip_prefix(r.as_str()) {
            return rest.trim_start_matches('/').to_string();
        }
    }
    p
}

fn root_forms(root: &Path) -> Vec<String> {
    let mut forms = vec![normalize(&root.to_string_lossy())];
    if let Ok(c) = root.canonicalize() {
        forms.push(normalize(&c.to_string_lossy()));
    }
    for f in &mut forms {
        while f.ends_with('/') {
            f.pop();
        }
    }
    forms.dedup();
    forms
}

fn parse_json(json: &str) -> Result<Value> {
    serde_json::from_str(json).map_err(|e| ArchError::Cargo(format!("bad JSON: {e}")))
}

/// Read the workspace members out of `cargo metadata --no-deps` (or the full) output.
pub fn parse_workspace(json: &str, root: &Path) -> Result<Workspace> {
    let v = parse_json(json)?;
    let forms = root_forms(root);
    let members: Option<BTreeSet<&str>> = v["workspace_members"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect());
    let mut packages = Vec::new();
    for p in v["packages"].as_array().into_iter().flatten() {
        if let (Some(ms), Some(id)) = (&members, p["id"].as_str()) {
            if !ms.contains(id) {
                continue;
            }
        }
        let name = p["name"].as_str().unwrap_or_default().to_string();
        let manifest = relative_to(&forms, p["manifest_path"].as_str().unwrap_or_default());
        let dir = manifest
            .rsplit_once('/')
            .map_or(String::new(), |(d, _)| d.to_string());
        let mut deps = Vec::new();
        for d in p["dependencies"].as_array().into_iter().flatten() {
            let kind = match d["kind"].as_str() {
                Some("dev") => DepKind::Dev,
                Some("build") => DepKind::Build,
                _ => DepKind::Normal,
            };
            deps.push(Dependency {
                name: d["name"].as_str().unwrap_or_default().to_string(),
                kind,
            });
        }
        let mut targets = Vec::new();
        for t in p["targets"].as_array().into_iter().flatten() {
            targets.push(Target {
                kinds: t["kind"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|k| k.as_str().map(str::to_string))
                    .collect(),
                src_path: relative_to(&forms, t["src_path"].as_str().unwrap_or_default()),
            });
        }
        let manifest_text = std::fs::read_to_string(root.join(&manifest))
            .map(|t| t.replace("\r\n", "\n"))
            .unwrap_or_default();
        packages.push(Package {
            name,
            manifest,
            dir,
            deps,
            targets,
            manifest_text,
        });
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(Workspace { packages })
}

/// Read the resolved graph out of the full `cargo metadata` output.
pub fn parse_resolve(json: &str) -> Result<Resolve> {
    let v = parse_json(json)?;
    let mut out = Resolve::default();
    for p in v["packages"].as_array().into_iter().flatten() {
        if let (Some(id), Some(name)) = (p["id"].as_str(), p["name"].as_str()) {
            out.names.insert(id.to_string(), name.to_string());
        }
    }
    for id in v["workspace_members"].as_array().into_iter().flatten() {
        if let Some(id) = id.as_str() {
            if let Some(name) = out.names.get(id) {
                out.members.insert(name.clone(), id.to_string());
            }
        }
    }
    for n in v["resolve"]["nodes"].as_array().into_iter().flatten() {
        let Some(id) = n["id"].as_str() else { continue };
        let mut deps = Vec::new();
        for d in n["deps"].as_array().into_iter().flatten() {
            let normal = d["dep_kinds"]
                .as_array()
                .is_some_and(|ks| ks.iter().any(|k| k["kind"].is_null()));
            if normal {
                if let Some(pkg) = d["pkg"].as_str() {
                    deps.push(pkg.to_string());
                }
            }
        }
        out.edges.insert(id.to_string(), deps);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "packages": [
        {"id":"a 0.1.0 (path+file:///r/crates/a)","name":"a","manifest_path":"/r/crates/a/Cargo.toml",
         "dependencies":[{"name":"b","kind":null},{"name":"serde","kind":null},{"name":"c","kind":"dev"},{"name":"d","kind":"build"}],
         "targets":[{"kind":["lib"],"src_path":"/r/crates/a/src/lib.rs"}]},
        {"id":"b 0.1.0 (path+file:///r/crates/b)","name":"b","manifest_path":"/r/crates/b/Cargo.toml","dependencies":[],"targets":[]},
        {"id":"c 0.1.0 (path+file:///r/crates/c)","name":"c","manifest_path":"/r/crates/c/Cargo.toml","dependencies":[],"targets":[]},
        {"id":"d 0.1.0 (path+file:///r/crates/d)","name":"d","manifest_path":"/r/crates/d/Cargo.toml","dependencies":[],"targets":[]},
        {"id":"serde 1.0.0 (registry)","name":"serde","manifest_path":"/x/serde/Cargo.toml","dependencies":[],"targets":[]},
        {"id":"tokio 1.0.0 (registry)","name":"tokio","manifest_path":"/x/tokio/Cargo.toml","dependencies":[],"targets":[]}
      ],
      "workspace_members": ["a 0.1.0 (path+file:///r/crates/a)","b 0.1.0 (path+file:///r/crates/b)","c 0.1.0 (path+file:///r/crates/c)","d 0.1.0 (path+file:///r/crates/d)"],
      "resolve": {"nodes": [
        {"id":"a 0.1.0 (path+file:///r/crates/a)","deps":[
          {"pkg":"b 0.1.0 (path+file:///r/crates/b)","dep_kinds":[{"kind":null}]},
          {"pkg":"serde 1.0.0 (registry)","dep_kinds":[{"kind":null}]},
          {"pkg":"c 0.1.0 (path+file:///r/crates/c)","dep_kinds":[{"kind":"dev"}]}]},
        {"id":"b 0.1.0 (path+file:///r/crates/b)","deps":[{"pkg":"tokio 1.0.0 (registry)","dep_kinds":[{"kind":null}]}]},
        {"id":"c 0.1.0 (path+file:///r/crates/c)","deps":[]},
        {"id":"d 0.1.0 (path+file:///r/crates/d)","deps":[]},
        {"id":"serde 1.0.0 (registry)","deps":[]},
        {"id":"tokio 1.0.0 (registry)","deps":[]}
      ]}
    }"#;

    #[test]
    fn workspace_members_and_internal_edges() {
        let ws = parse_workspace(SAMPLE, Path::new("/r")).unwrap();
        assert_eq!(ws.packages.len(), 4);
        let a = ws.get("a").unwrap();
        assert_eq!(a.manifest, "crates/a/Cargo.toml");
        assert_eq!(a.dir, "crates/a");
        assert_eq!(a.targets[0].src_path, "crates/a/src/lib.rs");
        // Dev edge excluded, external excluded, build included.
        assert_eq!(
            ws.internal_edges(),
            [
                ("a".to_string(), "b".to_string()),
                ("a".to_string(), "d".to_string())
            ]
        );
    }

    #[test]
    fn closure_uses_normal_edges_only_and_finds_shortest_path() {
        let r = parse_resolve(SAMPLE).unwrap();
        let paths = r.closure_paths("a");
        assert_eq!(paths.get("tokio").unwrap(), &["a", "b", "tokio"]);
        assert!(paths.contains_key("serde"));
        assert!(
            !paths.contains_key("c"),
            "dev edges are not part of the closure"
        );
        assert!(!paths.contains_key("a"));
        assert!(r.closure_paths("missing").is_empty());
    }

    #[test]
    fn dependency_line_lookup() {
        let mut p = Package::bare("x");
        p.manifest_text = "[package]\nname = \"x\"\n[dependencies]\nlector-a.workspace = true\nlector-b = { path = \"..\" }\n".to_string();
        assert_eq!(p.dep_line("lector-a"), 4);
        assert_eq!(p.dep_line("lector-b"), 5);
        assert_eq!(p.dep_line("nope"), 1);
    }

    #[test]
    fn windows_style_paths_are_normalised() {
        let forms = vec!["C:/r".to_string()];
        assert_eq!(
            relative_to(&forms, "C:\\r\\crates\\a\\Cargo.toml"),
            "crates/a/Cargo.toml"
        );
        assert_eq!(normalize("\\\\?\\C:\\r"), "C:/r");
    }
}
