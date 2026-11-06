//! Document commands (Agent DOCS): open, read, edit, save, revert, lock and
//! list the live documents of the [`DocumentStore`] — the same documents the
//! editor tabs show, so an agent's edit appears in every open view and stays
//! unsaved (dirty) until saved.
//!
//! Outputs are compact: project-relative paths, 0-based lines, `version`
//! (pass it back as `expected_version` to make edits conditional).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail, Context as _};
use lector_buffer::TextEdit;
use lector_config::Settings;
use lector_workspace::Project;
use serde_json::{json, Value};

use super::schema::{object, prop};
use super::{Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome};
use crate::documents::{DocError, DocHandle, DocumentStore, SaveOutcome, SaveRequest};

/// Longest text returned by `document.read` without an explicit range.
const MAX_READ_LINES: u32 = 2000;

/// Registers the document commands.
pub fn register(r: &CommandRegistry) {
    r.register(open());
    r.register(read());
    r.register(apply_edits());
    r.register(save());
    r.register(save_all());
    r.register(revert());
    r.register(set_read_only());
    r.register(list());
    r.register(close());
}

fn store(cx: &CommandContext) -> Arc<DocumentStore> {
    DocumentStore::for_core(&cx.core)
}

fn path_schema() -> Value {
    prop(
        "string",
        "File path: absolute or relative to the project root (default: the focused file)",
    )
}

fn resolve(cx: &CommandContext, args: &Value) -> anyhow::Result<PathBuf> {
    match args
        .get("path")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(p) if Path::new(p).is_absolute() => Ok(PathBuf::from(p)),
        Some(p) => Ok(cx.require_project()?.absolute_path(p)),
        None => cx
            .focus
            .path
            .clone()
            .ok_or_else(|| anyhow!("no `path` and no focused file")),
    }
}

fn project_for(cx: &CommandContext, path: &Path) -> anyhow::Result<Project> {
    if let Some(p) = &cx.project {
        if path.starts_with(p.root()) {
            return Ok(p.clone());
        }
    }
    let dir = path
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent", path.display()))?;
    let mut settings = Settings::default();
    settings.lsp.enabled = false;
    Project::open_detached(dir, settings).map_err(|e| anyhow!("{e}"))
}

/// The open document for `args.path`, opening it when `open` is set.
fn doc(cx: &CommandContext, args: &Value, open: bool) -> anyhow::Result<DocHandle> {
    let path = resolve(cx, args)?;
    let store = store(cx);
    if let Some(h) = store.get(&path) {
        return Ok(h);
    }
    if !open {
        bail!("{} is not open", path.display());
    }
    let project = project_for(cx, &path)?;
    let h = store.open(&path, &project)?;
    // Opened by a command: stays live (with unsaved edits) until `document.close`.
    store.pin(&h);
    Ok(h)
}

fn display_path(cx: &CommandContext, path: &Path) -> String {
    cx.project
        .as_ref()
        .and_then(|p| p.relative_path(path))
        .unwrap_or_else(|| path.display().to_string())
}

fn summary(cx: &CommandContext, h: &DocHandle) -> Value {
    let s = h.snapshot();
    let mut v = json!({
        "path": display_path(cx, &s.path),
        "version": s.version,
        "lines": s.document.line_count(),
        "dirty": s.dirty,
    });
    if let Some(lang) = s.document.language() {
        v["language"] = json!(lang.name);
    }
    if !s.editable {
        v["editable"] = json!(false);
    }
    if s.read_only {
        v["read_only"] = json!(true);
    }
    if s.disk.conflict.is_some() {
        v["conflict"] = json!(true);
    }
    if s.disk.deleted {
        v["deleted"] = json!(true);
    }
    v
}

fn open() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "document.open",
            "Open Document",
            "Documents",
            object(&[("path", path_schema())], &[]),
        ),
        |cx, args| {
            let h = doc(cx, args, true)?;
            let data = summary(cx, &h);
            Ok(
                Outcome::new(format!("Opened {}", data["path"].as_str().unwrap_or("")))
                    .with_data(data),
            )
        },
    )
}

