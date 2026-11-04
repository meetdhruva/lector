//! The three-way merge editor (`git::OpenMergeEditor`, pane item, key context
//! `MergeEditor`): Current (ours) | [Base] | Incoming (theirs) on top —
//! read-only, highlighted, each conflict tinted with Accept buttons — and the
//! editable Result below (a regular [`FileView`] of the working file sharing
//! its `DocumentModel`, so other tabs of the file stay in sync).
//!
//! Panes scroll together, aligned conflict by conflict; Next / Prev conflict,
//! per-conflict and "accept all" resolutions (confirmed) are buffer edits (one
//! undo step each). **Mark Resolved** refuses while markers remain (offering
//! "Resolve Anyway"), then saves through the normal save path, stages the file
//! and moves on to the next conflicted file.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui::{
    div, point, prelude::*, px, AnyElement, AnyView, App, ClickEvent, Context, Entity,
    EventEmitter, FocusHandle, Focusable, FontWeight, Hsla, Pixels, SharedString, Subscription,
    Task, WeakEntity, Window,
};
use lector_ui::actions::git as git_actions;
use lector_ui::actions::workspace as ws_actions;
use lector_ui::code_view::{
    CodeSource, CodeView, CodeViewEvent, Pos, RowBackground, RowWidget, WidgetAnchor,
};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, IconButton, Tooltip};
use lector_ui::editor::FileView;
use lector_ui::icons::{FileIcon, Icon, IconName, IconSize};
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::{ActiveTheme, ColorExt};
use lector_workspace::conflicts::{
    self, ConflictContents, ConflictKind, ConflictParse, ConflictStage, Resolution, Side, SideView,
};

use crate::confirm::{confirm, ConfirmSpec};
use crate::diff_source::RevisionSource;
use crate::inline_conflicts::{ConflictColors, FileConflicts, FileConflictsEvent};
use crate::staging::{run_op, GitOp};
use crate::store::GitStore;
use crate::util::split_path;

/// Layer owner on the top panes.
const LAYER: &str = "merge-editor";

/// How long programmatic scrolls of the other panes are ignored as sources.
const SYNC_GRACE: Duration = Duration::from_millis(250);

/// Tab key of the merge editor for `rel`.
pub fn item_key(rel: &str) -> SharedString {
    format!("merge:{rel}").into()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PaneId {
    Ours,
    Theirs,
    Base,
    Result,
    Nav,
}

/// The read-only top panes, built from the first parse of the file.
struct Panes {
    ours: Entity<CodeView>,
    theirs: Entity<CodeView>,
    /// Base pane: diff3 sections (with rows) or the `:1` stage (without).
    base: Option<Entity<CodeView>>,
    ours_view: SideView,
    theirs_view: SideView,
    /// Present when every conflict had a diff3 base section.
    base_view: Option<SideView>,
    ours_label: SharedString,
    theirs_label: SharedString,
    _subscriptions: Vec<Subscription>,
}

/// The merge editor view.
pub struct MergeEditor {
    store: Entity<GitStore>,
    rel: String,
    abs: PathBuf,
    result: Entity<FileView>,
    tracker: Entity<FileConflicts>,
    panes: Option<Panes>,
    building: Option<Task<()>>,
    original: Vec<ConflictContents>,
    /// Original conflict → current conflict in the Result (`None` = resolved).
    mapping: Vec<Option<usize>>,
    focused: usize,
    show_base: bool,
    busy: bool,
    error: Option<SharedString>,
    driver: Option<(PaneId, Instant)>,
    _subscriptions: Vec<Subscription>,
}

/// Emitted when the title or dirty state may have changed.
pub enum MergeEditorEvent {
    /// Repaint the tab.
    Changed,
}

impl EventEmitter<MergeEditorEvent> for MergeEditor {}

impl Focusable for MergeEditor {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.result.focus_handle(cx)
    }
}

impl MergeEditor {
    /// A merge editor for repository-relative `rel` (`abs` is its absolute path).
    pub fn new(
        store: Entity<GitStore>,
        project: lector_workspace::Project,
        rel: String,
        abs: PathBuf,
        cx: &mut Context<Self>,
    ) -> Self {
        let result = cx.new(|cx| FileView::new(abs.clone(), project, cx));
        let st = store.clone();
        let tracker = cx.new(|cx| FileConflicts::new(st, &result, true, cx));
        if let Some(views) = store.read(cx).views.clone() {
            views
                .conflicts
                .update(cx, |c, _| c.register(&result, tracker.clone()));
        }
        let result_cv = result.read(cx).code_view().clone();
        let subs = vec![
            cx.subscribe(&tracker, |this, _, _: &FileConflictsEvent, cx| {
                this.on_parse_changed(cx)
            }),
            cx.subscribe(&result_cv, |this, _, e: &CodeViewEvent, cx| {
                if *e == CodeViewEvent::Scrolled {
                    this.on_scrolled(PaneId::Result, cx);
                }
            }),
            cx.observe(&result, |_, _, cx| {
                cx.emit(MergeEditorEvent::Changed);
                cx.notify()
            }),
            cx.observe(&store, |_, _, cx| cx.notify()),
        ];
        let mut editor = MergeEditor {
            store,
            rel,
            abs,
            result,
            tracker,
            panes: None,
            building: None,
            original: Vec::new(),
            mapping: Vec::new(),
            focused: 0,
            show_base: false,
            busy: false,
            error: None,
            driver: None,
            _subscriptions: subs,
        };
        editor.on_parse_changed(cx);
        editor
    }

    /// Repository-relative path.
    pub fn rel_path(&self) -> &str {
        &self.rel
    }

    /// The Result file view.
    pub fn result(&self) -> &Entity<FileView> {
        &self.result
    }

