//! Checked-in configuration: `arch/layers.toml` and the `arch/*.allow` files.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::rc::Rc;

use crate::error::{ArchError, Result};

/// Glob match with `*` (any run of characters except `/`), `**` (any run, `/` included)
/// and `?` (one character except `/`).
pub fn glob_match(pattern: &str, text: &str) -> bool {
    fn go(p: &[u8], t: &[u8]) -> bool {
        match p.first() {
            None => t.is_empty(),
            Some(b'*') if p.get(1) == Some(&b'*') => (0..=t.len()).any(|k| go(&p[2..], &t[k..])),
            Some(b'*') => {
                for k in 0..=t.len() {
                    if go(&p[1..], &t[k..]) {
                        return true;
                    }
                    if k < t.len() && t[k] == b'/' {
                        break;
                    }
                }
                false
            }
            Some(b'?') => !t.is_empty() && t[0] != b'/' && go(&p[1..], &t[1..]),
            Some(c) => t.first() == Some(c) && go(&p[1..], &t[1..]),
        }
    }
    go(pattern.as_bytes(), text.as_bytes())
}

/// Split `tag:value` into its tag and value, when the entry has a single-colon separator.
/// `kind:pty-reader` and `RISK_PATTERNS: std::sync::OnceLock<X>` are tagged; `Repo::identity` and
/// `crates/a/**` are not (a `::` is a path separator, not a tag separator).
pub fn split_tag(entry: &str) -> Option<(&str, &str)> {
    let b = entry.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b':' {
            if b.get(i + 1) == Some(&b':') {
                i += 2;
                continue;
            }
            return Some((entry[..i].trim(), entry[i + 1..].trim()));
        }
        i += 1;
    }
    None
}

#[derive(Debug, Clone)]
struct Entry {
    text: String,
    line: usize,
}

/// A reviewed allowance list (`arch/*.allow`).
///
/// One entry per line; `#` starts a comment. Every entry needs a reason: an inline `# reason`
/// or a comment line above it in the same block (blocks are separated by blank lines).
/// Plain entries are path globs or item names (depending on the file); `tag:value` entries
/// (`kind:pty-reader`, `literal:settings.toml`, `crate:lector-text`, `NAME: Type`) carry a typed
/// value. An entry that exempts nothing is stale: rules report it through [`AllowList::unused`]
/// (lookups that succeed mark their entry as used; the mark is shared by clones).
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    file: String,
    entries: Vec<Entry>,
    used: Rc<RefCell<BTreeSet<usize>>>,
}

impl AllowList {
    /// Parse `text`; `name` is the file name used in messages (`arch/threads.allow`).
    pub fn parse(name: &str, text: &str) -> Result<AllowList> {
        let mut entries = Vec::new();
        let mut block_reason = false;
        for (idx, raw) in text.lines().enumerate() {
            let line = raw.trim();
            if line.is_empty() {
                block_reason = false;
                continue;
            }
            if let Some(comment) = line.strip_prefix('#') {
                if !comment.trim().is_empty() {
                    block_reason = true;
                }
                continue;
            }
            let (entry, inline) = match line.split_once('#') {
                Some((e, c)) => (e.trim(), !c.trim().is_empty()),
                None => (line, false),
            };
            if !(inline || block_reason) {
                return Err(ArchError::Config(format!(
                    "{name}:{}: allowance `{entry}` has no reason comment (add `# why` inline or on the line above)",
                    idx + 1
                )));
            }
            entries.push(Entry {
                text: entry.to_string(),
                line: idx + 1,
            });
        }
        Ok(AllowList {
            file: name.to_string(),
            entries,
            used: Rc::default(),
        })
    }

    /// Build a list directly from entries (tests).
    pub fn from_entries<I: IntoIterator<Item = S>, S: Into<String>>(entries: I) -> AllowList {
        AllowList {
            file: "<test>".to_string(),
            entries: entries
                .into_iter()
                .enumerate()
                .map(|(i, e)| Entry {
                    text: e.into(),
                    line: i + 1,
                })
                .collect(),
            used: Rc::default(),
        }
    }

    fn mark(&self, idx: usize) {
        self.used.borrow_mut().insert(idx);
    }