fn read() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "document.read",
            "Read Document",
            "Documents",
            object(
                &[
                    ("path", path_schema()),
                    (
                        "start_line",
                        json!({"type": "integer", "minimum": 0, "description": "First line (0-based)"}),
                    ),
                    (
                        "end_line",
                        json!({"type": "integer", "minimum": 0, "description": "Line after the last one (exclusive)"}),
                    ),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let h = doc(cx, args, true)?;
            let snap = h.snapshot();
            let text = snap.text().ok_or_else(|| anyhow!("not a text file"))?;
            let lines = text.line_count();
            let start = args
                .get("start_line")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .min(lines as u64) as u32;
            let end = args
                .get("end_line")
                .and_then(Value::as_u64)
                .map(|e| e.min(lines as u64) as u32)
                .unwrap_or_else(|| lines.min(start + MAX_READ_LINES))
                .max(start);
            let from = text.line_range(start).start;
            let to = if end >= lines {
                text.len()
            } else {
                text.line_range(end).start
            };
            let body = text.text_for_range(from..to).into_owned();
            let mut data = json!({
                "path": display_path(cx, &snap.path),
                "version": snap.version,
                "start_line": start,
                "end_line": end,
                "lines": lines,
                "text": body,
            });
            if snap.dirty {
                data["dirty"] = json!(true);
            }
            Ok(Outcome::new(format!("Read lines {start}..{end}")).with_data(data))
        },
    )
}

fn edit_schema() -> Value {
    json!({
        "type": "object",
        "description": "Either `range` (byte offsets [start, end) into the current text) or `lines` ([start, end) 0-based lines, whole lines incl. their line break), plus `new_text`.",
        "properties": {
            "range": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "lines": {"type": "array", "items": {"type": "integer", "minimum": 0}},
            "new_text": {"type": "string"},
        },
        "required": ["new_text"],
        "additionalProperties": false,
    })
}

/// Parses JSON edits against `text`.
fn parse_edits(text: &lector_text::Text, edits: &[Value]) -> anyhow::Result<Vec<TextEdit>> {
    let pair = |v: &Value, what: &str| -> anyhow::Result<(u64, u64)> {
        let a = v
            .as_array()
            .filter(|a| a.len() == 2)
            .ok_or_else(|| anyhow!("`{what}` must be [start, end]"))?;
        let s = a[0]
            .as_u64()
            .ok_or_else(|| anyhow!("`{what}` start must be an integer"))?;
        let e = a[1]
            .as_u64()
            .ok_or_else(|| anyhow!("`{what}` end must be an integer"))?;
        if e < s {
            bail!("`{what}` end is before start");
        }
        Ok((s, e))
    };
    edits
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let new_text = e
                .get("new_text")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("edits[{i}]: missing `new_text`"))?
                .to_string();
            let range = if let Some(r) = e.get("range") {
                let (s, e) = pair(r, "range").with_context(|| format!("edits[{i}]"))?;
                if e as usize > text.len() {
                    bail!("edits[{i}]: range end {e} is past the end ({})", text.len());
                }
                s as usize..e as usize
            } else if let Some(l) = e.get("lines") {
                let (s, e) = pair(l, "lines").with_context(|| format!("edits[{i}]"))?;
                let lines = text.line_count() as u64;
                let offset = |line: u64| {
                    if line >= lines {
                        text.len()
                    } else {
                        text.line_range(line as u32).start
                    }
                };
                offset(s)..offset(e)
            } else {
                bail!("edits[{i}]: needs `range` or `lines`");
            };
            Ok(TextEdit { range, new_text })
        })
        .collect()
}

fn apply_edits() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "document.apply_edits",
            "Apply Edits to Document",
            "Documents",
            object(
                &[
                    ("path", path_schema()),
                    ("edits", json!({"type": "array", "items": edit_schema()})),
                    (
                        "expected_version",
                        json!({"type": "integer", "minimum": 0, "description": "Fail unless the document is still at this version"}),
                    ),
                ],
                &["edits"],
            ),
        ),
        |cx, args| {
            let h = doc(cx, args, true)?;
            let snap = h.snapshot();
            let text = snap.text().ok_or_else(|| anyhow!("not a text file"))?;
            let edits = parse_edits(
                text,
                args.get("edits")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
            )?;
            let count = edits.len();
            let expected = args
                .get("expected_version")
                .and_then(Value::as_u64)
                .unwrap_or(snap.version);
            let tx = match h.apply_edits_at(expected, edits) {
                Ok(tx) => tx,
                Err(DocError::VersionMismatch { actual, .. }) => {
                    bail!("the document changed (now version {actual}); read it again")
                }
                Err(e) => return Err(e.into()),
            };
            let mut data = summary(cx, &h);
            data["changed"] = json!(!tx.deltas.is_empty());
            Ok(Outcome::new(format!(
                "Applied {count} edit(s) to {} (unsaved)",
                data["path"].as_str().unwrap_or("")
            ))
            .with_data(data)
            .with_paths(vec![h.path()]))
        },
    )
}

