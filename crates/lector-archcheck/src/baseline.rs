//! `arch/baseline.toml`: rule to key to count.
//!
//! The file is rendered by this module only (sorted, quoted keys, LF), so `--update` produces
//! the same bytes on every platform.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use crate::error::{ArchError, Result};

/// Section order in the rendered file; unknown rules follow alphabetically.
pub const RULE_ORDER: &[&str] = &[
    "deps",
    "writes",
    "threads",
    "background",
    "state",
    "dupes",
    "size",
    "purity",
    "hygiene",
];

const HEADER: &str = "\
# Architecture ratchet baseline (see arch/README.md and docs/adr/0008-guardrails.md).
#
# rule -> key -> count. Keys are files, except `deps` (crate edges) and `size` (file -> lines).
# Numbers may only go down. Never raise one: fix the code, or write an ADR.
# Lower entries with `scripts/arch-check --update`, which refuses to raise anything.
";

/// The parsed baseline.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Baseline {
    /// Rule name to (key to count).
    pub rules: BTreeMap<String, BTreeMap<String, u64>>,
}

impl Baseline {
    /// Entries of `rule` (empty when absent).
    pub fn entries(&self, rule: &str) -> BTreeMap<String, u64> {
        self.rules.get(rule).cloned().unwrap_or_default()
    }

    /// Parse `arch/baseline.toml`.
    pub fn parse(text: &str) -> Result<Baseline> {
        let doc: toml::Table = text
            .parse()
            .map_err(|e| ArchError::Config(format!("arch/baseline.toml: {e}")))?;
        let mut rules = BTreeMap::new();
        for (rule, value) in doc {
            let table = value.as_table().ok_or_else(|| {
                ArchError::Config(format!("arch/baseline.toml: `{rule}` must be a table"))
            })?;
            let mut entries = BTreeMap::new();
            for (key, count) in table {
                let n = count
                    .as_integer()
                    .and_then(|n| u64::try_from(n).ok())
                    .ok_or_else(|| {
                        ArchError::Config(format!(
                            "arch/baseline.toml: [{rule}] `{key}` must be a non-negative integer"
                        ))
                    })?;
                entries.insert(key.clone(), n);
            }
            rules.insert(rule, entries);
        }
        Ok(Baseline { rules })
    }

    /// Load `<root>/arch/baseline.toml`; `None` when the file does not exist.
    pub fn load(root: &Path) -> Result<Option<Baseline>> {
        match fs::read_to_string(root.join("arch/baseline.toml")) {
            Ok(text) => Ok(Some(Baseline::parse(&text.replace("\r\n", "\n"))?)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(ArchError::io("arch/baseline.toml", e)),
        }
    }

    /// Render the file contents.
    pub fn render(&self) -> String {
        let mut out = String::from(HEADER);
        let mut names: Vec<&String> = self.rules.keys().collect();
        names.sort_by_key(|n| {
            (
                RULE_ORDER
                    .iter()
                    .position(|r| r == n)
                    .unwrap_or(RULE_ORDER.len()),
                (*n).clone(),
            )
        });
        for name in names {
            out.push('\n');
            out.push_str(&format!("[{}]\n", bare_or_quoted(name)));
            for (key, count) in &self.rules[name] {
                out.push_str(&format!("{} = {count}\n", quote(key)));
            }
        }
        out
    }

    /// Write `<root>/arch/baseline.toml`.
    pub fn save(&self, root: &Path) -> Result<()> {
        let dir = root.join("arch");
        fs::create_dir_all(&dir).map_err(|e| ArchError::io("arch", e))?;
        fs::write(dir.join("baseline.toml"), self.render())
            .map_err(|e| ArchError::io("arch/baseline.toml", e))
    }
}

fn bare_or_quoted(name: &str) -> String {
    if !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        name.to_string()
    } else {
        quote(name)
    }
}

fn quote(key: &str) -> String {
    let mut s = String::with_capacity(key.len() + 2);
    s.push('"');
    for c in key.chars() {
        match c {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            c if (c as u32) < 0x20 => s.push_str(&format!("\\u{:04x}", c as u32)),
            c => s.push(c),
        }
    }
    s.push('"');
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Baseline {
        let mut b = Baseline::default();
        b.rules.insert(
            "threads".into(),
            [("crates/a/src/x.rs".to_string(), 3u64)]
                .into_iter()
                .collect(),
        );
        b.rules.insert(
            "deps".into(),
            [("lector-ui-git -> lector-process".to_string(), 1u64)]
                .into_iter()
                .collect(),
        );
        b.rules.insert("size".into(), BTreeMap::new());
        b
    }

    #[test]
    fn render_parse_round_trip_and_ordering() {
        let text = sample().render();
        assert!(text.find("[deps]").unwrap() < text.find("[threads]").unwrap());
        assert!(text.find("[threads]").unwrap() < text.find("[size]").unwrap());
        assert!(text.contains("\"lector-ui-git -> lector-process\" = 1"));
        let back = Baseline::parse(&text).unwrap();
        assert_eq!(back, sample());
    }

    #[test]
    fn keys_with_quotes_and_backslashes_survive() {
        let mut b = Baseline::default();
        b.rules.insert(
            "state".into(),
            [("a\"b\\c.rs".to_string(), 2u64)].into_iter().collect(),
        );
        assert_eq!(Baseline::parse(&b.render()).unwrap(), b);
    }

    #[test]
    fn rejects_non_integers() {
        assert!(Baseline::parse("[threads]\n\"a\" = \"x\"\n").is_err());
        assert!(Baseline::parse("[threads]\n\"a\" = -1\n").is_err());
        assert!(Baseline::parse("threads = 3\n").is_err());
    }

    #[test]
    fn render_is_deterministic_lf() {
        let t = sample().render();
        assert!(!t.contains('\r'));
        assert_eq!(t, sample().render());
    }
}
