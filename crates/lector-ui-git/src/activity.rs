//! The Activity log (`workspace::ShowActivity`) and "Undo last operation"
//! (`workspace::UndoLastOperation`): every write Lector made to your files or repository,
//! newest first, with an Undo button per entry, plus the Undo toast shown after a discard.
//!
//! Entries come from the activity journal (`lector_app::journal`); undoing restores what
//! the operation overwrote from the shadow repository (Lector's own backup, never your
//! `.git`) or runs the inverse command, and records the undo as a new entry (so it can be
//! undone again). When files changed since, Undo refuses and asks whether to restore
//! anyway.

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::UNIX_EPOCH;

use futures::StreamExt as _;
use gpui::{
    div, prelude::*, AnyView, App, Context, Entity, FocusHandle, Focusable, FontWeight,
    SharedString, Task, Window,
};
use lector_app::commands::{CommandContext, Source};
use lector_app::journal::{
    ActivityEntry, Journal, JournalEvent, Subscription as JournalSubscription, UndoError, UndoMode,
    UndoRecord, UndoStatus,
};
use lector_git::{Repo, ShadowRepo, Side};
use lector_ui::components::{h_flex, metrics, v_flex, Button, ButtonStyle, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::views::workspace::{ToastLevel, UNDO_TOAST_DURATION};
use lector_ui::views::Workspace;
use lector_ui::{ActiveTheme, ColorExt};

use crate::confirm::{confirm, ConfirmSpec};
use crate::staging::notify_repo_changed;
use crate::store::GitStore;
use crate::util::{now_secs, relative_time};

const KEY: &str = "activity";
/// Entries loaded into the view.
const LIMIT: usize = 150;
/// Entries whose backup is checked for existence (the rest are assumed intact until clicked).
const CHECKED: usize = 30;

fn journal(cx: &App) -> Journal {
    Journal::install(&lector_ui::commands_bridge::core(cx))
}

fn context_for(store: &Entity<GitStore>, cx: &App) -> CommandContext {
    CommandContext::new(
        lector_ui::commands_bridge::core(cx),
        store.read(cx).project().cloned(),
        Source::Ui,
    )
}

fn toast(store: &Entity<GitStore>, message: impl Into<SharedString>, cx: &mut App) {
    if let Some(ws) = store.read(cx).workspace.upgrade() {
        let message = message.into();
        ws.update(cx, |ws, cx| ws.show_toast(message, cx));
    }
}

/// Shows "<summary> · Undo" for eight seconds (`entry_id` is the journal entry to undo).
pub fn show_undo_toast(store: &Entity<GitStore>, entry_id: u64, summary: String, cx: &mut App) {
    show_undo_toast_level(store, entry_id, summary, ToastLevel::Success, cx)
}

/// [`show_undo_toast`] with the icon of `level` (a warning for "applied with conflicts").
pub fn show_undo_toast_level(
    store: &Entity<GitStore>,
    entry_id: u64,
    summary: String,
    level: ToastLevel,
    cx: &mut App,
) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let store = store.clone();
    ws.update(cx, |ws, cx| {
        ws.show_action_toast_level(
            summary,
            level,
            "Undo",
            UNDO_TOAST_DURATION,
            move |_, window, cx| undo_entry(&store, entry_id, UndoMode::Check, window, cx),
            cx,
        );
    });
}

/// [`show_undo_toast`] from inside a workspace update (the command palette and menus run
/// registry commands through `lector_ui::commands_bridge`).
pub fn offer_undo(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    entry_id: u64,
    summary: String,
    level: ToastLevel,
    cx: &mut Context<Workspace>,
) {
    let store = store.clone();
    ws.show_action_toast_level(
        summary,
        level,
        "Undo",
        UNDO_TOAST_DURATION,
        move |_, window, cx| undo_entry(&store, entry_id, UndoMode::Check, window, cx),
        cx,
    );
}