    fn parse(&self, cx: &App) -> Option<(std::sync::Arc<str>, std::sync::Arc<ConflictParse>)> {
        self.tracker
            .read(cx)
            .parsed()
            .map(|p| (p.text.clone(), p.parse.clone()))
    }

    /// Unresolved conflicts left in the Result.
    pub fn unresolved(&self, cx: &App) -> usize {
        self.parse(cx).map_or(0, |(_, p)| p.regions.len())
    }

    fn kind(&self, cx: &App) -> Option<ConflictKind> {
        self.store
            .read(cx)
            .conflicts
            .entry(&self.rel)
            .map(|e| e.kind)
    }

    fn on_parse_changed(&mut self, cx: &mut Context<Self>) {
        let Some((text, parse)) = self.parse(cx) else {
            return;
        };
        if self.panes.is_none() && self.building.is_none() && !parse.regions.is_empty() {
            self.original = parse.regions.iter().map(|r| r.contents(&text)).collect();
            self.build_panes(text.clone(), parse.clone(), cx);
        }
        let current: Vec<ConflictContents> =
            parse.regions.iter().map(|r| r.contents(&text)).collect();
        self.mapping = conflicts::match_regions(&self.original, &current);
        if self.mapping.get(self.focused).is_none_or(|m| m.is_none()) {
            if let Some(next) = self.next_unresolved(self.focused, true) {
                self.focused = next;
            }
        }
        self.update_layers(cx);
        cx.emit(MergeEditorEvent::Changed);
        cx.notify();
    }

    fn build_panes(
        &mut self,
        text: std::sync::Arc<str>,
        parse: std::sync::Arc<ConflictParse>,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = self.store.read(cx).project().cloned() else {
            return;
        };
        let rel = self.rel.clone();
        let first = parse.regions.first().cloned();
        let ours_label = first
            .as_ref()
            .map(|r| r.ours_label.clone())
            .unwrap_or_default();
        let theirs_label = first
            .as_ref()
            .map(|r| r.theirs_label.clone())
            .unwrap_or_default();
        let work = cx.background_spawn(async move {
            let ours_view = conflicts::side_view(&text, &parse, Side::Ours);
            let theirs_view = conflicts::side_view(&text, &parse, Side::Theirs);
            let base_markers = conflicts::side_view(&text, &parse, Side::Base);
            let (base_text, base_view) = if base_markers.complete {
                (Some(base_markers.text.clone()), Some(base_markers))
            } else {
                let stage = project.repo().and_then(|repo| {
                    conflicts::stage_text(repo, &rel, ConflictStage::Base)
                        .ok()
                        .flatten()
                });
                (stage, None)
            };
            let side = |t: &str| {
                lector_workspace::review::side_from_bytes(
                    &rel,
                    t.as_bytes().to_vec(),
                    project.languages(),
                )
            };
            let ours = side(&ours_view.text);
            let theirs = side(&theirs_view.text);
            let base = base_text.as_deref().map(side);
            (ours_view, theirs_view, base_view, ours, theirs, base)
        });
        self.building = Some(cx.spawn(async move |this, cx| {
            let (ours_view, theirs_view, base_view, ours, theirs, base) = work.await;
            let _ = this.update(cx, |this, cx| {
                this.building = None;
                let make = |side: lector_workspace::review::DiffSide, cx: &mut Context<Self>| {
                    let source: Rc<dyn CodeSource> = Rc::new(RevisionSource::new(side));
                    cx.new(|cx| {
                        let mut v = CodeView::new(source, cx);
                        v.set_show_minimap(false, cx);
                        v.set_soft_wrap(false, cx);
                        v.set_read_only(true, cx);
                        v
                    })
                };
                let ours = make(ours, cx);
                let theirs = make(theirs, cx);
                let base = base.map(|b| make(b, cx));
                let mut subs = Vec::new();
                for (view, id) in [(&ours, PaneId::Ours), (&theirs, PaneId::Theirs)]
                    .into_iter()
                    .chain(base.as_ref().map(|b| (b, PaneId::Base)))
                {
                    subs.push(cx.subscribe(view, move |this, _, e: &CodeViewEvent, cx| {
                        if *e == CodeViewEvent::Scrolled {
                            this.on_scrolled(id, cx);
                        }
                    }));
                }
                this.panes = Some(Panes {
                    ours,
                    theirs,
                    base,
                    ours_view,
                    theirs_view,
                    base_view,
                    ours_label: ours_label.into(),
                    theirs_label: theirs_label.into(),
                    _subscriptions: subs,
                });
                this.update_layers(cx);
                let focused = this.focused;
                this.reveal(focused, cx);
                cx.notify();
            });
        }));
    }

    // Navigation.

    fn next_unresolved(&self, from: usize, inclusive: bool) -> Option<usize> {
        let n = self.mapping.len();
        (0..n)
            .map(|i| (from + i + usize::from(!inclusive)) % n.max(1))
            .find(|k| self.mapping.get(*k).is_some_and(|m| m.is_some()))
    }

    fn prev_unresolved(&self, from: usize) -> Option<usize> {
        let n = self.mapping.len();
        (1..=n)
            .map(|i| (from + n - i) % n.max(1))
            .find(|k| self.mapping.get(*k).is_some_and(|m| m.is_some()))
    }

    /// Focuses the next (or previous) unresolved conflict and scrolls every pane to it.
    pub fn go(&mut self, forward: bool, cx: &mut Context<Self>) {
        let target = if forward {
            self.next_unresolved(self.focused, false)
        } else {
            self.prev_unresolved(self.focused)
        };
        if let Some(k) = target {
            self.focused = k;
            self.reveal(k, cx);
            self.update_layers(cx);
            cx.notify();
        }
    }

