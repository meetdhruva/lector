//! Conversions between `lsp-types` / raw JSON and Lector's own types.

use std::ops::Range;
use std::path::{Path, PathBuf};

use lsp_types as lsp;
use serde_json::Value;

use crate::error::{LspError, Result};
use crate::types::{DocumentSymbol, HoverInfo, Location, SymbolKind, WorkspaceSymbol};
use crate::PointUtf16;

/// `file://` URI string for `path`.
pub(crate) fn path_to_uri(path: &Path) -> Option<String> {
    url::Url::from_file_path(path)
        .or_else(|_| url::Url::from_directory_path(path))
        .ok()
        .map(Into::into)
}

/// Local path of a `file://` URI string.
pub(crate) fn uri_to_path(uri: &str) -> Option<PathBuf> {
    url::Url::parse(uri).ok()?.to_file_path().ok()
}

fn lsp_uri_to_path(uri: &lsp::Uri) -> Option<PathBuf> {
    uri_to_path(uri.as_str())
}

fn pt(p: lsp::Position) -> PointUtf16 {
    PointUtf16 {
        row: p.line,
        column: p.character,
    }
}

fn rng(r: lsp::Range) -> Range<PointUtf16> {
    pt(r.start)..pt(r.end)
}

fn decode<T: serde::de::DeserializeOwned>(v: Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| LspError::Protocol(e.to_string()))
}

fn marked(m: lsp::MarkedString) -> String {
    match m {
        lsp::MarkedString::String(s) => s,
        lsp::MarkedString::LanguageString(l) => format!("```{}\n{}\n```", l.language, l.value),
    }
}

pub(crate) fn hover(v: Value) -> Result<Option<HoverInfo>> {
    if v.is_null() {
        return Ok(None);
    }
    let h: lsp::Hover = decode(v)?;
    let markdown = match h.contents {
        lsp::HoverContents::Scalar(m) => marked(m),
        lsp::HoverContents::Array(ms) => {
            ms.into_iter().map(marked).collect::<Vec<_>>().join("\n\n")
        }
        lsp::HoverContents::Markup(m) => m.value,
    };
    if markdown.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(HoverInfo {
        markdown,
        range: h.range.map(rng),
    }))
}

pub(crate) fn locations(v: Value) -> Result<Vec<Location>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    // Definition responses may be Location | Location[] | LocationLink[].
    let resp: lsp::GotoDefinitionResponse = decode(v)?;
    let loc = |uri: &lsp::Uri, r: lsp::Range| {
        lsp_uri_to_path(uri).map(|path| Location {
            path,
            range: rng(r),
        })
    };
    Ok(match resp {
        lsp::GotoDefinitionResponse::Scalar(l) => loc(&l.uri, l.range).into_iter().collect(),
        lsp::GotoDefinitionResponse::Array(ls) => {
            ls.iter().filter_map(|l| loc(&l.uri, l.range)).collect()
        }
        lsp::GotoDefinitionResponse::Link(ls) => ls
            .iter()
            .filter_map(|l| loc(&l.target_uri, l.target_selection_range))
            .collect(),
    })
}

fn kind(k: lsp::SymbolKind) -> SymbolKind {
    use SymbolKind::*;
    let n = serde_json::to_value(k)
        .ok()
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    match n {
        1 => File,
        2 => Module,
        3 => Namespace,
        4 => Package,
        5 => Class,
        6 => Method,
        7 => Property,
        8 => Field,
        9 => Constructor,
        10 => Enum,
        11 => Interface,
        12 => Function,
        13 => Variable,
        14 => Constant,
        15 => String,
        16 => Number,
        17 => Boolean,
        18 => Array,
        19 => Object,
        20 => Key,
        21 => Null,
        22 => EnumMember,
        23 => Struct,
        24 => Event,
        25 => Operator,
        26 => TypeParameter,
        _ => Other,
    }
}

fn nested(s: lsp::DocumentSymbol) -> DocumentSymbol {
    DocumentSymbol {
        name: s.name,
        detail: s.detail,
        kind: kind(s.kind),
        range: rng(s.range),
        selection_range: rng(s.selection_range),
        children: s
            .children
            .unwrap_or_default()
            .into_iter()
            .map(nested)
            .collect(),
    }
}

