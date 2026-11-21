//! Public-API comparison of a file diff: exported items of both sides (tree-sitter
//! outline), compared by signature or whole definition.

use std::collections::{BTreeMap, HashSet};
use std::ops::Range;

use lector_syntax::{Language, OutlineItem, SymbolKind, Syntax};
use lector_text::Text;

use super::{At, Builder, Param, RiskKind, RiskWeights};
use crate::review::{DiffSide, FileDiff};

#[derive(Debug, Clone)]
struct ApiItem {
    key: String,
    label: &'static str,
    display: String,
    signature: String,
    /// Rows of the item (its first lines), in the text of its own side.
    rows: Range<u32>,
}

fn is_whole_text_kind(k: SymbolKind) -> bool {
    matches!(
        k,
        SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Interface
            | SymbolKind::Type
            | SymbolKind::Constant
            | SymbolKind::Variable
            | SymbolKind::Field
    )
}

fn strip_comments(s: &str, c_style: bool) -> String {
    if !c_style {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte length of the header of `item` (up to the body).
fn header_len(item: &str, python: bool) -> usize {
    let (mut paren, mut bracket) = (0i32, 0i32);
    let b = item.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'[' => bracket += 1,
            b']' => bracket -= 1,
            b'{' | b';' if paren <= 0 && bracket <= 0 && !python => return i,
            b':' if python && paren <= 0 && bracket <= 0 => return i,
            b'=' if paren <= 0 && bracket <= 0 && b.get(i + 1) == Some(&b'>') => return i,
            _ => {}
        }
    }
    item.len()
}

fn first_word_is(s: &str, words: &[&str]) -> bool {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| words.contains(&w))
}