    /// Scrolls all panes so that conflict `k` is in view and puts the Result
    /// caret on it.
    fn reveal(&mut self, k: usize, cx: &mut Context<Self>) {
        self.driver = Some((PaneId::Nav, Instant::now()));
        let margin = 3;
        if let Some(p) = &self.panes {
            let pairs = [
                (&p.ours, p.ours_view.rows.get(k)),
                (&p.theirs, p.theirs_view.rows.get(k)),
            ];
            for (view, rows) in pairs {
                if let Some(rows) = rows {
                    let row = rows.start.saturating_sub(margin);
                    view.update(cx, |v, cx| v.scroll_to_row(row, false, cx));
                }
            }
            if let (Some(view), Some(rows)) =
                (&p.base, p.base_view.as_ref().and_then(|b| b.rows.get(k)))
            {
                let row = rows.start.saturating_sub(margin);
                view.update(cx, |v, cx| v.scroll_to_row(row, false, cx));
            }
        }
        let current = self.mapping.get(k).copied().flatten();
        if let (Some(j), Some((_, parse))) = (current, self.parse(cx)) {
            if let Some(region) = parse.regions.get(j) {
                let row = region.start_row;
                let cv = self.result.read(cx).code_view().clone();
                cv.update(cx, |v, cx| {
                    v.set_cursor(Pos::new(row, 0), cx);
                    v.scroll_to_row(row.saturating_sub(margin), false, cx);
                });
            }
        }
    }

    // Resolutions.

    /// Resolves original conflict `k` (if still unresolved), then moves on.
    pub fn accept(&mut self, k: usize, resolution: Resolution, cx: &mut Context<Self>) {
        let Some(Some(j)) = self.mapping.get(k).copied() else {
            return;
        };
        self.error = None;
        let changed = self
            .tracker
            .update(cx, |t, cx| t.resolve(j, resolution, cx));
        if !changed && !self.tracker.read(cx).model().read(cx).is_editable() {
            self.error =
                Some("This file can't be edited here (read-only or not a text file).".into());
        }
        self.focused = k;
        self.on_parse_changed(cx);
        if let Some(next) = self.next_unresolved(k, false) {
            self.focused = next;
            self.reveal(next, cx);
        }
        self.update_layers(cx);
        cx.notify();
    }

    fn accept_focused(&mut self, resolution: Resolution, cx: &mut Context<Self>) {
        let k = self.focused;
        self.accept(k, resolution, cx);
    }

    /// Asks, then resolves every remaining conflict with one side.
    pub fn accept_all(&mut self, ours: bool, window: &mut Window, cx: &mut Context<Self>) {
        let n = self.unresolved(cx);
        if n == 0 {
            return;
        }
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let (side, label) = if ours {
            ("current", "Accept All Current")
        } else {
            ("incoming", "Accept All Incoming")
        };
        let name = split_path(&self.rel).0.to_string();
        let spec = ConfirmSpec {
            title: format!("Accept all {side} changes in {name}?").into(),
            message: vec![
                format!(
                    "Resolves the {} in the Result with the {side} version; the other side's \
                     changes in them are dropped.",
                    crate::inline_conflicts::count_label(n, "remaining conflict")
                )
                .into(),
                "You can undo this in the Result editor. Nothing is written to disk until you \
                 save or Mark Resolved."
                    .into(),
            ],
            items: vec![self.rel.clone().into()],
            confirm_label: label.into(),
        };
        let this = cx.entity().downgrade();
        confirm(&ws, spec, window, cx, move |_, cx| {
            let _ = this.update(cx, |this, cx| {
                let res = if ours {
                    Resolution::Ours
                } else {
                    Resolution::Theirs
                };
                this.tracker.update(cx, |t, cx| t.resolve_all(res, cx));
                this.on_parse_changed(cx);
            });
        });
    }