    fn is_plain(e: &Entry) -> bool {
        split_tag(&e.text).is_none()
    }

    /// True when a plain entry (glob) matches the repository-relative `path`. Does not mark.
    pub fn matches_path(&self, path: &str) -> bool {
        self.entries
            .iter()
            .any(|e| Self::is_plain(e) && glob_match(&e.text, path))
    }

    /// Like [`AllowList::matches_path`], and marks every matching entry as used: call it only
    /// when the path really has something to be exempted from.
    pub fn exempt_path(&self, path: &str) -> bool {
        let mut hit = false;
        for (i, e) in self.entries.iter().enumerate() {
            if Self::is_plain(e) && glob_match(&e.text, path) {
                self.mark(i);
                hit = true;
            }
        }
        hit
    }

    /// True when a plain entry equals `name`; marks it as used.
    pub fn contains(&self, name: &str) -> bool {
        let mut hit = false;
        for (i, e) in self.entries.iter().enumerate() {
            if Self::is_plain(e) && e.text == name {
                self.mark(i);
                hit = true;
            }
        }
        hit
    }

    /// Values of the `tag:value` entries with this tag (does not mark).
    pub fn tagged(&self, tag: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|e| {
                split_tag(&e.text)
                    .filter(|(t, _)| *t == tag)
                    .map(|(_, v)| v)
            })
            .collect()
    }

    /// Mark the `tag:value` entry as used.
    pub fn mark_tagged(&self, tag: &str, value: &str) {
        for (i, e) in self.entries.iter().enumerate() {
            if split_tag(&e.text).is_some_and(|(t, v)| t == tag && v == value) {
                self.mark(i);
            }
        }
    }

    /// Every tagged entry as `(tag, value)` (`NAME: Type` entries use this).
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.entries
            .iter()
            .filter_map(|e| split_tag(&e.text))
            .collect()
    }

    /// Mark the entry `tag: value` (exact, whitespace-insensitive on the value) as used.
    pub fn mark_pair(&self, tag: &str, value: &str) {
        let norm = |s: &str| s.split_whitespace().collect::<String>();
        for (i, e) in self.entries.iter().enumerate() {
            if split_tag(&e.text).is_some_and(|(t, v)| t == tag && norm(v) == norm(value)) {
                self.mark(i);
            }
        }
    }

    /// All plain entries.
    pub fn plain(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| Self::is_plain(e))
            .map(|e| e.text.as_str())
            .collect()
    }

    /// All entries as written.
    pub fn all(&self) -> Vec<&str> {
        self.entries.iter().map(|e| e.text.as_str()).collect()
    }

    /// Number of entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when there are no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Entries never marked as used: `(line, entry)`.
    pub fn unused(&self) -> Vec<(usize, &str)> {
        let used = self.used.borrow();
        self.entries
            .iter()
            .enumerate()
            .filter(|(i, _)| !used.contains(i))
            .map(|(_, e)| (e.line, e.text.as_str()))
            .collect()
    }

    /// One violation per stale entry (an entry that exempted nothing in this run).
    pub fn stale_violations(
        &self,
        rule: &'static str,
        skip: &dyn Fn(&str) -> bool,
    ) -> Vec<crate::report::Violation> {
        self.unused()
            .into_iter()
            .filter(|(_, e)| !skip(e))
            .map(|(line, e)| {
                crate::report::Violation::new(
                    rule,
                    &self.file,
                    line as u32,
                    format!("stale allowance `{e}`: it exempts nothing any more; remove it"),
                )
            })
            .collect()
    }
}

/// One layer of `arch/layers.toml`.
#[derive(Debug, Clone)]
pub struct Layer {
    /// Human-readable name.
    pub name: String,
    /// Crates in this layer (they may depend only on lower layers).
    pub crates: Vec<String>,
}

/// An explicitly forbidden dependency edge (globs allowed).
#[derive(Debug, Clone)]
pub struct ForbiddenEdge {
    /// Depending crate (glob).
    pub from: String,
    /// Depended-on crate (glob).
    pub to: String,
    /// ADR number.
    pub adr: String,
    /// Why.
    pub reason: String,
}