/// Undoes journal entry `id` in the background, then refreshes the git views and toasts
/// the result. Files changed since → a confirmation offers "Restore anyway".
pub fn undo_entry(
    store: &Entity<GitStore>,
    id: u64,
    mode: UndoMode,
    window: &mut Window,
    cx: &mut App,
) {
    let journal = journal(cx);
    let ctx = context_for(store, cx);
    let work = cx.background_spawn(async move { journal.undo(id, &ctx, mode) });
    let store = store.clone();
    window
        .spawn(cx, async move |cx| {
            let result = work.await;
            let _ = cx.update(|window, cx| {
                notify_repo_changed(&store, cx);
                match result {
                    Ok(entry) => {
                        let redo = entry.undo.is_some();
                        let id = entry.id;
                        let message = entry.summary.clone();
                        match (redo, store.read(cx).workspace.upgrade()) {
                            (true, Some(ws)) => {
                                let store = store.clone();
                                ws.update(cx, |ws, cx| {
                                    ws.show_action_toast(
                                        message,
                                        "Redo",
                                        UNDO_TOAST_DURATION,
                                        move |_, window, cx| {
                                            undo_entry(&store, id, UndoMode::Check, window, cx)
                                        },
                                        cx,
                                    )
                                });
                            }
                            _ => toast(&store, message, cx),
                        }
                    }
                    Err(UndoError::Changed { paths, .. }) => {
                        ask_restore_anyway(&store, id, paths, window, cx)
                    }
                    Err(err) => toast(&store, err.to_string(), cx),
                }
            });
        })
        .detach();
}

/// The choice an undo turns into when files changed since the operation.
fn ask_restore_anyway(
    store: &Entity<GitStore>,
    id: u64,
    paths: Vec<String>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    let spec = ConfirmSpec {
        title: "These files changed since — restore anyway?".into(),
        message: vec![
            "Undoing would overwrite changes made after the operation.".into(),
            "Lector backs the current contents up first, so restoring anyway can itself be undone."
                .into(),
        ],
        items: paths.into_iter().map(SharedString::from).collect(),
        confirm_label: "Restore anyway".into(),
    };
    let store = store.clone();
    confirm(&ws, spec, window, cx, move |window, cx| {
        undo_entry(&store, id, UndoMode::Force, window, cx)
    });
}

/// `workspace::UndoLastOperation`: undoes the newest undoable operation in this repository.
pub fn undo_last(store: &Entity<GitStore>, window: &mut Window, cx: &mut App) {
    let repo = store.read(cx).repo().map(|r| r.workdir().to_path_buf());
    match journal(cx).last_undoable(repo.as_deref()) {
        Some(entry) => undo_entry(store, entry.id, UndoMode::Check, window, cx),
        None => toast(store, "Nothing to undo", cx),
    }
}

/// Registers `workspace::ShowActivity` and `workspace::UndoLastOperation`.
pub fn register(ws: &mut Workspace, store: &Entity<GitStore>) {
    let s = store.clone();
    ws.register_action(
        move |ws, _: &lector_ui::actions::workspace::ShowActivity, window, cx| {
            show_in(ws, &s, window, cx)
        },
    );
    let s = store.clone();
    ws.register_action(
        move |_, _: &lector_ui::actions::workspace::UndoLastOperation, window, cx| {
            // The workspace is being updated: toasts must wait until it is free again.
            let s = s.clone();
            window.defer(cx, move |window, cx| undo_last(&s, window, cx));
        },
    );
}

/// Opens (or activates) the Activity log tab.
pub fn open_activity(store: &Entity<GitStore>, window: &mut Window, cx: &mut App) {
    let Some(ws) = store.read(cx).workspace.upgrade() else {
        return;
    };
    ws.update(cx, |ws, cx| show_in(ws, store, window, cx));
}

/// [`open_activity`] from inside a workspace update (action handlers).
fn show_in(
    ws: &mut Workspace,
    store: &Entity<GitStore>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let key: SharedString = KEY.into();
    let pane = ws.active_pane().clone();
    if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
        pane.update(cx, |p, cx| {
            p.pin(ix, cx);
            p.activate(ix, true, window, cx)
        });
        return;
    }
    let store = store.clone();
    let view = cx.new(|cx| ActivityView::new(store, cx));
    ws.add_item(Box::new(ActivityItem { view }), false, window, cx);
}