    /// `git::MarkResolved`: refuses while markers remain (offering "Resolve
    /// Anyway"), saves, stages and opens the next conflicted file.
    pub fn mark_resolved(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        let markers = self
            .tracker
            .update(cx, |t, cx| t.fresh(cx))
            .map(|p| p.parse.regions.len() + p.parse.problems.len())
            .unwrap_or(0);
        if markers > 0 && !force {
            let Some(ws) = self.store.read(cx).workspace.upgrade() else {
                return;
            };
            let n = self.unresolved(cx);
            let spec = ConfirmSpec {
                title: format!("{} still has conflict markers", split_path(&self.rel).0).into(),
                message: vec![format!(
                    "{} not resolved yet. Marking the file resolved saves and stages it \
                         with the markers in it, and they would be committed as text.",
                    if n == 1 {
                        "1 conflict is".to_string()
                    } else {
                        format!("{n} conflicts are")
                    }
                )
                .into()],
                items: vec![self.rel.clone().into()],
                confirm_label: "Resolve Anyway".into(),
            };
            let this = cx.entity().downgrade();
            confirm(&ws, spec, window, cx, move |window, cx| {
                let _ = this.update(cx, |this, cx| this.mark_resolved(true, window, cx));
            });
            return;
        }
        self.busy = true;
        self.error = None;
        cx.notify();
        let save = self.result.update(cx, |v, cx| v.save(window, cx));
        let store = self.store.clone();
        let rel = self.rel.clone();
        cx.spawn_in(window, async move |this, cx| {
            let saved = save.await;
            let failure = match saved {
                Ok(true) => None,
                Ok(false) => Some("The file was not saved, so it was not staged.".to_string()),
                Err(e) => Some(format!("Saving failed: {e:#}")),
            };
            if let Some(msg) = failure {
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.error = Some(msg.into());
                    cx.notify();
                });
                return;
            }
            let staged = cx.update(|_, cx| run_op(&store, GitOp::Stage(vec![rel.clone()]), cx));
            let Ok(staged) = staged else {
                return;
            };
            let result = staged.await;
            let _ = this.update_in(cx, |this, window, cx| {
                this.busy = false;
                match result {
                    Ok(_) => this.after_resolved(window, cx),
                    Err(e) => this.error = Some(e.into()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Opens the next conflicted file (or says that all are done) and closes this tab.
    fn after_resolved(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let entries = self.store.read(cx).conflicts.entries.clone();
        let next = entries
            .iter()
            .map(|e| e.path.clone())
            .filter(|p| *p != self.rel)
            .find(|p| p.as_str() > self.rel.as_str())
            .or_else(|| {
                entries
                    .iter()
                    .map(|e| e.path.clone())
                    .find(|p| *p != self.rel)
            });
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let name = split_path(&self.rel).0.to_string();
        let key = item_key(&self.rel);
        let store = self.store.clone();
        window.defer(cx, move |window, cx| {
            match &next {
                Some(rel) => {
                    open_merge_editor(&store, rel.clone(), window, cx);
                    ws.update(cx, |ws, cx| ws.show_toast(format!("Resolved {name}"), cx));
                }
                None => {
                    ws.update(cx, |ws, cx| {
                        ws.show_toast(
                            format!("Resolved {name} — no conflicts left; continue from the Changes panel"),
                            cx,
                        )
                    });
                    // `ShowChanges` toggles: only send it when the panel is hidden.
                    let showing = ws.read(cx).is_sidebar_visible()
                        && ws.read(cx).sidebar().read(cx).active_panel()
                            == lector_ui::views::sidebar::SidebarPanel::Changes;
                    if !showing {
                        window.dispatch_action(Box::new(ws_actions::ShowChanges), cx);
                    }
                }
            }
            // Close this merge tab (it is clean after the save; if the Result was
            // edited again while staging ran, the tab stays open with its changes).
            let pane = ws.read(cx).active_pane().clone();
            if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
                let dirty = pane.read(cx).items().nth(ix).is_some_and(|i| i.is_dirty(cx));
                if !dirty {
                    pane.update(cx, |p, cx| p.close_without_prompt(ix, window, cx));
                }
            }
        });
    }

    /// Modify/delete conflicts: deletes the working file (after asking) and stages the deletion.
    fn delete_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ws) = self.store.read(cx).workspace.upgrade() else {
            return;
        };
        let spec = ConfirmSpec {
            title: format!("Delete {}?", split_path(&self.rel).0).into(),
            message: vec![
                "Resolves the conflict by deleting the file from disk and staging the deletion. \
                 Its current content is lost."
                    .into(),
            ],
            items: vec![self.rel.clone().into()],
            confirm_label: "Delete".into(),
        };
        let this = cx.entity().downgrade();
        confirm(&ws, spec, window, cx, move |window, cx| {
            let _ = this.update(cx, |this, cx| {
                let path = this.abs.clone();
                if path.exists() {
                    if let Err(e) = std::fs::remove_file(&path) {
                        this.error = Some(format!("Could not delete: {e}").into());
                        cx.notify();
                        return;
                    }
                }
                let staged = run_op(&this.store, GitOp::Stage(vec![this.rel.clone()]), cx);
                cx.spawn_in(window, async move |this, cx| {
                    let result = staged.await;
                    let _ = this.update_in(cx, |this, window, cx| match result {
                        Ok(_) => this.after_resolved(window, cx),
                        Err(e) => {
                            this.error = Some(e.into());
                            cx.notify();
                        }
                    });
                })
                .detach();
            });
        });
    }

    // Scrolling.

    fn view_of(&self, id: PaneId, cx: &App) -> Option<Entity<CodeView>> {
        match id {
            PaneId::Ours => self.panes.as_ref().map(|p| p.ours.clone()),
            PaneId::Theirs => self.panes.as_ref().map(|p| p.theirs.clone()),
            PaneId::Base => self.panes.as_ref().and_then(|p| p.base.clone()),
            PaneId::Result => Some(self.result.read(cx).code_view().clone()),
            PaneId::Nav => None,
        }
    }

    /// Row span of original conflict `k` in pane `id`.
    fn span(&self, id: PaneId, k: usize, cx: &App) -> Option<std::ops::Range<u32>> {
        let p = self.panes.as_ref()?;
        match id {
            PaneId::Ours => p.ours_view.rows.get(k).cloned(),
            PaneId::Theirs => p.theirs_view.rows.get(k).cloned(),
            PaneId::Base => p.base_view.as_ref()?.rows.get(k).cloned(),
            PaneId::Result => {
                let j = (*self.mapping.get(k)?)?;
                let (_, parse) = self.parse(cx)?;
                parse.regions.get(j).map(|r| r.rows())
            }
            PaneId::Nav => None,
        }
    }

    /// Maps a (fractional) top row of pane `a` to pane `b` through the conflicts.
    fn map_row(&self, a: PaneId, b: PaneId, row: f32, cx: &App) -> f32 {
        let mut best: Option<(std::ops::Range<u32>, std::ops::Range<u32>)> = None;
        for k in 0..self.original.len() {
            if let (Some(sa), Some(sb)) = (self.span(a, k, cx), self.span(b, k, cx)) {
                if sa.start as f32 <= row {
                    best = Some((sa, sb));
                } else {
                    break;
                }
            }
        }
        match best {
            None => row,
            Some((sa, sb)) => {
                let off = row - sa.start as f32;
                if row < sa.end as f32 {
                    sb.start as f32 + off.min(sb.len() as f32)
                } else {
                    sb.end as f32 + (row - sa.end as f32)
                }
            }
        }
    }

    fn on_scrolled(&mut self, source: PaneId, cx: &mut Context<Self>) {
        if let Some((driver, at)) = self.driver {
            if driver != source && at.elapsed() < SYNC_GRACE {
                return;
            }
        }
        let Some(view) = self.view_of(source, cx) else {
            return;
        };
        self.driver = Some((source, Instant::now()));
        let lh = line_height(&view, cx);
        let y = view.read(cx).scroll_offset().y;
        let row = y / lh;
        for other in [PaneId::Ours, PaneId::Theirs, PaneId::Base, PaneId::Result] {
            if other == source {
                continue;
            }
            let Some(target) = self.view_of(other, cx) else {
                continue;
            };
            let mapped = self.map_row(source, other, row, cx).max(0.);
            let tlh = line_height(&target, cx);
            let offset = target.read(cx).scroll_offset();
            let new_y = tlh * mapped;
            if (offset.y - new_y).abs() > px(0.5) {
                target.update(cx, |v, cx| v.set_scroll_offset(point(offset.x, new_y), cx));
            }
        }
    }

    // Layers on the top panes.

    fn update_layers(&mut self, cx: &mut Context<Self>) {
        let Some(p) = &self.panes else {
            return;
        };
        let colors = ConflictColors::get(cx);
        let me = cx.entity().downgrade();
        let focused = self.focused;
        let mapping = self.mapping.clone();
        let layer = |view: &SideView, color: Hsla, side: Side| {
            let mut bgs = Vec::new();
            let mut widgets = Vec::new();
            for (k, rows) in view.rows.iter().enumerate() {
                let resolved = mapping.get(k).is_none_or(|m| m.is_none());
                let tint = if resolved { colors.base } else { color };
                let alpha = match (resolved, k == focused) {
                    (true, _) => 0.07,
                    (false, true) => 0.22,
                    (false, false) => 0.12,
                };
                if !rows.is_empty() {
                    bgs.push(RowBackground {
                        rows: rows.clone(),
                        color: tint.opacity(alpha),
                        edge: Some(if k == focused && !resolved {
                            tint
                        } else {
                            tint.opacity(0.6)
                        }),
                    });
                }
                let me = me.clone();
                widgets.push(RowWidget {
                    row: widget_row(&view.text, rows, k > 0),
                    anchor: WidgetAnchor::AfterText,
                    render: Rc::new(move |_, cx| pane_actions(&me, k, side, resolved, cx)),
                });
            }
            (bgs, widgets)
        };
        let (ob, ow) = layer(&p.ours_view, colors.ours, Side::Ours);
        let (tb, tw) = layer(&p.theirs_view, colors.theirs, Side::Theirs);
        let base = p
            .base_view
            .as_ref()
            .map(|b| layer(b, colors.base, Side::Base));
        let (ours, theirs, base_view) = (p.ours.clone(), p.theirs.clone(), p.base.clone());
        ours.update(cx, |v, cx| {
            v.set_row_backgrounds(LAYER, ob, cx);
            v.set_row_widgets(LAYER, ow, cx);
        });
        theirs.update(cx, |v, cx| {
            v.set_row_backgrounds(LAYER, tb, cx);
            v.set_row_widgets(LAYER, tw, cx);
        });
        if let (Some(view), Some((bb, bw))) = (base_view, base) {
            view.update(cx, |v, cx| {
                v.set_row_backgrounds(LAYER, bb, cx);
                v.set_row_widgets(LAYER, bw, cx);
            });
        }
    }

    // Rendering.

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let (name, dir) = split_path(&self.rel);
        let total = self.original.len();
        let unresolved = self.unresolved(cx);
        let kind = self.kind(cx);
        let op = self
            .store
            .read(cx)
            .conflicts
            .operation
            .as_ref()
            .map(|o| o.describe());
        let base_available = self.panes.as_ref().is_some_and(|p| p.base.is_some());
        let busy = self.busy;
        let counter: SharedString = if total == 0 && unresolved == 0 {
            "No conflict markers".into()
        } else if unresolved == 0 {
            "All conflicts resolved".into()
        } else {
            format!("{unresolved} of {} unresolved", total.max(unresolved)).into()
        };
        let done = unresolved == 0;
        h_flex()
            .flex_none()
            .h(px(38.))
            .px(px(10.))
            .gap(px(8.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .bg(ui.surface.hsla())
            .child(
                Icon::new(IconName::GitMerge)
                    .size(IconSize::Small)
                    .color(theme.git.conflicted.hsla()),
            )
            .child(FileIcon::file(name.to_string()))
            .child(
                h_flex()
                    .min_w_0()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(metrics::TEXT)
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child(name.to_string()),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .overflow_hidden()
                            .whitespace_nowrap()
                            .text_ellipsis()
                            .text_size(metrics::TEXT_SM)
                            .text_color(ui.text_faint.hsla())
                            .child(dir.to_string()),
                    ),
            )
            .when_some(kind, |el, k| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .rounded(px(4.))
                        .text_size(metrics::TEXT_XS)
                        .bg(theme.git.conflicted.hsla().opacity(0.15))
                        .text_color(theme.git.conflicted.hsla())
                        .child(k.label()),
                )
            })
            .when_some(op, |el, op| {
                el.child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_muted.hsla())
                        .child(op),
                )
            })
            .child(div().flex_1())
            .child(
                h_flex()
                    .flex_none()
                    .h(px(22.))
                    .px(px(8.))
                    .gap(px(5.))
                    .rounded(px(11.))
                    .text_size(metrics::TEXT_SM)
                    .bg(if done {
                        ui.success.hsla().opacity(0.14)
                    } else {
                        theme.git.conflicted.hsla().opacity(0.14)
                    })
                    .text_color(if done {
                        ui.success.hsla()
                    } else {
                        ui.text.hsla()
                    })
                    .child(
                        Icon::new(if done {
                            IconName::CircleCheck
                        } else {
                            IconName::AlertTriangle
                        })
                        .size(IconSize::XSmall)
                        .color(if done {
                            ui.success.hsla()
                        } else {
                            theme.git.conflicted.hsla()
                        }),
                    )
                    .child(counter),
            )
            .child(
                IconButton::new("merge-prev", IconName::ArrowUp)
                    .icon_size(IconSize::Small)
                    .disabled(done)
                    .tooltip(Tooltip::text("Previous Conflict (Alt+P)"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(false, cx))),
            )
            .child(
                IconButton::new("merge-next", IconName::ArrowDown)
                    .icon_size(IconSize::Small)
                    .disabled(done)
                    .tooltip(Tooltip::text("Next Conflict (Alt+N)"))
                    .on_click(cx.listener(|this, _, _, cx| this.go(true, cx))),
            )
            .when(base_available, |el| {
                el.child(
                    Button::new("merge-base", "Base")
                        .icon(IconName::Layers)
                        .compact()
                        .selected(self.show_base)
                        .tooltip(Tooltip::text("Show the common ancestor (stage 1)"))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.show_base = !this.show_base;
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("merge-all-ours", "All Current…")
                    .compact()
                    .disabled(done)
                    .tooltip(Tooltip::text(
                        "Accept the current side for every remaining conflict",
                    ))
                    .on_click(cx.listener(|this, _, window, cx| this.accept_all(true, window, cx))),
            )
            .child(
                Button::new("merge-all-theirs", "All Incoming…")
                    .compact()
                    .disabled(done)
                    .tooltip(Tooltip::text(
                        "Accept the incoming side for every remaining conflict",
                    ))
                    .on_click(
                        cx.listener(|this, _, window, cx| this.accept_all(false, window, cx)),
                    ),
            )
            .child(
                Button::new(
                    "merge-resolved",
                    if busy { "Staging…" } else { "Mark Resolved" },
                )
                .icon(IconName::Check)
                .style(ButtonStyle::Accent)
                .compact()
                .disabled(busy)
                .tooltip(Tooltip::text(
                    "Save, stage and go to the next conflicted file",
                ))
                .on_click(cx.listener(|this, _, window, cx| this.mark_resolved(false, window, cx))),
            )
            .into_any_element()
    }

    fn pane_header(
        &self,
        title: &'static str,
        label: SharedString,
        color: Hsla,
        cx: &App,
    ) -> AnyElement {
        let ui = &cx.theme().ui;
        h_flex()
            .flex_none()
            .h(px(26.))
            .px(px(10.))
            .gap(px(6.))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .bg(ui.surface.hsla())
            .text_size(metrics::TEXT_SM)
            .child(div().size(px(8.)).rounded(px(4.)).bg(color))
            .child(
                div()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(ui.text.hsla())
                    .child(title),
            )
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(ui.text_muted.hsla())
                    .child(label),
            )
            .child(div().flex_1())
            .child(
                Icon::new(IconName::Lock)
                    .size(IconSize::XSmall)
                    .color(ui.text_faint.hsla()),
            )
            .into_any_element()
    }

    fn render_top(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let colors = ConflictColors::get(cx);
        let kind = self.kind(cx);
        let Some(p) = &self.panes else {
            // No markers: modify/delete and friends, or still loading.
            let loading = self.tracker.read(cx).parsed().is_none() || self.building.is_some();
            let deleted = kind.is_some_and(|k| !k.has_markers());
            let message: SharedString = if loading {
                "Loading…".into()
            } else if deleted {
                match kind {
                    Some(ConflictKind::DeletedByThem) => {
                        "The incoming change deletes this file; the current side modified it."
                            .into()
                    }
                    Some(ConflictKind::DeletedByUs) => {
                        "The current side deleted this file; the incoming change modified it."
                            .into()
                    }
                    Some(k) => format!("Conflict: {}.", k.label()).into(),
                    None => SharedString::default(),
                }
            } else {
                "No conflict markers left in this file. Review the result below, then Mark Resolved."
                    .into()
            };
            return v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap(px(10.))
                .bg(ui.background.hsla())
                .child(
                    div()
                        .max_w(px(520.))
                        .text_size(metrics::TEXT)
                        .text_color(ui.text_muted.hsla())
                        .child(message),
                )
                .when(deleted && !loading, |el| {
                    el.child(
                        h_flex()
                            .gap(px(8.))
                            .child(
                                Button::new("merge-keep-file", "Keep File")
                                    .icon(IconName::Check)
                                    .style(ButtonStyle::Outlined)
                                    .tooltip(Tooltip::text("Stage the file as it is now"))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.mark_resolved(true, window, cx)
                                    })),
                            )
                            .child(
                                Button::new("merge-delete-file", "Delete File…")
                                    .icon(IconName::Trash)
                                    .style(ButtonStyle::Outlined)
                                    .tooltip(Tooltip::text(
                                        "Delete the file and stage the deletion",
                                    ))
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.delete_file(window, cx)
                                    })),
                            ),
                    )
                })
                .into_any_element();
        };
        let pane = |header: AnyElement, view: Entity<CodeView>| {
            v_flex()
                .flex_1()
                .min_w_0()
                .h_full()
                .child(header)
                .child(div().flex_1().min_h_0().child(view))
        };
        let divider = || div().w(px(1.)).h_full().bg(ui.border.hsla());
        let mut row = h_flex().size_full().child(pane(
            self.pane_header("Current", p.ours_label.clone(), colors.ours, cx),
            p.ours.clone(),
        ));
        if self.show_base {
            if let Some(base) = &p.base {
                let label: SharedString = if p.base_view.is_some() {
                    "common ancestor".into()
                } else {
                    "common ancestor (stage 1, full file)".into()
                };
                row = row.child(divider()).child(pane(
                    self.pane_header("Base", label, colors.base, cx),
                    base.clone(),
                ));
            }
        }
        row.child(divider())
            .child(pane(
                self.pane_header("Incoming", p.theirs_label.clone(), colors.theirs, cx),
                p.theirs.clone(),
            ))
            .into_any_element()
    }
}