/// A crate that must not exist.
#[derive(Debug, Clone)]
pub struct ForbiddenCrate {
    /// Crate name.
    pub name: String,
    /// ADR number.
    pub adr: String,
    /// Why.
    pub reason: String,
}

/// Dependency-closure rules (core crates stay free of GUI and runtimes).
#[derive(Debug, Clone, Default)]
pub struct ClosureConfig {
    /// Workspace crates (globs) that may use GUI/runtime crates; every other crate is core.
    pub gui_crates: Vec<String>,
    /// Package names (globs) that must not be in a core crate's normal dependency closure.
    pub forbidden: Vec<String>,
    /// Crates that additionally promise a GUI-free closure (headless CLI and MCP).
    pub headless: Vec<String>,
    /// Package names (globs) forbidden in the headless crates' closure.
    pub headless_forbidden: Vec<String>,
}

/// Parsed `arch/layers.toml`.
#[derive(Debug, Clone, Default)]
pub struct Layers {
    /// Layers from bottom (index 0) to top.
    pub layers: Vec<Layer>,
    /// Forbidden edges.
    pub forbid: Vec<ForbiddenEdge>,
    /// Crates that must not exist.
    pub forbid_crates: Vec<ForbiddenCrate>,
    /// Same-layer edges that are explicitly allowed (from, to).
    pub same_layer: Vec<(String, String)>,
    /// Closure rules.
    pub closure: ClosureConfig,
}

fn str_of(t: &toml::Table, key: &str, what: &str) -> Result<String> {
    t.get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            ArchError::Config(format!("arch/layers.toml: {what} needs a string `{key}`"))
        })
}

fn strs_of(t: &toml::Table, key: &str) -> Vec<String> {
    t.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn tables_of<'a>(t: &'a toml::Table, key: &str) -> Vec<&'a toml::Table> {
    t.get(key)
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_table()).collect())
        .unwrap_or_default()
}

impl Layers {
    /// Parse the text of `arch/layers.toml`.
    pub fn parse(text: &str) -> Result<Layers> {
        let doc: toml::Table = text
            .parse()
            .map_err(|e| ArchError::Config(format!("arch/layers.toml: {e}")))?;
        let mut out = Layers::default();
        for l in tables_of(&doc, "layer") {
            out.layers.push(Layer {
                name: str_of(l, "name", "a [[layer]]")?,
                crates: strs_of(l, "crates"),
            });
        }
        for f in tables_of(&doc, "forbid") {
            out.forbid.push(ForbiddenEdge {
                from: str_of(f, "from", "a [[forbid]]")?,
                to: str_of(f, "to", "a [[forbid]]")?,
                adr: str_of(f, "adr", "a [[forbid]]")?,
                reason: str_of(f, "reason", "a [[forbid]]")?,
            });
        }
        for f in tables_of(&doc, "forbid_crate") {
            out.forbid_crates.push(ForbiddenCrate {
                name: str_of(f, "name", "a [[forbid_crate]]")?,
                adr: str_of(f, "adr", "a [[forbid_crate]]")?,
                reason: str_of(f, "reason", "a [[forbid_crate]]")?,
            });
        }
        for s in tables_of(&doc, "same_layer") {
            out.same_layer.push((
                str_of(s, "from", "a [[same_layer]]")?,
                str_of(s, "to", "a [[same_layer]]")?,
            ));
        }
        if let Some(c) = doc.get("closure").and_then(|v| v.as_table()) {
            out.closure = ClosureConfig {
                gui_crates: strs_of(c, "gui_crates"),
                forbidden: strs_of(c, "forbidden"),
                headless: strs_of(c, "headless"),
                headless_forbidden: strs_of(c, "headless_forbidden"),
            };
        }
        let mut seen: BTreeMap<&str, &str> = BTreeMap::new();
        for l in &out.layers {
            for c in &l.crates {
                if let Some(prev) = seen.insert(c, &l.name) {
                    return Err(ArchError::Config(format!(
                        "arch/layers.toml: crate `{c}` is in layers `{prev}` and `{}`",
                        l.name
                    )));
                }
            }
        }
        Ok(out)
    }