/// Recreates the tab from the session.
pub fn restore_item(
    store: &Entity<GitStore>,
    kind: &lector_config::TabKind,
    cx: &mut App,
) -> Option<Box<dyn PaneItem>> {
    match kind {
        lector_config::TabKind::Other { label } if label == KEY => {
            let store = store.clone();
            let view = cx.new(|cx| ActivityView::new(store, cx));
            Some(Box::new(ActivityItem { view }))
        }
        _ => None,
    }
}

struct Row {
    entry: ActivityEntry,
    status: UndoStatus,
}

/// The Activity log view.
pub struct ActivityView {
    store: Entity<GitStore>,
    rows: Vec<Row>,
    expanded: HashSet<u64>,
    loaded: bool,
    load_task: Option<Task<()>>,
    _watch_task: Option<Task<()>>,
    _subscription: Option<JournalSubscription>,
    focus_handle: FocusHandle,
}

impl Focusable for ActivityView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ActivityView {
    /// A view of the journal entries of the store's repository.
    pub fn new(store: Entity<GitStore>, cx: &mut Context<Self>) -> Self {
        let journal = journal(cx);
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<()>();
        let subscription = journal.subscribe(move |_: &JournalEvent| {
            let _ = tx.unbounded_send(());
        });
        let watch = cx.spawn(async move |this, cx| {
            while rx.next().await.is_some() {
                // Coalesce bursts (an undo records several events).
                while rx.try_recv().is_ok() {}
                if this.update(cx, |this, cx| this.reload(cx)).is_err() {
                    break;
                }
            }
        });
        let mut view = ActivityView {
            store,
            rows: Vec::new(),
            expanded: HashSet::new(),
            loaded: false,
            load_task: None,
            _watch_task: Some(watch),
            _subscription: Some(subscription),
            focus_handle: cx.focus_handle().tab_stop(true),
        };
        view.reload(cx);
        view
    }

    fn repo_path(&self, cx: &App) -> Option<PathBuf> {
        self.store
            .read(cx)
            .repo()
            .map(|r| r.workdir().to_path_buf())
    }