/// The row to put a conflict's Accept buttons on, after the text: the first
/// short row of the conflict (or the row just before it), else the shortest.
fn widget_row(text: &str, rows: &std::ops::Range<u32>, has_previous: bool) -> u32 {
    const SHORT: usize = 24;
    let first = if has_previous || rows.start == 0 {
        rows.start
    } else {
        rows.start - 1
    };
    let last = rows.end.max(rows.start + 1);
    let lens: Vec<(u32, usize)> = text
        .split('\n')
        .enumerate()
        .skip(first as usize)
        .take((last - first) as usize)
        .map(|(i, l)| (i as u32, l.trim_end().chars().count()))
        .collect();
    let in_region = |r: &u32| *r >= rows.start;
    lens.iter()
        .filter(|(r, _)| in_region(r))
        .find(|(_, n)| *n <= SHORT)
        .or_else(|| lens.iter().find(|(_, n)| *n <= SHORT))
        .or_else(|| lens.iter().min_by_key(|(_, n)| *n))
        .map_or(rows.start, |(r, _)| *r)
}

fn line_height(view: &Entity<CodeView>, cx: &App) -> Pixels {
    view.read(cx).line_height().unwrap_or_else(|| {
        let s = lector_ui::code_view::EditorSettings::get(cx);
        (s.font_size * s.line_height).round()
    })
}

