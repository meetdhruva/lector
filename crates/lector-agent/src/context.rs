//! Turning a prompt and its [`PromptContext`] into ACP content blocks.

use lector_app::documents::DocumentStore;
use lector_app::AppCore;
use lector_workspace::Project;
use serde_json::{json, Value};

use crate::model::PromptContext;
use crate::paths::{file_uri, relative, resolve_in_root};

/// Biggest file sent as context (larger ones are sent as links only).
pub const MAX_CONTEXT_BYTES: usize = 256 * 1024;

/// The current text of a project file: the open document (unsaved edits included) or disk.
pub fn current_text(core: &AppCore, project: &Project, path: &str) -> Option<(String, String)> {
    let abs = resolve_in_root(project.root(), path).ok()?;
    let rel = relative(project.root(), &abs);
    let store = DocumentStore::for_core(core);
    if let Some(h) = store.get(&abs) {
        let snap = h.snapshot();
        if let Some(t) = snap.text() {
            return Some((rel, t.text_for_range(0..t.len()).into_owned()));
        }
    }
    let bytes = std::fs::read(&abs).ok()?;
    Some((rel, String::from_utf8(bytes).ok()?))
}

/// ACP `prompt` blocks: the text first, then each context item — as an embedded
/// resource when the agent supports it, else as a text block.
pub fn build_prompt(
    core: &AppCore,
    project: &Project,
    text: &str,
    context: &[PromptContext],
    embedded: bool,
) -> Vec<Value> {
    let mut blocks = vec![json!({ "type": "text", "text": text })];
    for item in context {
        match item {
            PromptContext::File { path } => {
                blocks.push(file_block(core, project, path, None, embedded));
            }
            PromptContext::Range { path, start, end } => {
                blocks.push(file_block(
                    core,
                    project,
                    path,
                    Some((*start, *end)),
                    embedded,
                ));
            }
            PromptContext::Diff { label, diff } => blocks.push(json!({
                "type": "text",
                "text": format!("{label}:\n```diff\n{}\n```", diff.trim_end()),
            })),
            PromptContext::Comments { text } => blocks.push(json!({
                "type": "text",
                "text": format!("Review feedback:\n{text}"),
            })),
        }
    }
    blocks
}

fn file_block(
    core: &AppCore,
    project: &Project,
    path: &str,
    lines: Option<(u32, u32)>,
    embedded: bool,
) -> Value {
    let abs = resolve_in_root(project.root(), path).unwrap_or_else(|_| project.root().join(path));
    let mut uri = file_uri(&abs);
    let Some((rel, text)) = current_text(core, project, path) else {
        return json!({ "type": "resource_link", "uri": uri, "name": path });
    };
    let body = match lines {
        Some((start, end)) => {
            uri.push_str(&format!("#L{}-L{}", start + 1, end.max(start + 1)));
            text.split_inclusive('\n')
                .skip(start as usize)
                .take(end.saturating_sub(start).max(1) as usize)
                .collect::<String>()
        }
        None => text,
    };
    if body.len() > MAX_CONTEXT_BYTES {
        return json!({ "type": "resource_link", "uri": uri, "name": rel });
    }
    if embedded {
        json!({
            "type": "resource",
            "resource": { "uri": uri, "mimeType": "text/plain", "text": body }
        })
    } else {
        let where_ = match lines {
            Some((s, e)) => format!("{rel} (lines {}-{})", s + 1, e.max(s + 1)),
            None => rel,
        };
        json!({ "type": "text", "text": format!("{where_}:\n```\n{}\n```", body.trim_end_matches('\n')) })
    }
}