    /// Re-reads the journal (and which backups still exist) in the background.
    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let journal = journal(cx);
        let repo = self.repo_path(cx);
        let work = cx.background_spawn(async move {
            journal
                .list(LIMIT)
                .into_iter()
                .filter(|e| e.repo.is_none() || e.repo == repo)
                .enumerate()
                .map(|(i, entry)| {
                    let status = if i < CHECKED {
                        journal.undo_status(&entry)
                    } else if entry.undone {
                        UndoStatus::Unavailable("Already undone".into())
                    } else if entry.undo.is_none() {
                        UndoStatus::Unavailable("This operation has no undo".into())
                    } else {
                        UndoStatus::Available
                    };
                    Row { entry, status }
                })
                .collect::<Vec<_>>()
        });
        self.load_task = Some(cx.spawn(async move |this, cx| {
            let rows = work.await;
            let _ = this.update(cx, |this, cx| {
                this.rows = rows;
                this.loaded = true;
                this.load_task = None;
                cx.notify();
            });
        }));
    }

    fn toggle(&mut self, id: u64, cx: &mut Context<Self>) {
        if !self.expanded.remove(&id) {
            self.expanded.insert(id);
        }
        cx.notify();
    }

    /// Opens a comparison of what the operation overwrote (backup) against what is there now.
    fn open_file(
        &self,
        entry: &ActivityEntry,
        rel: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(repo_path) = entry.repo.clone() else {
            return;
        };
        let record = entry.undo.clone();
        let summary = entry.summary.clone();
        let data_dir = journal(cx).data_dir().to_path_buf();
        let work = cx.background_spawn(async move {
            let repo = Repo::discover(&repo_path).map_err(|e| e.to_string())?;
            let current = std::fs::read(repo.workdir().join(&rel)).ok();
            let snaps = record
                .as_ref()
                .map(UndoRecord::snapshots)
                .unwrap_or_default();
            let shadow = ShadowRepo::open(&data_dir, &repo).map_err(|e| e.to_string())?;
            let mut before = None;
            let mut index_now = None;
            for snap in &snaps {
                match shadow.kind(snap) {
                    Ok(lector_git::shadow::SnapshotKind::Index) => {
                        if let Some(rec) = shadow
                            .index_entries(snap)
                            .ok()
                            .and_then(|e| e.into_iter().find(|r| r.path == rel && r.stage <= 1))
                        {
                            before = shadow.load(&Side::Blob(rec.oid)).ok().flatten();
                            index_now = repo.load(&Side::Index(rel.clone())).ok().flatten();
                        }
                    }
                    Ok(_) => {
                        if let Ok(Some(bytes)) = shadow.file_at(snap, &rel) {
                            before = Some(bytes);
                        }
                    }
                    Err(_) => {}
                }
                if before.is_some() {
                    break;
                }
            }
            let (Some(before), after) = (before, index_now.or(current)) else {
                return Err(
                    "Nothing was recorded for this file (it did not exist before)".to_string(),
                );
            };
            Ok::<_, String>((rel, before, after.unwrap_or_default()))
        });
        let store = self.store.clone();
        window
            .spawn(cx, async move |cx| {
                let result = work.await;
                let _ = cx.update(|window, cx| match result {
                    Ok((rel, before, after)) => {
                        let title = format!("{rel}: before → after ({summary})");
                        let item = lector_ui::editor::compare::CompareItem::new(
                            title,
                            Some(PathBuf::from(&rel)),
                            &String::from_utf8_lossy(&before),
                            &String::from_utf8_lossy(&after),
                            cx,
                        );
                        lector_ui::editor::compare::open_compare_item(item, window, cx);
                    }
                    Err(msg) => toast(&store, msg, cx),
                });
            })
            .detach();
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> impl IntoElement {
        let row = &self.rows[ix];
        let entry = &row.entry;
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let id = entry.id;
        let secs = entry
            .time
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let expanded = self.expanded.contains(&id);
        let workdir = entry.repo.clone();
        let rels: Vec<String> = entry
            .paths
            .iter()
            .map(|p| {
                workdir
                    .as_ref()
                    .and_then(|w| p.strip_prefix(w).ok())
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        let meta = format!(
            "{} · {}{}",
            relative_time(secs, now_secs()),
            match rels.len() {
                0 => "no files".to_string(),
                1 => rels[0].clone(),
                n => format!("{n} files"),
            },
            if entry.is_undo() { " · undo" } else { "" }
        );
        let available = row.status.is_available();
        let reason = match &row.status {
            UndoStatus::Unavailable(why) => why.clone(),
            UndoStatus::Available => "Restore what this operation overwrote".to_string(),
        };
        let undone = entry.undone;
        let faded = undone;
        let summary_color = if faded {
            ui.text_faint.hsla()
        } else {
            ui.text.hsla()
        };
        let files: Vec<(String, SharedString)> = rels
            .iter()
            .map(|r| (r.clone(), SharedString::from(r.clone())))
            .collect();
        let can_open = entry.undo.is_some() && entry.repo.is_some();
        let entry_for_files = entry.clone();
        v_flex()
            .id(("activity-row", id as usize))
            .border_b_1()
            .border_color(ui.border_variant.hsla())
            .child(
                h_flex()
                    .px(px(14.))
                    .py(px(9.))
                    .gap(px(10.))
                    .items_start()
                    .hover(|s| s.bg(ui.hover.hsla()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _, _, cx| this.toggle(id, cx)),
                    )
                    .child(
                        div().pt(px(2.)).child(
                            Icon::new(if undone {
                                IconName::Undo
                            } else {
                                IconName::Clock
                            })
                            .size(IconSize::Small)
                            .color(ui.text_faint.hsla()),
                        ),
                    )
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(metrics::text_sm())
                                    .font_weight(FontWeight::MEDIUM)
                                    .text_color(summary_color)
                                    .child(entry.summary.clone()),
                            )
                            .child(
                                div()
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.text_faint.hsla())
                                    .child(meta),
                            ),
                    )
                    .child(if undone {
                        div()
                            .px(px(8.))
                            .py(px(2.))
                            .rounded_full()
                            .bg(ui.hover.hsla())
                            .text_size(metrics::text_xs())
                            .text_color(ui.text_muted.hsla())
                            .child("Undone")
                            .into_any_element()
                    } else {
                        let store = self.store.clone();
                        Button::new(("activity-undo", id as usize), "Undo")
                            .icon(IconName::Undo)
                            .style(ButtonStyle::Outlined)
                            .compact()
                            .disabled(!available)
                            .tooltip(Tooltip::text(reason))
                            .on_click(move |_, window, cx| {
                                undo_entry(&store, id, UndoMode::Check, window, cx)
                            })
                            .into_any_element()
                    }),
            )
            .when(expanded, |el| {
                el.child(
                    v_flex()
                        .pl(px(44.))
                        .pr(px(14.))
                        .pb(px(8.))
                        .gap(px(2.))
                        .children(files.into_iter().enumerate().map(|(i, (rel, label))| {
                            let entry = entry_for_files.clone();
                            div()
                                .id(("activity-file", id as usize * 10_000 + i))
                                .px(px(6.))
                                .py(px(2.))
                                .rounded(metrics::radius())
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_muted.hsla())
                                .when(can_open, |d| {
                                    d.cursor_pointer()
                                        .tab_index(0)
                                        .focus(lector_ui::a11y::ring_focus(cx))
                                        .hover(|s| s.bg(ui.hover.hsla()))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.open_file(&entry, rel.clone(), window, cx)
                                        }))
                                })
                                .child(label)
                        }))
                        .when(!can_open, |el| {
                            el.child(
                                div()
                                    .px(px(6.))
                                    .text_size(metrics::text_xs())
                                    .text_color(ui.text_faint.hsla())
                                    .child("No before/after recorded for this operation"),
                            )
                        }),
                )
            })
    }
}