fn do_save(cx: &CommandContext, h: &DocHandle, force: bool) -> anyhow::Result<SaveOutcome> {
    let editor = h.project().settings().editor;
    let _ = cx;
    let req = SaveRequest {
        force,
        ..SaveRequest::from_settings(&editor)
    };
    match h.save(req) {
        Ok(o) => Ok(o),
        Err(DocError::ChangedOnDisk { path }) => bail!(
            "{} changed on disk since it was loaded; nothing was written (save with force to overwrite, or revert)",
            path.display()
        ),
        Err(e) => Err(e.into()),
    }
}

fn save() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "document.save",
            "Save Document",
            "Documents",
            object(
                &[
                    ("path", path_schema()),
                    (
                        "force",
                        prop("boolean", "Overwrite even if the file changed on disk"),
                    ),
                ],
                &[],
            ),
        )
        .with_ui_action("editor::Save"),
        |cx, args| {
            let h = doc(cx, args, false)?;
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let outcome = do_save(cx, &h, force)?;
            let path = display_path(cx, &h.path());
            let summary = match outcome {
                SaveOutcome::Saved => format!("Saved {path}"),
                SaveOutcome::Unchanged => format!("{path} is unchanged"),
            };
            Ok(Outcome::new(summary)
                .with_data(json!({"path": path, "saved": outcome == SaveOutcome::Saved}))
                .with_paths(vec![h.path()]))
        },
    )
}

fn save_all() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "document.save_all",
            "Save All Documents",
            "Documents",
            object(&[], &[]),
        )
        .with_ui_action("workspace::SaveAll"),
        |cx, _| {
            let mut saved = Vec::new();
            let mut failed = Vec::new();
            for h in store(cx).dirty_documents() {
                if !h.is_editable() {
                    continue;
                }
                let path = display_path(cx, &h.path());
                match do_save(cx, &h, false) {
                    Ok(_) => saved.push(path),
                    Err(e) => failed.push(json!({"path": path, "error": e.to_string()})),
                }
            }
            let paths = saved.iter().map(PathBuf::from).collect();
            let summary = if failed.is_empty() {
                format!("Saved {} file(s)", saved.len())
            } else {
                format!("Saved {} file(s), {} failed", saved.len(), failed.len())
            };
            Ok(Outcome::new(summary)
                .with_data(json!({"saved": saved, "failed": failed}))
                .with_paths(paths))
        },
    )
}

fn revert() -> FnCommand {
    FnCommand::new(
        CommandSpec::write(
            "document.revert",
            "Revert Document",
            "Documents",
            object(&[("path", path_schema())], &[]),
        )
        .with_ui_action("editor::RevertFile")
        .destructive(),
        |cx, args| {
            let h = doc(cx, args, false)?;
            h.reload()?;
            let data = summary(cx, &h);
            Ok(
                Outcome::new(format!("Reverted {}", data["path"].as_str().unwrap_or("")))
                    .with_data(data),
            )
        },
    )
}

fn set_read_only() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "document.set_read_only",
            "Set Document Read-Only",
            "Documents",
            object(
                &[
                    ("path", path_schema()),
                    (
                        "read_only",
                        prop("boolean", "Lock (true) or unlock (false) edits"),
                    ),
                ],
                &["read_only"],
            ),
        ),
        |cx, args| {
            let h = doc(cx, args, true)?;
            let ro = args
                .get("read_only")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            h.set_read_only(ro);
            let data = summary(cx, &h);
            let path = data["path"].as_str().unwrap_or("").to_string();
            Ok(Outcome::new(if ro {
                format!("Locked {path}")
            } else {
                format!("Unlocked {path}")
            })
            .with_data(data))
        },
    )
}

