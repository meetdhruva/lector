//! Conversions between `lsp-types` / raw JSON and Lector's own types.

use std::ops::Range;
use std::path::{Path, PathBuf};

use lsp_types as lsp;
use serde_json::Value;

use crate::error::{LspError, Result};
use crate::types::{
    CodeAction, Command, Diagnostic, DiagnosticSeverity, DiagnosticTag, DocumentSymbol, HoverInfo,
    Location, PrepareRename, ServerCapabilities, SymbolKind, TextEdit, WorkspaceEdit,
    WorkspaceSymbol,
};
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

// Lenient raw-JSON conversions for the editing features (servers vary in
// which optional shapes they use; unknown fields are ignored).

fn json_point(v: &Value) -> Option<PointUtf16> {
    Some(PointUtf16 {
        row: u32::try_from(v.get("line")?.as_u64()?).ok()?,
        column: u32::try_from(v.get("character")?.as_u64()?).ok()?,
    })
}

/// A JSON `Range`.
pub(crate) fn json_range(v: &Value) -> Option<Range<PointUtf16>> {
    let start = json_point(v.get("start")?)?;
    let end = json_point(v.get("end")?)?;
    Some(if end < start { end..start } else { start..end })
}

/// JSON `Range` for `range`.
pub(crate) fn range_json(range: &Range<PointUtf16>) -> Value {
    serde_json::json!({
        "start": {"line": range.start.row, "character": range.start.column},
        "end": {"line": range.end.row, "character": range.end.column},
    })
}

fn str_field(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

/// `TextEdit[]` (also `AnnotatedTextEdit`, `SnippetTextEdit` text as is).
pub(crate) fn text_edits(v: &Value) -> Result<Vec<TextEdit>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    let arr = v
        .as_array()
        .ok_or_else(|| LspError::Protocol("expected an array of text edits".into()))?;
    arr.iter()
        .map(|e| {
            let range = e
                .get("range")
                .and_then(json_range)
                .ok_or_else(|| LspError::Protocol("text edit without a range".into()))?;
            let new_text = e
                .get("newText")
                .and_then(Value::as_str)
                .or_else(|| {
                    e.get("snippet")
                        .and_then(|s| s.get("value"))
                        .and_then(Value::as_str)
                })
                .ok_or_else(|| LspError::Protocol("text edit without newText".into()))?
                .to_string();
            Ok(TextEdit { range, new_text })
        })
        .collect()
}

fn push_file_edits(out: &mut WorkspaceEdit, path: PathBuf, mut edits: Vec<TextEdit>) {
    match out.changes.iter_mut().find(|(p, _)| *p == path) {
        Some((_, existing)) => existing.append(&mut edits),
        None => out.changes.push((path, edits)),
    }
}

/// `WorkspaceEdit`. Resource operations (create / rename / delete files) are
/// not supported (the client does not advertise them) and are an error.
pub(crate) fn workspace_edit(v: &Value) -> Result<WorkspaceEdit> {
    let mut out = WorkspaceEdit::default();
    if v.is_null() {
        return Ok(out);
    }
    let bad_uri = |u: &str| LspError::Protocol(format!("unsupported document URI: {u}"));
    if let Some(changes) = v.get("documentChanges").and_then(Value::as_array) {
        for change in changes {
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                return Err(LspError::Protocol(format!(
                    "file operation `{kind}` is not supported"
                )));
            }
            let doc = change
                .get("textDocument")
                .ok_or_else(|| LspError::Protocol("document change without a document".into()))?;
            let uri = doc.get("uri").and_then(Value::as_str).unwrap_or_default();
            let path = uri_to_path(uri).ok_or_else(|| bad_uri(uri))?;
            let edits = text_edits(change.get("edits").unwrap_or(&Value::Null))?;
            if let Some(version) = doc.get("version").and_then(Value::as_i64) {
                if let Ok(version) = i32::try_from(version) {
                    if !out.versions.iter().any(|(p, _)| *p == path) {
                        out.versions.push((path.clone(), version));
                    }
                }
            }
            push_file_edits(&mut out, path, edits);
        }
    } else if let Some(changes) = v.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let path = uri_to_path(uri).ok_or_else(|| bad_uri(uri))?;
            push_file_edits(&mut out, path, text_edits(edits)?);
        }
    }
    Ok(out)
}

fn severity(v: Option<&Value>) -> DiagnosticSeverity {
    match v.and_then(Value::as_i64) {
        Some(2) => DiagnosticSeverity::Warning,
        Some(3) => DiagnosticSeverity::Information,
        Some(4) => DiagnosticSeverity::Hint,
        _ => DiagnosticSeverity::Error,
    }
}