/// Accept buttons on a top pane for original conflict `k`.
fn pane_actions(
    me: &WeakEntity<MergeEditor>,
    k: usize,
    side: Side,
    resolved: bool,
    cx: &mut App,
) -> AnyElement {
    let theme = cx.theme().clone();
    let ui = &theme.ui;
    if resolved {
        return h_flex()
            .id(("merge-resolved-pill", k))
            .occlude()
            .h(px(18.))
            .px(px(6.))
            .gap(px(3.))
            .rounded(px(9.))
            .bg(ui.elevated.hsla())
            .border_1()
            .border_color(ui.border_variant.hsla())
            .text_size(metrics::TEXT_XS)
            .text_color(ui.text_faint.hsla())
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::XSmall)
                    .color(ui.success.hsla()),
            )
            .child("Resolved")
            .into_any_element();
    }
    let button = |id: &'static str, label: &'static str, res: Resolution, primary: bool| {
        let me = me.clone();
        div()
            .id((id, k))
            .h(px(18.))
            .px(px(7.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .cursor_pointer()
            .when(primary, |el| {
                el.bg(ui.accent.hsla())
                    .text_color(ui.accent_foreground.hsla())
                    .hover(|s| s.opacity(0.9))
            })
            .when(!primary, |el| {
                el.text_color(ui.text_muted.hsla())
                    .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
            })
            .child(label)
            .on_click(move |_: &ClickEvent, _, cx| {
                cx.stop_propagation();
                let _ = me.update(cx, |m, cx| m.accept(k, res, cx));
            })
    };
    let bar = h_flex()
        .id(("merge-pane-actions", k * 4 + side as usize))
        .occlude()
        .p(px(2.))
        .gap(px(2.))
        .rounded(px(6.))
        .bg(ui.elevated.hsla())
        .border_1()
        .border_color(ui.border.hsla())
        .shadow_sm()
        .text_size(metrics::TEXT_XS);
    match side {
        Side::Ours => bar
            .child(button(
                "accept-ours",
                "Accept Current",
                Resolution::Ours,
                true,
            ))
            .child(button(
                "accept-both",
                "Accept Both",
                Resolution::OursThenTheirs,
                false,
            )),
        Side::Theirs => bar
            .child(button(
                "accept-theirs",
                "Accept Incoming",
                Resolution::Theirs,
                true,
            ))
            .child(button(
                "accept-both-rev",
                "Both (Incoming First)",
                Resolution::TheirsThenOurs,
                false,
            )),
        Side::Base => bar.child(button("accept-base", "Accept Base", Resolution::Base, true)),
    }
    .into_any_element()
}

impl Render for MergeEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let dirty = self.result.read(cx).is_dirty(cx);
        let header = self.render_header(cx);
        let top = self.render_top(cx);
        v_flex()
            .id("merge-editor")
            .key_context("MergeEditor")
            .size_full()
            .bg(ui.background.hsla())
            .on_action(cx.listener(|this, _: &git_actions::AcceptOurs, _, cx| {
                this.accept_focused(Resolution::Ours, cx)
            }))
            .on_action(cx.listener(|this, _: &git_actions::AcceptTheirs, _, cx| {
                this.accept_focused(Resolution::Theirs, cx)
            }))
            .on_action(cx.listener(|this, _: &git_actions::AcceptBoth, _, cx| {
                this.accept_focused(Resolution::OursThenTheirs, cx)
            }))
            .on_action(
                cx.listener(|this, _: &git_actions::AcceptAllOurs, window, cx| {
                    this.accept_all(true, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::AcceptAllTheirs, window, cx| {
                    this.accept_all(false, window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &git_actions::NextConflict, _, cx| this.go(true, cx)))
            .on_action(cx.listener(|this, _: &git_actions::PrevConflict, _, cx| this.go(false, cx)))
            .on_action(
                cx.listener(|this, _: &git_actions::MarkResolved, window, cx| {
                    this.mark_resolved(false, window, cx)
                }),
            )
            .on_action(
                cx.listener(|this, _: &git_actions::OpenMergeEditor, _, cx| {
                    // Already here: jump to the focused conflict.
                    let k = this.focused;
                    this.reveal(k, cx);
                }),
            )
            .child(header)
            .when_some(self.error.clone(), |el, e| {
                el.child(
                    h_flex()
                        .flex_none()
                        .px(px(10.))
                        .py(px(6.))
                        .gap(px(6.))
                        .bg(ui.error.hsla().opacity(0.08))
                        .border_b_1()
                        .border_color(ui.error.hsla().opacity(0.5))
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text.hsla())
                        .child(
                            Icon::new(IconName::CircleX)
                                .size(IconSize::XSmall)
                                .color(ui.error.hsla()),
                        )
                        .child(div().flex_1().child(e))
                        .child(
                            IconButton::new("merge-error-close", IconName::Close)
                                .icon_size(IconSize::XSmall)
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.error = None;
                                    cx.notify();
                                })),
                        ),
                )
            })
            .child(div().flex_1().min_h_0().child(top))
            .child(div().flex_none().h(px(1.)).bg(ui.border.hsla()))
            .child(
                h_flex()
                    .flex_none()
                    .h(px(26.))
                    .px(px(10.))
                    .gap(px(6.))
                    .border_b_1()
                    .border_color(ui.border_variant.hsla())
                    .bg(ui.surface.hsla())
                    .text_size(metrics::TEXT_SM)
                    .child(div().size(px(8.)).rounded(px(4.)).bg(ui.accent.hsla()))
                    .child(
                        div()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child("Result"),
                    )
                    .child(
                        div()
                            .text_color(ui.text_muted.hsla())
                            .child(self.rel.clone()),
                    )
                    .when(dirty, |el| {
                        el.child(div().text_color(ui.text_faint.hsla()).child("● unsaved"))
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_faint.hsla())
                            .child("Editable · Alt+1/2/3 accept · Alt+N/P navigate"),
                    ),
            )
            .child(div().flex_1().min_h_0().child(self.result.clone()))
    }
}

