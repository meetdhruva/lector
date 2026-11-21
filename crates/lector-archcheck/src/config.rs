//! Checked-in configuration: `arch/layers.toml` and the `arch/*.allow` files.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

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

/// A reviewed allowance list (`arch/*.allow`).
///
/// One entry per line; `#` starts a comment. Every entry needs a reason: an inline `# reason`
/// or a comment line above it in the same block (blocks are separated by blank lines).
/// Plain entries are path globs or item names (depending on the file); `tag:value` entries
/// (`kind:pty-reader`, `literal:settings.toml`, `crate:lector-text`) carry a typed value.
#[derive(Debug, Clone, Default)]
pub struct AllowList {
    entries: Vec<String>,
}

impl AllowList {
    /// Parse `text`; `name` is only used in error messages.
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
            entries.push(entry.to_string());
        }
        Ok(AllowList { entries })
    }

    /// Build a list directly from entries (tests).
    pub fn from_entries<I: IntoIterator<Item = S>, S: Into<String>>(entries: I) -> AllowList {
        AllowList {
            entries: entries.into_iter().map(Into::into).collect(),
        }
    }

    /// True when a plain entry (glob) matches the repository-relative `path`.
    pub fn matches_path(&self, path: &str) -> bool {
        self.entries
            .iter()
            .filter(|e| !e.contains(':'))
            .any(|e| glob_match(e, path))
    }

    /// True when a plain entry equals `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.entries.iter().any(|e| e == name)
    }

    /// Values of the `tag:value` entries with this tag.
    pub fn tagged(&self, tag: &str) -> Vec<&str> {
        self.entries
            .iter()
            .filter_map(|e| e.strip_prefix(tag).and_then(|r| r.strip_prefix(':')))
            .collect()
    }

    /// All plain entries.
    pub fn plain(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|e| !e.contains(':'))
            .map(String::as_str)
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
];

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