impl Render for ActivityView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let count = self.rows.len();
        let rows: Vec<_> = (0..count).map(|ix| self.render_row(ix, cx)).collect();
        let store = self.store.clone();
        let undoable = self
            .rows
            .iter()
            .any(|r| r.entry.undo.is_some() && !r.entry.undone && !r.entry.is_undo());
        v_flex()
            .size_full()
            .bg(ui.background.hsla())
            .track_focus(&self.focus_handle)
            .child(
                h_flex()
                    .flex_none()
                    .px(px(14.))
                    .py(px(10.))
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(ui.border.hsla())
                    .bg(ui.surface.hsla())
                    .child(
                        div()
                            .text_size(metrics::text())
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(ui.text.hsla())
                            .child("Activity"),
                    )
                    .child(
                        div()
                            .text_size(metrics::text_sm())
                            .text_color(ui.text_faint.hsla())
                            .child("Everything Lector wrote to your files or repository"),
                    )
                    .child(div().flex_1())
                    .child(
                        Button::new("activity-undo-last", "Undo last operation")
                            .icon(IconName::Undo)
                            .style(ButtonStyle::Subtle)
                            .compact()
                            .disabled(!undoable)
                            .on_click(move |_, window, cx| undo_last(&store, window, cx)),
                    ),
            )
            .child(
                div()
                    .id("activity-list")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .children(rows)
                    .when(count == 0, |el| {
                        el.child(
                            div()
                                .p(px(24.))
                                .text_size(metrics::text_sm())
                                .text_color(ui.text_faint.hsla())
                                .child(if self.loaded {
                                    "Nothing yet. Stage, discard or commit something and it shows up here, with an Undo button when it can be undone."
                                } else {
                                    "Loading…"
                                }),
                        )
                    }),
            )
    }
}

/// The pane item.
pub struct ActivityItem {
    view: Entity<ActivityView>,
}

impl PaneItem for ActivityItem {
    fn title(&self, _: &App) -> SharedString {
        "Activity".into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Clock)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(KEY.into())
    }

    fn tab_kind(&self, _: &App) -> Option<lector_config::TabKind> {
        Some(lector_config::TabKind::Other { label: KEY.into() })
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.focus_handle(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let store = self.view.read(cx).store.clone();
        let view = cx.new(|cx| ActivityView::new(store, cx));
        Some(Box::new(ActivityItem { view }))
    }
}