    /// Index of the layer holding `krate` (0 is the bottom).
    pub fn layer_of(&self, krate: &str) -> Option<usize> {
        self.layers
            .iter()
            .position(|l| l.crates.iter().any(|c| c == krate))
    }
}

/// Everything read from `arch/`.
#[derive(Debug, Clone, Default)]
pub struct Config {
    /// `arch/layers.toml`, when present.
    pub layers: Option<Layers>,
    /// `arch/one-impl.toml`: concepts with one implementation only.
    pub concepts: Vec<Concept>,
    allow: BTreeMap<String, AllowList>,
}

/// Names of the allow files (`arch/<name>.allow`).
pub const ALLOW_FILES: &[&str] = &[
    "threads",
    "background",
    "state",
    "generated",
    "review-statics",
    "git-read",
    "git-ambiguous",
    "store-ambiguous",
    "unsafe",
    "git-callers",
    "fs-writers",
];

/// A concept that may have one implementation only (`arch/one-impl.toml`).
#[derive(Debug, Clone, Default)]
pub struct Concept {
    /// Human-readable name ("cancel token").
    pub name: String,
    /// ADR number.
    pub adr: String,
    /// Path globs of the files that may implement it.
    pub owner: Vec<String>,
    /// Name globs of `struct` / `enum` definitions.
    pub structs: Vec<String>,
    /// Name globs of `fn` definitions.
    pub fns: Vec<String>,
    /// Name globs of module names (file stems, directories, inline `mod`).
    pub modules: Vec<String>,
    /// Reviewed delegations: a function outside the owner that matches a name pattern but only
    /// calls the owner (a thin wrapper), each with the file, the function and the reason.
    pub delegates: Vec<Delegate>,
}

/// One reviewed delegation of a [`Concept`] (`delegate = [{ file, fn, why }]`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Delegate {
    /// Path of the file.
    pub file: String,
    /// Function name.
    pub name: String,
    /// Why the function is a wrapper of the owner and not a second implementation.
    pub why: String,
}

impl Concept {
    /// Parse the text of `arch/one-impl.toml`.
    pub fn parse_all(text: &str) -> Result<Vec<Concept>> {
        let doc: toml::Table = text
            .parse()
            .map_err(|e| ArchError::Config(format!("arch/one-impl.toml: {e}")))?;
        let mut out = Vec::new();
        for c in tables_of(&doc, "concept") {
            let mut concept = Concept {
                name: str_of(c, "name", "a [[concept]]")?,
                adr: str_of(c, "adr", "a [[concept]]")?,
                owner: strs_of(c, "owner"),
                structs: strs_of(c, "struct"),
                fns: strs_of(c, "fn"),
                modules: strs_of(c, "module"),
                delegates: Vec::new(),
            };
            for d in tables_of(c, "delegate") {
                let field = |k: &str| d.get(k).and_then(|v| v.as_str()).map(str::to_string);
                let (Some(file), Some(name), Some(why)) =
                    (field("file"), field("fn"), field("why"))
                else {
                    return Err(ArchError::Config(format!(
                        "arch/one-impl.toml: a `delegate` of `{}` needs `file`, `fn` and `why`",
                        concept.name
                    )));
                };
                if why.trim().is_empty() {
                    return Err(ArchError::Config(format!(
                        "arch/one-impl.toml: the delegate `{name}` of `{}` needs a reason",
                        concept.name
                    )));
                }
                concept.delegates.push(Delegate { file, name, why });
            }
            if concept.owner.is_empty() {
                return Err(ArchError::Config(format!(
                    "arch/one-impl.toml: concept `{}` needs an `owner` (path globs)",
                    concept.name
                )));
            }
            if concept.structs.is_empty() && concept.fns.is_empty() && concept.modules.is_empty() {
                return Err(ArchError::Config(format!(
                    "arch/one-impl.toml: concept `{}` has no `struct`, `fn` or `module` pattern",
                    concept.name
                )));
            }
            out.push(concept);
        }
        Ok(out)
    }
}