/// One JSON `Diagnostic`.
pub(crate) fn diagnostic(v: &Value) -> Option<Diagnostic> {
    let range = json_range(v.get("range")?)?;
    let code = match v.get("code") {
        Some(Value::String(s)) => Some(s.clone()),
        Some(Value::Number(n)) => Some(n.to_string()),
        _ => None,
    };
    let tags = v
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(|t| match t.as_i64() {
                    Some(1) => Some(DiagnosticTag::Unnecessary),
                    Some(2) => Some(DiagnosticTag::Deprecated),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    let related = v
        .get("relatedInformation")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|r| {
                    let loc = r.get("location")?;
                    let path = uri_to_path(loc.get("uri")?.as_str()?)?;
                    let range = json_range(loc.get("range")?)?;
                    Some((Location { path, range }, str_field(r, "message")?))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Diagnostic {
        range,
        severity: severity(v.get("severity")),
        message: str_field(v, "message").unwrap_or_default(),
        source: str_field(v, "source"),
        code,
        tags,
        related,
    })
}

/// `textDocument/publishDiagnostics` params: path, version and the
/// diagnostics paired with their raw JSON (needed for code action contexts).
#[allow(clippy::type_complexity)]
pub(crate) fn publish_diagnostics(
    params: &Value,
) -> Option<(PathBuf, Option<i32>, Vec<(Diagnostic, Value)>)> {
    let path = uri_to_path(params.get("uri")?.as_str()?)?;
    let version = params
        .get("version")
        .and_then(Value::as_i64)
        .and_then(|v| i32::try_from(v).ok());
    let diags = params
        .get("diagnostics")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|d| diagnostic(d).map(|c| (c, d.clone())))
                .collect()
        })
        .unwrap_or_default();
    Some((path, version, diags))
}

pub(crate) fn command(v: &Value) -> Option<Command> {
    Some(Command {
        title: str_field(v, "title").unwrap_or_default(),
        command: str_field(v, "command")?,
        arguments: v
            .get("arguments")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default(),
    })
}

/// One `CodeAction` or bare `Command`.
pub(crate) fn code_action(v: &Value) -> Result<CodeAction> {
    let title = str_field(v, "title")
        .ok_or_else(|| LspError::Protocol("code action without a title".into()))?;
    // A bare Command has a string `command` field.
    if v.get("command").is_some_and(Value::is_string) {
        return Ok(CodeAction {
            title,
            kind: None,
            is_preferred: false,
            disabled_reason: None,
            edit: None,
            command: command(v),
            diagnostics: vec![],
            raw: v.clone(),
        });
    }
    let edit = match v.get("edit") {
        Some(e) if !e.is_null() => Some(workspace_edit(e)?),
        _ => None,
    };
    Ok(CodeAction {
        title,
        kind: str_field(v, "kind"),
        is_preferred: v
            .get("isPreferred")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        disabled_reason: v
            .get("disabled")
            .and_then(|d| d.get("reason"))
            .and_then(Value::as_str)
            .map(str::to_string),
        edit,
        command: v.get("command").and_then(command),
        diagnostics: v
            .get("diagnostics")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(diagnostic).collect())
            .unwrap_or_default(),
        raw: v.clone(),
    })
}

/// `(Command | CodeAction)[] | null`. Entries that cannot be understood
/// (e.g. unsupported file operations) are skipped.
pub(crate) fn code_actions(v: &Value) -> Result<Vec<CodeAction>> {
    if v.is_null() {
        return Ok(vec![]);
    }
    let arr = v
        .as_array()
        .ok_or_else(|| LspError::Protocol("expected an array of code actions".into()))?;
    Ok(arr
        .iter()
        .filter_map(|a| match code_action(a) {
            Ok(a) => Some(a),
            Err(e) => {
                tracing::debug!("skipping code action: {e}");
                None
            }
        })
        .collect())
}

/// `Range | { range, placeholder } | { defaultBehavior } | null`.
pub(crate) fn prepare_rename(v: &Value) -> Option<PrepareRename> {
    if v.is_null() {
        return None;
    }
    if let Some(range) = json_range(v) {
        return Some(PrepareRename {
            range: Some(range),
            placeholder: None,
        });
    }
    if let Some(range) = v.get("range").and_then(json_range) {
        return Some(PrepareRename {
            range: Some(range),
            placeholder: str_field(v, "placeholder"),
        });
    }
    v.get("defaultBehavior")
        .and_then(Value::as_bool)
        .filter(|b| *b)
        .map(|_| PrepareRename {
            range: None,
            placeholder: None,
        })
}