fn close() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "document.close",
            "Close Document",
            "Documents",
            object(
                &[
                    ("path", path_schema()),
                    ("discard", prop("boolean", "Close even with unsaved changes (they are lost unless a tab shows the document)")),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let h = doc(cx, args, false)?;
            let discard = args.get("discard").and_then(Value::as_bool).unwrap_or(false);
            if h.is_dirty() && !discard {
                bail!("{} has unsaved changes: save it, or close with discard", display_path(cx, &h.path()));
            }
            let store = store(cx);
            store.unpin(&h);
            let path = display_path(cx, &h.path());
            Ok(Outcome::new(format!("Closed {path}")).with_data(json!({"path": path})))
        },
    )
}

fn list() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "document.list",
            "List Open Documents",
            "Documents",
            object(
                &[(
                    "dirty_only",
                    prop("boolean", "Only documents with unsaved changes"),
                )],
                &[],
            ),
        ),
        |cx, args| {
            let dirty_only = args
                .get("dirty_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let store = store(cx);
            let docs = if dirty_only {
                store.dirty_documents()
            } else {
                store.documents()
            };
            let items: Vec<Value> = docs.iter().map(|h| summary(cx, h)).collect();
            Ok(Outcome::new(format!("{} document(s)", items.len()))
                .with_data(json!({"documents": items})))
        },
    )
    .with_available(|_| Availability::Available)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Source;
    use crate::core::{AppCore, CoreOptions};

    #[test]
    fn agent_edits_documents_through_commands() {
        let dir = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut settings = Settings::default();
        settings.lsp.enabled = false;
        let project = Project::open(&root, settings).unwrap();
        let core = AppCore::open(CoreOptions::for_tests());
        let registry = CommandRegistry::new();
        register(&registry);
        let cx = CommandContext::new(core.clone(), Some(project), Source::Agent);
        let run = |id: &str, args: Value| registry.invoke(id, args, &cx);

        let o = run(
            "document.read",
            json!({"path": "a.txt", "start_line": 1, "end_line": 2}),
        )
        .unwrap();
        assert_eq!(o.data["text"], "two\n");
        let version = o.data["version"].as_u64().unwrap();
        let o = run(
            "document.apply_edits",
            json!({"path": "a.txt", "expected_version": version, "edits": [
                {"lines": [1, 2], "new_text": "TWO\n"},
                {"range": [0, 3], "new_text": "ONE"},
            ]}),
        )
        .unwrap();
        assert_eq!(o.data["dirty"], true);
        // Stale version: refused.
        let err = run(
            "document.apply_edits",
            json!({"path": "a.txt", "expected_version": version, "edits": [{"range": [0, 0], "new_text": "x"}]}),
        )
        .unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
        let o = run("document.list", json!({"dirty_only": true})).unwrap();
        assert_eq!(o.data["documents"][0]["path"], "a.txt");
        // Not written until saved.
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "one\ntwo\nthree\n"
        );
        run("document.save", json!({"path": "a.txt"})).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("a.txt")).unwrap(),
            "ONE\nTWO\nthree\n"
        );
        run(
            "document.apply_edits",
            json!({"path": "a.txt", "edits": [{"range": [0, 0], "new_text": "x"}]}),
        )
        .unwrap();
        run("document.revert", json!({"path": "a.txt"})).unwrap();
        let o = run("document.read", json!({"path": "a.txt"})).unwrap();
        assert_eq!(o.data["text"], "ONE\nTWO\nthree\n");
        run(
            "document.set_read_only",
            json!({"path": "a.txt", "read_only": true}),
        )
        .unwrap();
        assert!(run(
            "document.apply_edits",
            json!({"path": "a.txt", "edits": [{"range": [0, 0], "new_text": "x"}]})
        )
        .is_err());
        let o = run("document.save_all", json!({})).unwrap();
        assert_eq!(o.data["saved"].as_array().unwrap().len(), 0);
        run("document.close", json!({"path": "a.txt"})).unwrap();
        let o = run("document.list", json!({})).unwrap();
        assert_eq!(o.data["documents"].as_array().unwrap().len(), 0);
    }
}