impl Config {
    /// Read `<root>/arch`. Missing files are empty; malformed ones are errors.
    pub fn load(root: &Path) -> Result<Config> {
        let dir = root.join("arch");
        let mut cfg = Config::default();
        let layers_path = dir.join("layers.toml");
        match fs::read_to_string(&layers_path) {
            Ok(text) => cfg.layers = Some(Layers::parse(&text.replace("\r\n", "\n"))?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ArchError::io("arch/layers.toml", e)),
        }
        match fs::read_to_string(dir.join("one-impl.toml")) {
            Ok(text) => cfg.concepts = Concept::parse_all(&text.replace("\r\n", "\n"))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(ArchError::io("arch/one-impl.toml", e)),
        }
        for name in ALLOW_FILES {
            let rel = format!("arch/{name}.allow");
            match fs::read_to_string(dir.join(format!("{name}.allow"))) {
                Ok(text) => {
                    cfg.allow.insert(
                        (*name).to_string(),
                        AllowList::parse(&rel, &text.replace("\r\n", "\n"))?,
                    );
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(ArchError::io(rel, e)),
            }
        }
        Ok(cfg)
    }

    /// The allow list `arch/<name>.allow` (empty when the file does not exist).
    pub fn allow(&self, name: &str) -> AllowList {
        self.allow.get(name).cloned().unwrap_or_default()
    }

    /// Install an allow list (tests).
    pub fn set_allow(&mut self, name: &str, list: AllowList) {
        self.allow.insert(name.to_string(), list);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_match("crates/a/src/**", "crates/a/src/x/y.rs"));
        assert!(glob_match("crates/*/src/lib.rs", "crates/a/src/lib.rs"));
        assert!(!glob_match("crates/*/lib.rs", "crates/a/src/lib.rs"));
        assert!(glob_match("lector-ui*", "lector-ui-git"));
        assert!(glob_match("lector-ui*", "lector-ui"));
        assert!(!glob_match("lector-ui*", "lector-uf"));
        assert!(glob_match(
            "crates/lector-search/src/picker*",
            "crates/lector-search/src/picker.rs"
        ));
        assert!(!glob_match(
            "crates/lector-search/src/picker*",
            "crates/lector-search/src/picker/mod.rs"
        ));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "a/c"));
    }

    #[test]
    fn allow_entries_need_reasons() {
        let ok = "# reason\nfoo\nbar\n\nbaz # inline\n";
        let list = AllowList::parse("t.allow", ok).unwrap();
        assert_eq!(list.plain(), ["foo", "bar", "baz"]);
        assert!(AllowList::parse("t.allow", "foo\n").is_err());
        assert!(AllowList::parse("t.allow", "# why\nfoo\n\nbar\n").is_err());
        assert!(AllowList::parse("t.allow", "foo #\n").is_err());
    }

    #[test]
    fn tagged_entries_and_matching() {
        let l = AllowList::parse(
            "t",
            "# r\nkind:pty-reader\nliteral:settings.toml\ncrates/a/**\nname\n",
        )
        .unwrap();
        assert_eq!(l.tagged("kind"), ["pty-reader"]);
        assert_eq!(l.tagged("literal"), ["settings.toml"]);
        assert!(l.matches_path("crates/a/src/x.rs"));
        assert!(!l.matches_path("kind:pty-reader"));
        assert!(l.contains("name"));
    }

    #[test]
    fn layers_parse_and_duplicates_are_rejected() {
        let text = r#"
[[layer]]
name = "low"
crates = ["a", "b"]
[[layer]]
name = "high"
crates = ["c"]
[[forbid]]
from = "c"
to = "a"
adr = "0001"
reason = "r"
[[forbid_crate]]
name = "old"
adr = "0001"
reason = "r"
[closure]
gui_crates = ["c"]
forbidden = ["gpui*"]
"#;
        let l = Layers::parse(text).unwrap();
        assert_eq!(l.layer_of("c"), Some(1));
        assert_eq!(l.layer_of("zzz"), None);
        assert_eq!(l.forbid.len(), 1);
        assert_eq!(l.closure.forbidden, ["gpui*"]);
        let dup = "[[layer]]\nname=\"x\"\ncrates=[\"a\"]\n[[layer]]\nname=\"y\"\ncrates=[\"a\"]\n";
        assert!(Layers::parse(dup).is_err());
        assert!(Layers::parse("[[forbid]]\nfrom=\"a\"\n").is_err());
    }
}