fn provider(v: Option<&Value>) -> bool {
    match v {
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

/// Summary of an `initialize` result's `capabilities`.
pub(crate) fn server_capabilities(init_result: &Value) -> ServerCapabilities {
    let caps = init_result.get("capabilities").unwrap_or(&Value::Null);
    let get = |k: &str| caps.get(k);
    ServerCapabilities {
        hover: provider(get("hoverProvider")),
        definition: provider(get("definitionProvider")),
        references: provider(get("referencesProvider")),
        formatting: provider(get("documentFormattingProvider")),
        range_formatting: provider(get("documentRangeFormattingProvider")),
        rename: provider(get("renameProvider")),
        prepare_rename: get("renameProvider")
            .and_then(|r| r.get("prepareProvider"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        code_actions: provider(get("codeActionProvider")),
        code_action_resolve: get("codeActionProvider")
            .and_then(|r| r.get("resolveProvider"))
            .and_then(Value::as_bool)
            .unwrap_or(false),
        commands: get("executeCommandProvider")
            .and_then(|e| e.get("commands"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|c| c.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        completion: crate::completion::completion_capabilities(caps),
        signature_help: crate::completion::signature_capabilities(caps),
    }
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

    #[cfg(unix)]
    #[test]
    fn workspace_edit_shapes() {
        let r = json!({"start": {"line": 1, "character": 2}, "end": {"line": 1, "character": 4}});
        let e =
            workspace_edit(&json!({"changes": {"file:///a.rs": [{"range": r, "newText": "x"}]}}))
                .unwrap();
        assert_eq!(e.changes.len(), 1);
        assert_eq!(e.changes[0].0, PathBuf::from("/a.rs"));
        assert_eq!(e.changes[0].1[0].new_text, "x");
        assert!(e.versions.is_empty());
        let e = workspace_edit(&json!({"documentChanges": [
            {"textDocument": {"uri": "file:///a.rs", "version": 3}, "edits": [{"range": r, "newText": "y"}]},
            {"textDocument": {"uri": "file:///b.rs", "version": null},
             "edits": [{"range": r, "newText": "z", "annotationId": "q"}]}]}))
        .unwrap();
        assert_eq!(e.file_count(), 2);
        assert_eq!(e.version_for(Path::new("/a.rs")), Some(3));
        assert_eq!(e.version_for(Path::new("/b.rs")), None);
        assert!(workspace_edit(
            &json!({"documentChanges": [{"kind": "create", "uri": "file:///c"}]})
        )
        .is_err());
        assert!(workspace_edit(&Value::Null).unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn diagnostics_and_actions() {
        let r = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 3}});
        let (path, version, diags) = publish_diagnostics(&json!({"uri": "file:///a.py", "version": 4,
            "diagnostics": [
                {"range": r, "severity": 2, "message": "m", "code": 12, "source": "s", "tags": [1],
                 "relatedInformation": [{"location": {"uri": "file:///b.py", "range": r}, "message": "here"}]},
                {"range": r, "message": "no severity"}]}))
        .unwrap();
        assert_eq!(path, PathBuf::from("/a.py"));
        assert_eq!(version, Some(4));
        assert_eq!(diags[0].0.severity, DiagnosticSeverity::Warning);
        assert_eq!(diags[0].0.code.as_deref(), Some("12"));
        assert_eq!(diags[0].0.tags, vec![DiagnosticTag::Unnecessary]);
        assert_eq!(diags[0].0.related[0].1, "here");
        assert_eq!(diags[1].0.severity, DiagnosticSeverity::Error);
        let actions = code_actions(&json!([
            {"title": "cmd", "command": "do.it", "arguments": [1]},
            {"title": "fix", "kind": "quickfix", "isPreferred": true,
             "edit": {"changes": {"file:///a.py": [{"range": r, "newText": "x"}]}}},
            {"title": "nope", "kind": "refactor", "disabled": {"reason": "why"}},
            {"title": "bad", "edit": {"documentChanges": [{"kind": "delete", "uri": "file:///a"}]}}
        ]))
        .unwrap();
        assert_eq!(actions.len(), 3);
        assert_eq!(actions[0].command.as_ref().unwrap().command, "do.it");
        assert!(actions[1].is_quick_fix() && actions[1].is_preferred);
        assert_eq!(actions[2].disabled_reason.as_deref(), Some("why"));
    }

    #[test]
    fn prepare_rename_and_capabilities() {
        let r = json!({"start": {"line": 0, "character": 1}, "end": {"line": 0, "character": 3}});
        assert_eq!(prepare_rename(&r).unwrap().range.unwrap().end.column, 3);
        let p = prepare_rename(&json!({"range": r, "placeholder": "ab"})).unwrap();
        assert_eq!(p.placeholder.as_deref(), Some("ab"));
        assert_eq!(
            prepare_rename(&json!({"defaultBehavior": true}))
                .unwrap()
                .range,
            None
        );
        assert!(prepare_rename(&Value::Null).is_none());
        let caps = server_capabilities(&json!({"capabilities": {
            "documentFormattingProvider": true, "renameProvider": {"prepareProvider": true},
            "codeActionProvider": {"resolveProvider": true}, "executeCommandProvider": {"commands": ["a"]}}}));
        assert!(caps.formatting && !caps.range_formatting && caps.rename && caps.prepare_rename);
        assert!(caps.code_actions && caps.code_action_resolve);
        assert_eq!(caps.commands, vec!["a".to_string()]);
    }
}