/// The merge editor's pane item.
pub struct MergeItem {
    view: Entity<MergeEditor>,
    result: Entity<FileView>,
    result_code_view: Entity<CodeView>,
    key: SharedString,
}

impl MergeItem {
    /// Item for `view`.
    pub fn new(view: Entity<MergeEditor>, cx: &App) -> Self {
        let editor = view.read(cx);
        let key = item_key(editor.rel_path());
        let result = editor.result().clone();
        let result_code_view = result.read(cx).code_view().clone();
        MergeItem {
            view,
            result,
            result_code_view,
            key,
        }
    }
}

impl PaneItem for MergeItem {
    fn title(&self, cx: &App) -> SharedString {
        let rel = self.view.read(cx).rel_path();
        format!("Merge: {}", split_path(rel).0).into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::GitMerge)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn code_view(&self) -> Option<Entity<CodeView>> {
        Some(self.result_code_view.clone())
    }

    fn file_view(&self) -> Option<Entity<FileView>> {
        Some(self.result.clone())
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        Some(lector_config::TabKind::Other {
            label: self.key.to_string(),
        })
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(self.key.clone())
    }

    fn is_dirty(&self, cx: &App) -> bool {
        self.result.read(cx).is_dirty(cx)
    }

    fn clone_item(&self, _window: &mut Window, _cx: &mut App) -> Option<Box<dyn PaneItem>> {
        None
    }
}

