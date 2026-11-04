//! Blame for the active code view: a left gutter column with author, age and
//! summary per block of lines; clicking a block opens its commit.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    App, AppContext, Context, Entity, EntityId, Global, Hsla, SharedString, Subscription, Task,
    WeakEntity, Window,
};
use lector_diff::{diff_lines, DiffOptions, LineDiff};
use lector_git::{BlameHunk, Oid};
use lector_ui::code_view::{CodeView, GutterAnnotation, GutterAnnotations};
use lector_ui::editor::{DocumentEvent, DocumentModel};
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::Project;

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

/// How long typing must pause before blame is mapped onto the edited text.
const REMAP_DEBOUNCE: Duration = Duration::from_millis(120);

/// Maps blame `hunks` (of the text `diff` was taken from) onto the lines of
/// the edited text: unchanged lines keep their commit and move with the edit,
/// added or modified lines become "not committed yet".
pub fn remap_hunks(hunks: &[BlameHunk], diff: &LineDiff) -> Vec<BlameHunk> {
    let edited = Oid::from_bytes(&[0u8; 20]);
    let mut out: Vec<BlameHunk> = Vec::with_capacity(hunks.len() + diff.hunks.len());
    let copy = |out: &mut Vec<BlameHunk>, old: std::ops::Range<u32>, new_start: u32| {
        let first = hunks.partition_point(|h| h.lines.end <= old.start);
        for h in hunks[first..]
            .iter()
            .take_while(|h| h.lines.start < old.end)
        {
            let start = h.lines.start.max(old.start);
            let end = h.lines.end.min(old.end);
            if start >= end {
                continue;
            }
            let mut moved = h.clone();
            moved.lines = start - old.start + new_start..end - old.start + new_start;
            moved.orig_start = h.orig_start + (start - h.lines.start);
            out.push(moved);
        }
    };
    let (mut old_pos, mut new_pos) = (0u32, 0u32);
    for hunk in &diff.hunks {
        copy(&mut out, old_pos..hunk.old.start, new_pos);
        if let (Some(oid), false) = (edited, hunk.new.is_empty()) {
            out.push(BlameHunk {
                oid,
                lines: hunk.new.clone(),
                orig_start: 0,
                orig_path: String::new(),
                author: String::new(),
                time: 0,
                summary: String::new(),
                uncommitted: true,
            });
        }
        old_pos = hunk.old.end;
        new_pos = hunk.new.end;
    }
    copy(&mut out, old_pos..diff.old_lines, new_pos);
    out
}

/// Blame of `base` (the file on disk) shown for `current` (the buffer).
fn blame_for_text(hunks: &[BlameHunk], base: &str, current: &str) -> Vec<BlameHunk> {
    if base == current {
        return hunks.to_vec();
    }
    remap_hunks(hunks, &diff_lines(base, current, &DiffOptions::default()))
}