pub(crate) fn document_symbols(v: Value) -> Result<Vec<DocumentSymbol>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    let resp: lsp::DocumentSymbolResponse = decode(v)?;
    Ok(match resp {
        lsp::DocumentSymbolResponse::Nested(v) => v.into_iter().map(nested).collect(),
        lsp::DocumentSymbolResponse::Flat(v) => {
            // Rebuild hierarchy from container names is unreliable; keep flat.
            v.into_iter()
                .map(|s| DocumentSymbol {
                    name: s.name,
                    detail: s.container_name,
                    kind: kind(s.kind),
                    range: rng(s.location.range),
                    selection_range: rng(s.location.range),
                    children: vec![],
                })
                .collect()
        }
    })
}

pub(crate) fn workspace_symbols(v: Value) -> Result<Vec<WorkspaceSymbol>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    let resp: lsp::WorkspaceSymbolResponse = decode(v)?;
    Ok(match resp {
        lsp::WorkspaceSymbolResponse::Flat(v) => v
            .into_iter()
            .filter_map(|s| {
                Some(WorkspaceSymbol {
                    name: s.name,
                    kind: kind(s.kind),
                    path: lsp_uri_to_path(&s.location.uri)?,
                    range: rng(s.location.range),
                    container: s.container_name,
                })
            })
            .collect(),
        lsp::WorkspaceSymbolResponse::Nested(v) => v
            .into_iter()
            .filter_map(|s| {
                let (uri, range) = match s.location {
                    lsp::OneOf::Left(l) => (l.uri, rng(l.range)),
                    lsp::OneOf::Right(w) => (w.uri, PointUtf16::default()..PointUtf16::default()),
                };
                Some(WorkspaceSymbol {
                    name: s.name,
                    kind: kind(s.kind),
                    path: lsp_uri_to_path(&uri)?,
                    range,
                    container: s.container_name,
                })
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn hover_variants() {
        let h = hover(json!({"contents": {"kind": "markdown", "value": "**x**"},
            "range": {"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 5}}}))
        .unwrap()
        .unwrap();
        assert_eq!(h.markdown, "**x**");
        assert_eq!(h.range.unwrap().end, PointUtf16 { row: 1, column: 5 });
        let h = hover(json!({"contents": [{"language": "rust", "value": "fn a()"}, "doc"]}))
            .unwrap()
            .unwrap();
        assert_eq!(h.markdown, "```rust\nfn a()\n```\n\ndoc");
        let h = hover(json!({"contents": "plain"})).unwrap().unwrap();
        assert_eq!(h.markdown, "plain");
        assert!(hover(Value::Null).unwrap().is_none());
        assert!(hover(json!({"contents": ""})).unwrap().is_none());
    }

    #[test]
    fn location_variants() {
        // File URIs must name absolute paths on the host platform.
        #[cfg(windows)]
        let (uri_b, path_b, uri_c, path_c) = (
            "file:///C:/a/b.rs",
            PathBuf::from(r"C:\a\b.rs"),
            "file:///C:/c.rs",
            PathBuf::from(r"C:\c.rs"),
        );
        #[cfg(not(windows))]
        let (uri_b, path_b, uri_c, path_c) = (
            "file:///a/b.rs",
            PathBuf::from("/a/b.rs"),
            "file:///c.rs",
            PathBuf::from("/c.rs"),
        );
        let r = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 1}});
        let one = locations(json!({"uri": uri_b, "range": r})).unwrap();
        assert_eq!(one[0].path, path_b);
        let link = locations(json!([{"targetUri": uri_c, "targetRange": r,
            "targetSelectionRange": r}]))
        .unwrap();
        assert_eq!(link[0].path, path_c);
        assert!(locations(Value::Null).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn uri_roundtrip_with_spaces() {
        let p = PathBuf::from("/tmp/a b/c#d.rs");
        let u = path_to_uri(&p).unwrap();
        assert!(<lsp::Uri as std::str::FromStr>::from_str(&u).is_ok());
        assert_eq!(uri_to_path(&u).unwrap(), p);
    }
}