/// Opens (or activates) the merge editor of repository-relative `rel`.
pub fn open_merge_editor(store: &Entity<GitStore>, rel: String, window: &mut Window, cx: &mut App) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let key = item_key(&rel);
    let pane = ws.read(cx).active_pane().clone();
    if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
        pane.update(cx, |p, cx| {
            p.pin(ix, cx);
            p.activate(ix, true, window, cx);
        });
        return;
    }
    let Some(item) = new_item(store, rel, cx) else {
        return;
    };
    ws.update(cx, |ws, cx| ws.add_item(item, false, window, cx));
}

/// Opens the merge editor for an absolute path inside the repository.
pub fn open_merge_editor_for_path(
    store: &Entity<GitStore>,
    path: &Path,
    window: &mut Window,
    cx: &mut App,
) {
    if let Some(rel) = rel_path(store, path, cx) {
        let store = store.clone();
        window.defer(cx, move |window, cx| {
            open_merge_editor(&store, rel, window, cx)
        });
    }
}

/// Repository-relative path of `abs`.
pub fn rel_path(store: &Entity<GitStore>, abs: &Path, cx: &App) -> Option<String> {
    let repo = store.read(cx).repo()?;
    let wd = dunce::canonicalize(repo.workdir()).ok()?;
    let abs = dunce::canonicalize(abs).unwrap_or_else(|_| abs.to_path_buf());
    let rel = abs.strip_prefix(&wd).ok()?;
    Some(rel.to_string_lossy().replace('\\', "/"))
}

/// Recreates a merge tab from the session.
pub fn restore_item(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn PaneItem>> {
    let lector_config::TabKind::Other { label } = kind else {
        return None;
    };
    let rel = label.strip_prefix("merge:")?.to_string();
    Some(new_item(store, rel, cx)? as Box<dyn PaneItem>)
}

fn new_item(store: &Entity<GitStore>, rel: String, cx: &mut App) -> Option<Box<MergeItem>> {
    let s = store.read(cx);
    let project = s.project()?.clone();
    let abs = s.repo()?.workdir().join(&rel);
    let st = store.clone();
    let view = cx.new(|cx| MergeEditor::new(st, project, rel, abs, cx));
    Some(Box::new(MergeItem::new(view, cx)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_buttons_go_on_a_short_row() {
        let text = "fn main() {\n    let a_very_long_binding_name = compute_something(1, 2, 3);\n    }\n}\n";
        // Region rows 1..3: row 2 ("    }") is short.
        assert_eq!(widget_row(text, &(1..3), true), 2);
        // The row before the region is short and allowed for the first conflict.
        assert_eq!(
            widget_row("x\nlong long long long long long long\n", &(1..2), false),
            0
        );
        // Empty region at the end of the text.
        assert_eq!(widget_row("a\n", &(1..1), true), 1);
    }
}