/// Line endings do not matter to blame mapping.
fn unix_lines(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// Blame sessions by code view, so toggling blame off drops the session.
#[derive(Default)]
struct BlameSessions(HashMap<EntityId, Entity<BlameSession>>);

impl Global for BlameSessions {}

/// Keeps a view's blame column true while its buffer is edited: edited lines
/// read "Not committed yet" and blame is refreshed after a save.
struct BlameSession {
    view: WeakEntity<CodeView>,
    model: Entity<DocumentModel>,
    store: Entity<GitStore>,
    project: Project,
    rel: String,
    /// Blame of the file as it is on disk.
    hunks: Arc<Vec<BlameHunk>>,
    /// The text blame was computed for.
    base: Arc<String>,
    remap_task: Option<Task<()>>,
    refresh_task: Option<Task<()>>,
    _subscription: Subscription,
}

impl BlameSession {
    fn new(
        view: WeakEntity<CodeView>,
        model: Entity<DocumentModel>,
        store: Entity<GitStore>,
        project: Project,
        rel: String,
        (hunks, base): (Arc<Vec<BlameHunk>>, String),
        cx: &mut Context<Self>,
    ) -> Self {
        let subscription = cx.subscribe(&model, |this, _, event: &DocumentEvent, cx| match event {
            DocumentEvent::SourceChanged => this.remap(REMAP_DEBOUNCE, cx),
            DocumentEvent::Saved | DocumentEvent::Loaded => this.refresh(cx),
            _ => {}
        });
        let mut session = Self {
            view,
            model,
            store,
            project,
            rel,
            hunks,
            base: Arc::new(unix_lines(base)),
            remap_task: None,
            refresh_task: None,
            _subscription: subscription,
        };
        session.remap(Duration::ZERO, cx);
        session
    }

    /// Re-annotates the view for the buffer's current text after `delay`.
    fn remap(&mut self, delay: Duration, cx: &mut Context<Self>) {
        let Some(text) = self
            .model
            .read(cx)
            .live_document()
            .and_then(|d| d.text().cloned())
        else {
            return;
        };
        let (hunks, base) = (self.hunks.clone(), self.base.clone());
        self.remap_task = Some(cx.spawn(async move |this, cx| {
            if !delay.is_zero() {
                cx.background_executor().timer(delay).await;
            }
            let mapped = cx
                .background_executor()
                .spawn(async move { blame_for_text(&hunks, &base, &unix_lines(text.to_string())) })
                .await;
            let _ = this.update(cx, |this, cx| this.show(&mapped, cx));
        }));
    }

    fn show(&mut self, hunks: &[BlameHunk], cx: &mut Context<Self>) {
        let Some(view) = self.view.upgrade() else {
            return;
        };
        if view.read(cx).gutter_annotations().is_none() {
            return;
        }
        apply(&view, &self.store, hunks, cx);
    }

    /// Blames the file again (after a save or a reload from disk).
    fn refresh(&mut self, cx: &mut Context<Self>) {
        let project = self.project.clone();
        let rel = self.rel.clone();
        let path = self.model.read(cx).path().to_path_buf();
        self.refresh_task = Some(cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move {
                    let hunks = project.blame(&rel).map_err(|e| e.to_string())?;
                    let base = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;
                    Ok::<_, String>((hunks, base))
                })
                .await;
            let _ = this.update(cx, |this, cx| match result {
                Ok((hunks, base)) => {
                    this.hunks = hunks;
                    this.base = Arc::new(unix_lines(base));
                    this.remap(Duration::ZERO, cx);
                }
                Err(err) => tracing::warn!("blame refresh: {err}"),
            });
        }));
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
        cx.default_global::<BlameSessions>()
            .0
            .remove(&view.entity_id());
        return;
    }
    let model = ws
        .active_pane()
        .read(cx)
        .active_item()
        .and_then(|item| item.file_view())
        .map(|file| file.read(cx).model().clone());
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
    // The file on disk is what blame describes; the buffer may differ.
    let disk = path.clone();
    let blamed = project.clone();
    let blamed_rel = rel.clone();
    let task = cx.background_spawn(async move {
        let hunks = blamed.blame(&blamed_rel).map_err(|e| e.to_string())?;
        let text = std::fs::read_to_string(&disk).unwrap_or_default();
        Ok::<_, String>((hunks, text))
    });
    let store = store.clone();
    let view = view.downgrade();
    cx.spawn(async move |ws, cx| {
        let result = task.await;
        let _ = ws.update(cx, |ws, cx| {
            let (hunks, base) = match result {
                Ok(blamed) => blamed,
                Err(e) => {
                    tracing::warn!("blame: {e}");
                    ws.show_toast("Blame unavailable for this file", cx);
                    return;
                }
            };
            let Some(view) = view.upgrade() else { return };
            match model {
                Some(model) => {
                    let id = view.entity_id();
                    let weak = view.downgrade();
                    // Show something at once; the session maps it to the buffer.
                    apply(&view, &store, &hunks, cx);
                    let session = cx.new(|cx| {
                        BlameSession::new(weak, model, store, project, rel, (hunks, base), cx)
                    });
                    cx.default_global::<BlameSessions>().0.insert(id, session);
                }
                None => apply(&view, &store, &hunks, cx),
            }
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

    fn committed(hunks: &[BlameHunk]) -> Vec<(std::ops::Range<u32>, bool)> {
        hunks
            .iter()
            .map(|h| (h.lines.clone(), h.uncommitted))
            .collect()
    }

    fn remapped(base: &str, current: &str, hunks: &[BlameHunk]) -> Vec<BlameHunk> {
        blame_for_text(hunks, base, current)
    }

    #[test]
    fn edited_lines_are_not_committed_yet_and_the_rest_moves() {
        let hunks = vec![hunk(1, 0..2, 100, false), hunk(2, 2..5, 200, false)];
        let base = "a\nb\nc\nd\ne\n";

        // Unchanged text: blame as is.
        assert_eq!(remapped(base, base, &hunks), hunks);

        // Edit inside the first hunk: only that line changes.
        let out = remapped(base, "a\nB\nc\nd\ne\n", &hunks);
        assert_eq!(
            committed(&out),
            [(0..1, false), (1..2, true), (2..5, false)]
        );

        // Insert two lines at the top: everything else shifts down.
        let out = remapped(base, "x\ny\na\nb\nc\nd\ne\n", &hunks);
        assert_eq!(
            committed(&out),
            [(0..2, true), (2..4, false), (4..7, false)]
        );
        assert_eq!(out[2].orig_start, 0);

        // Delete a line inside the second hunk: it shrinks (in two pieces that
        // the gutter merges), nothing is marked.
        let out = remapped(base, "a\nb\nc\ne\n", &hunks);
        assert_eq!(
            committed(&out),
            [(0..2, false), (2..3, false), (3..4, false)]
        );
        let ann = annotations_for(&out, 1000, &|_| gpui::black(), gpui::white());
        assert_eq!(
            ann.blocks
                .iter()
                .map(|b| b.rows.clone())
                .collect::<Vec<_>>(),
            [0..2, 2..4]
        );

        // Append at the end.
        let out = remapped(base, "a\nb\nc\nd\ne\nf\n", &hunks);
        assert_eq!(
            committed(&out),
            [(0..2, false), (2..5, false), (5..6, true)]
        );

        // Two separate edits keep the lines between them.
        let out = remapped(base, "A\nb\nc\nd\nE\n", &hunks);
        assert_eq!(
            committed(&out),
            [(0..1, true), (1..2, false), (2..4, false), (4..5, true)]
        );
    }

    #[test]
    fn remapped_blame_renders_edited_lines_as_not_committed() {
        let hunks = vec![hunk(1, 0..3, 100, false)];
        let out = remapped("a\nb\nc\n", "a\nb2\nc\n", &hunks);
        let ann = annotations_for(&out, 1000, &|_| gpui::black(), gpui::white());
        assert_eq!(ann.blocks.len(), 3);
        assert_eq!(ann.blocks[1].text.as_ref(), "Not committed yet");
        assert!(ann.blocks[1].muted);
        assert_eq!(ann.blocks[1].rows, 1..2);
    }

    #[test]
    fn line_endings_do_not_mark_lines() {
        assert_eq!(unix_lines("a\r\nb\r\n".into()), "a\nb\n");
    }

    use gpui::{Focusable, TestAppContext, VisualTestContext};
    use lector_ui::actions::editor::{Save, ToggleBlame};
    use lector_ui::code_view::Pos;
    use lector_ui::editor::FileView;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "Ada Lovelace")
            .env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada Lovelace")
            .env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn blocks(view: &gpui::Entity<FileView>, cx: &mut VisualTestContext) -> Vec<(String, String)> {
        view.read_with(cx, |v, cx| {
            let ann = v.code_view().read(cx).gutter_annotations().cloned();
            ann.map(|a| {
                a.blocks
                    .iter()
                    .map(|b| {
                        (
                            format!("{}..{}", b.rows.start, b.rows.end),
                            b.text.to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn blame_follows_unsaved_edits_and_refreshes_after_save(cx: &mut TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        git(&root, &["add", "a.txt"]);
        git(&root, &["commit", "-q", "-m", "initial"]);

        let mut settings = lector_config::Settings::default();
        settings.lsp.enabled = false;
        settings.session.restore = false;
        cx.update(|cx| {
            lector_ui::ThemeState::init(
                cx,
                lector_theme::ThemeRegistry::new(),
                lector_ui::ThemeMode::Dark,
                lector_theme::Appearance::Dark,
            );
            cx.set_global(lector_ui::code_view::EditorSettings::default());
            cx.set_global(lector_ui::settings::AppSettings {
                settings: std::sync::Arc::new(settings.clone()),
                warnings: Default::default(),
            });
            crate::init(cx);
        });
        let project = Project::open(&root, settings).unwrap();
        let (ws, cx) = cx.add_window_view(|window, cx| Workspace::new(project, window, cx));
        cx.run_until_parked();
        let view = ws
            .update_in(cx, |ws, window, cx| {
                ws.open_path_at(root.join("a.txt"), Some(1), Some(1), window, cx)
            })
            .expect("file view");
        cx.run_until_parked();
        cx.update(|window, cx| window.focus(&view.focus_handle(cx)));
        cx.run_until_parked();

        let initial = vec![("0..3".to_string(), "Ada · initial".to_string())];
        cx.dispatch_action(ToggleBlame);
        cx.run_until_parked();
        assert_eq!(blocks(&view, cx), initial);

        // An edited line reads "Not committed yet"; the others keep their commit.
        view.update(cx, |v, cx| {
            v.code_view()
                .update(cx, |c, cx| c.set_cursor(Pos::new(1, 0), cx))
        });
        cx.simulate_input("X");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        let edited = vec![
            ("0..1".to_string(), "Ada · initial".to_string()),
            ("1..2".to_string(), "Not committed yet".to_string()),
            ("2..3".to_string(), "Ada · initial".to_string()),
        ];
        assert_eq!(blocks(&view, cx), edited);

        // A new line above shifts blame instead of mislabelling the old lines.
        view.update(cx, |v, cx| {
            v.code_view()
                .update(cx, |c, cx| c.set_cursor(Pos::new(0, 0), cx))
        });
        cx.simulate_input("new\n");
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        assert_eq!(
            blocks(&view, cx),
            vec![
                ("0..1".to_string(), "Not committed yet".to_string()),
                ("1..2".to_string(), "Ada · initial".to_string()),
                ("2..3".to_string(), "Not committed yet".to_string()),
                ("3..4".to_string(), "Ada · initial".to_string()),
            ]
        );

        // Saving refreshes blame from git: the same lines are uncommitted.
        cx.dispatch_action(Save);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        assert!(!view.read_with(cx, |v, cx| v.is_dirty(cx)));
        assert_eq!(
            blocks(&view, cx),
            vec![
                ("0..1".to_string(), "Not committed yet".to_string()),
                ("1..2".to_string(), "Ada · initial".to_string()),
                ("2..3".to_string(), "Not committed yet".to_string()),
                ("3..4".to_string(), "Ada · initial".to_string()),
            ]
        );

        // Committing and saving again turns everything into committed lines.
        git(&root, &["commit", "-q", "-a", "-m", "edit"]);
        view.update(cx, |v, cx| {
            v.code_view()
                .update(cx, |c, cx| c.set_cursor(Pos::new(3, 0), cx))
        });
        cx.simulate_input("!");
        cx.dispatch_action(Save);
        cx.executor()
            .advance_clock(std::time::Duration::from_millis(400));
        cx.run_until_parked();
        let after = blocks(&view, cx);
        assert_eq!(after.len(), 4, "{after:?}");
        assert_eq!(after[0].1, "Ada · edit");
        assert_eq!(after[1].1, "Ada · initial");
        assert_eq!(after[3].1, "Not committed yet");
    }
}
