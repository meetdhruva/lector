//! Blame for the active code view: a left gutter column with author, age and
//! summary per block of lines; clicking a block opens its commit.

use std::collections::HashMap;

use gpui::{App, AppContext, Context, Entity, Hsla, SharedString, Window};
use lector_git::BlameHunk;
use lector_ui::code_view::{CodeView, GutterAnnotation, GutterAnnotations};
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::store::GitStore;
use crate::util::{now_secs, short_relative_time};

/// Builds gutter annotations from blame hunks (adjacent hunks of the same
/// commit are merged; blocks alternate shading).
pub fn annotations_for(
    hunks: &[BlameHunk],
    now: i64,
    heat: &dyn Fn(f32) -> Hsla,
    uncommitted: Hsla,
) -> GutterAnnotations {
    let mut blocks: Vec<GutterAnnotation> = Vec::new();
    let mut owners: Vec<lector_git::Oid> = Vec::new();
    let oldest = hunks
        .iter()
        .filter(|h| !h.uncommitted)
        .map(|h| h.time)
        .min()
        .unwrap_or(now);
    let span = (now - oldest).max(1) as f32;
    for h in hunks {
        if let (Some(last), Some(owner)) = (blocks.last_mut(), owners.last()) {
            if *owner == h.oid && last.rows.end == h.lines.start {
                last.rows.end = h.lines.end;
                continue;
            }
        }
        let shaded = blocks.len() % 2 == 1;
        let (text, detail, bar, muted) = if h.uncommitted {
            (
                "Not committed yet".to_string(),
                String::new(),
                Some(uncommitted),
                true,
            )
        } else {
            let first = h.author.split_whitespace().next().unwrap_or(&h.author);
            let age = 1. - ((now - h.time) as f32 / span).clamp(0., 1.);
            (
                format!("{first} · {}", h.summary),
                short_relative_time(h.time, now),
                Some(heat(age)),
                false,
            )
        };
        blocks.push(GutterAnnotation {
            rows: h.lines.clone(),
            text: text.into(),
            detail: detail.into(),
            bar,
            shaded,
            muted,
        });
        owners.push(h.oid);
    }
    GutterAnnotations {
        width_chars: 38,
        blocks,
    }
}

/// Toggles blame on the workspace's active code view.
pub fn toggle_blame(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    _window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let Some(view) = ws.active_code_view(cx) else {
        return;
    };
    if view.read(cx).gutter_annotations().is_some() {
        view.update(cx, |v, cx| v.set_gutter_annotations(None, cx));
        return;
    }
    let Some(path) = view.read(cx).path().cloned().or_else(|| ws.active_path(cx)) else {
        ws.show_toast("Blame needs a file in the repository", cx);
        return;
    };
    let Some(project) = store.read(cx).project().cloned() else {
        return;
    };
    let Some(rel) = project.relative_path(&path) else {
        ws.show_toast("File is outside the repository", cx);
        return;
    };
    let task = cx.background_spawn(async move { project.blame(&rel) });
    let store = store.clone();
    let view = view.downgrade();
    cx.spawn(async move |ws, cx| {
        let result = task.await;
        let _ = ws.update(cx, |ws, cx| {
            let hunks: std::sync::Arc<Vec<BlameHunk>> = match result {
                Ok(h) => h,
                Err(e) => {
                    tracing::warn!("blame: {e}");
                    ws.show_toast("Blame unavailable for this file", cx);
                    return;
                }
            };
            let Some(view) = view.upgrade() else { return };
            apply(&view, &store, &hunks, cx);
        });
    })
    .detach();
}

fn apply(view: &Entity<CodeView>, store: &Entity<GitStore>, hunks: &[BlameHunk], cx: &mut App) {
    let theme = cx.theme().clone();
    let dark = theme.appearance.is_dark();
    let accent = theme.ui.accent.hsla();
    let heat = move |age: f32| {
        // Recent = saturated accent, old = faint.
        accent.opacity(if dark {
            0.15 + 0.75 * age
        } else {
            0.2 + 0.7 * age
        })
    };
    let ann = annotations_for(hunks, now_secs(), &heat, theme.git.modified.hsla());
    // Block index → commit, for clicks.
    let mut commit_of: HashMap<usize, (lector_git::Oid, bool)> = HashMap::new();
    for (i, b) in ann.blocks.iter().enumerate() {
        if let Some(h) = hunks.iter().find(|h| h.lines.start == b.rows.start) {
            commit_of.insert(i, (h.oid, h.uncommitted));
        }
    }
    let store = store.clone();
    view.update(cx, |v, cx| {
        v.set_gutter_annotations(Some(ann), cx);
        v.on_gutter_annotation_click(move |ix, window, cx| {
            if let Some((oid, false)) = commit_of.get(&ix).copied() {
                crate::open_commit(&store, oid, window, cx);
            }
        });
    });
}

/// Label used in tests / tooltips.
pub fn describe(h: &BlameHunk) -> SharedString {
    format!("{} {} — {}", h.oid.short(), h.author, h.summary).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_git::Oid;

    fn hunk(oid: u8, lines: std::ops::Range<u32>, time: i64, uncommitted: bool) -> BlameHunk {
        let mut bytes = [0u8; 20];
        bytes[0] = oid;
        BlameHunk {
            oid: Oid::from_bytes(&bytes).unwrap(),
            lines,
            orig_start: 0,
            orig_path: "a.rs".into(),
            author: "Ada Lovelace".into(),
            time,
            summary: "Fix it".into(),
            uncommitted,
        }
    }

    #[test]
    fn merges_and_alternates() {
        let hunks = vec![
            hunk(1, 0..2, 100, false),
            hunk(1, 2..3, 100, false),
            hunk(2, 3..5, 200, false),
            hunk(0, 5..6, 0, true),
        ];
        let ann = annotations_for(&hunks, 1000, &|_| gpui::black(), gpui::white());
        assert_eq!(ann.blocks.len(), 3);
        assert_eq!(ann.blocks[0].rows, 0..3);
        assert!(!ann.blocks[0].shaded && ann.blocks[1].shaded);
        assert_eq!(ann.blocks[0].text.as_ref(), "Ada · Fix it");
        assert!(ann.blocks[2].muted);
        assert_eq!(ann.block_at(4), Some(1));
        assert_eq!(ann.block_at(9), None);
    }
}