fn rust_pub(head: &str) -> bool {
    let mut h = head.trim_start();
    while h.starts_with("#[") {
        match h.find(']') {
            Some(e) => h = h[e + 1..].trim_start(),
            None => break,
        }
    }
    h.strip_prefix("pub")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn api_items(lang: &Language, syntax: &Syntax, text: &Text, src: &str) -> Vec<ApiItem> {
    let outline = syntax.outline(text);
    let mut out: Vec<ApiItem> = Vec::new();
    let mut stack: Vec<(usize, &OutlineItem, bool)> = Vec::new(); // (index, item, exported)
    let name = lang.name.as_str();
    let c_style = !matches!(name, "Python" | "Ruby");
    for (ix, item) in outline.iter().enumerate() {
        while stack.last().is_some_and(|(_, p, _)| p.depth >= item.depth) {
            stack.pop();
        }
        let parent = stack.last().copied();
        let range = item.range.clone();
        let Some(item_src) = src.get(range.clone()) else {
            continue;
        };
        let line_start = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let prefix = &src[line_start..range.start];
        let python = name == "Python";
        let hl = if name == "Ruby" {
            item_src.find('\n').unwrap_or(item_src.len())
        } else {
            header_len(item_src, python)
        };
        let header = format!("{prefix}{}", &item_src[..hl]);
        let has_methods = outline[ix + 1..]
            .iter()
            .take_while(|n| n.depth > item.depth)
            .any(|n| matches!(n.kind, SymbolKind::Method | SymbolKind::Function));
        let whole = is_whole_text_kind(item.kind) && !has_methods;

        // Local items (inside function bodies) are not API; fields of whole-text
        // parents are part of the parent's definition.
        let inside_fn = stack
            .iter()
            .any(|(_, p, _)| matches!(p.kind, SymbolKind::Function | SymbolKind::Method));
        let parent_whole = stack.iter().any(|(pi, p, _)| {
            is_whole_text_kind(p.kind)
                && !outline[pi + 1..]
                    .iter()
                    .take_while(|n| n.depth > p.depth)
                    .any(|n| matches!(n.kind, SymbolKind::Method | SymbolKind::Function))
        });
        let parent_exported = parent.map(|(_, _, e)| e);
        let inherits = matches!(
            parent.map(|(_, p, _)| p.kind),
            Some(SymbolKind::Trait | SymbolKind::Interface)
        ) || (matches!(parent.map(|(_, p, _)| p.kind), Some(SymbolKind::Enum))
            && item.kind == SymbolKind::Field);

        let exported_own = match name {
            "Rust" => rust_pub(&header),
            "TypeScript" | "TSX" | "JavaScript" => {
                let h = header.trim_start();
                if parent.is_some_and(|(_, p, _)| matches!(p.kind, SymbolKind::Class)) {
                    !first_word_is(
                        &header[..header.find(&item.name).unwrap_or(header.len())],
                        &["private", "protected"],
                    ) && !item.name.starts_with('#')
                } else {
                    h.starts_with("export")
                }
            }
            "Python" => {
                !item.name.starts_with('_')
                    || (item.name.starts_with("__") && item.name.ends_with("__"))
            }
            "Go" => item.name.chars().next().is_some_and(char::is_uppercase),
            "Java" | "C#" => first_word_is(&header, &["public", "protected"]),
            "Swift" => first_word_is(&header, &["public", "open"]),
            "Kotlin" => !first_word_is(&header, &["private", "internal", "protected"]),
            "PHP" => !first_word_is(&header, &["private", "protected"]),
            "C" | "C++" => !first_word_is(&header, &["static"]),
            "Ruby" => !item.name.starts_with('_'),
            _ => false,
        };
        let exported = if inherits {
            parent_exported.unwrap_or(false)
        } else if let Some(pe) = parent_exported {
            // Members need an exported container (except Go, where names decide, and Rust `impl` blocks).
            let container_kind = parent.map(|(_, p, _)| p.kind);
            if name == "Go" || (name == "Rust" && container_kind == Some(SymbolKind::Other)) {
                exported_own
            } else {
                exported_own && pe
            }
        } else {
            exported_own
        };
        stack.push((ix, item, exported));
        if !exported || inside_fn || parent_whole {
            continue;
        }

        let sig_src = if whole { item_src } else { &item_src[..hl] };
        let signature = normalize_ws(&strip_comments(sig_src, c_style));
        let qualified: Vec<&str> = stack
            .iter()
            .take(stack.len() - 1)
            .map(|(_, p, _)| p.name.as_str())
            .chain(std::iter::once(item.name.as_str()))
            .collect();
        let display = qualified.join("::");
        out.push(ApiItem {
            key: format!("{}:{display}", item.kind.label()),
            label: item.kind.label(),
            display,
            signature,
            rows: {
                let first = text.offset_to_point(item.range.start).row;
                let last = text.offset_to_point(item.range.end).row;
                first..(last + 1).min(first + 8)
            },
        });
    }
    out
}

fn side_api(diff_side: &DiffSide, lang: &Language, src: &str) -> Vec<ApiItem> {
    if !diff_side.exists {
        return Vec::new();
    }
    let (Some(text), Some(syntax)) = (&diff_side.text, &diff_side.syntax) else {
        return Vec::new();
    };
    api_items(lang, syntax, text, src)
}

fn clip(s: &str) -> String {
    if s.chars().count() > 90 {
        let cut: String = s.chars().take(87).collect();
        format!("{cut}...")
    } else {
        s.to_string()
    }
}

pub(super) fn api_reasons(
    b: &mut Builder,
    lang: &Language,
    diff: &FileDiff,
    old_src: &str,
    new_src: &str,
) {
    // Both sides must have been parsed (or be absent).
    if (diff.old.exists && diff.old.syntax.is_none())
        || (diff.new.exists && diff.new.syntax.is_none())
    {
        return;
    }
    let old = side_api(&diff.old, lang, old_src);
    let new = side_api(&diff.new, lang, new_src);
    let mut old_by: BTreeMap<&str, Vec<&ApiItem>> = BTreeMap::new();
    for i in &old {
        old_by.entry(i.key.as_str()).or_default().push(i);
    }
    let mut new_by: BTreeMap<&str, Vec<&ApiItem>> = BTreeMap::new();
    for i in &new {
        new_by.entry(i.key.as_str()).or_default().push(i);
    }
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let keys: HashSet<&str> = old_by.keys().chain(new_by.keys()).copied().collect();
    let mut keys: Vec<&str> = keys.into_iter().collect();
    keys.sort_unstable();
    for k in keys {
        match (old_by.get(k), new_by.get(k)) {
            (Some(o), None) => removed.extend(o.iter().copied()),
            (None, Some(n)) => added.extend(n.iter().copied()),
            (Some(o), Some(n)) => {
                let o_sigs: Vec<&str> = o.iter().map(|i| i.signature.as_str()).collect();
                let n_sigs: Vec<&str> = n.iter().map(|i| i.signature.as_str()).collect();
                let mut o_left: Vec<&ApiItem> = o
                    .iter()
                    .copied()
                    .filter(|i| !n_sigs.contains(&i.signature.as_str()))
                    .collect();
                let mut n_left: Vec<&ApiItem> = n
                    .iter()
                    .copied()
                    .filter(|i| !o_sigs.contains(&i.signature.as_str()))
                    .collect();
                while !o_left.is_empty() && !n_left.is_empty() {
                    changed.push((o_left.remove(0), n_left.remove(0)));
                }
                removed.extend(o_left);
                added.extend(n_left);
            }
            (None, None) => {}
        }
    }
    const SHOWN: usize = 5;
    for i in removed.iter().take(SHOWN) {
        b.add_at(
            RiskKind::PublicApiRemoved,
            param!(api_removed),
            1.0,
            format!("Removes public {} `{}`", i.label, i.display),
            At::Old,
            i.rows.clone(),
        );
    }
    for (o, n) in changed.iter().take(SHOWN) {
        b.add_at(
            RiskKind::PublicApiChanged,
            param!(api_changed),
            1.0,
            format!(
                "Changes the signature of public {} `{}`: `{}` -> `{}`",
                n.label,
                n.display,
                clip(&o.signature),
                clip(&n.signature)
            ),
            At::New,
            n.rows.clone(),
        );
    }
    // New exports are additive and rarely break callers: one reason for all of them,
    // growing slowly with their number.
    if let Some(first) = added.first() {
        let names: Vec<String> = added
            .iter()
            .take(3)
            .map(|i| format!("{} `{}`", i.label, i.display))
            .collect();
        let more = added.len().saturating_sub(3);
        let list = if more > 0 {
            format!("{} and {more} more", names.join(", "))
        } else {
            names.join(", ")
        };
        b.add_at(
            RiskKind::PublicApiAdded,
            param!(api_added),
            1.0 + 0.1 * (added.len() - 1).min(4) as f32,
            format!("Adds public {list}"),
            At::New,
            first.rows.clone(),
        );
    }
    let extra = |total: usize| total.saturating_sub(SHOWN);
    if extra(removed.len()) > 0 {
        b.add(
            RiskKind::PublicApiRemoved,
            param!(api_removed),
            1.0,
            format!("... and {} more public items removed", extra(removed.len())),
        );
    }
    if extra(changed.len()) > 0 {
        b.add(
            RiskKind::PublicApiChanged,
            param!(api_changed),
            1.0,
            format!(
                "... and {} more public signatures changed",
                extra(changed.len())
            ),
        );
    }
}
