//! The diff view as a pane tab: [`DiffItem`], opening a diff in the
//! workspace ([`open_diff`]), restoring a saved diff tab, and the diff
//! options taken from settings.

use std::path::PathBuf;

use gpui::{
    prelude::*, AnyView, App, Entity, FocusHandle, Focusable, SharedString, WeakEntity, Window,
};
use lector_diff::DiffOptions;
use lector_review::review::{self, diff_options, ReviewFile};
use lector_ui::background::{background, view_key, JobKind, Priority};
use lector_ui::icons::IconName;
use lector_ui::views::pane::{PaneItem, TabIcon};
use lector_ui::views::Workspace;

use super::{DiffTarget, DiffView, ViewedTarget};
use crate::store::GitStore;
use crate::util::split_path;

/// A diff in a pane tab.
pub struct DiffItem {
    /// The view.
    pub view: Entity<DiffView>,
    key: SharedString,
}

impl DiffItem {
    /// Wraps `view`; `key` identifies the tab (dedupe).
    pub fn new(view: Entity<DiffView>, key: impl Into<SharedString>) -> Self {
        DiffItem {
            view,
            key: key.into(),
        }
    }
}

impl PaneItem for DiffItem {
    fn title(&self, cx: &App) -> SharedString {
        let v = self.view.read(cx);
        if let Some(t) = &v.title_override {
            return t.clone();
        }
        let name = v
            .target
            .as_ref()
            .map(|t| split_path(t.path()).0.to_string())
            .unwrap_or_else(|| "Diff".into());
        format!("{name} (diff)").into()
    }

    fn icon(&self, _: &App) -> TabIcon {
        TabIcon::Ui(IconName::Diff)
    }

    fn path(&self, _: &App) -> Option<PathBuf> {
        None
    }

    fn item_key(&self, _: &App) -> Option<SharedString> {
        Some(self.key.clone())
    }

    fn tab_kind(&self, cx: &App) -> Option<lector_app::store::TabKind> {
        let v = self.view.read(cx);
        match (v.target.as_ref()?, &v.viewed) {
            (DiffTarget::FileVsRev { path, rev }, _) => Some(lector_app::store::TabKind::Other {
                label: format!("vs:{rev}:{path}"),
            }),
            (DiffTarget::Change(change), Some(viewed)) => Some(lector_app::store::TabKind::Diff {
                changeset_key: viewed.key.clone(),
                path: change.path().to_string(),
            }),
            (DiffTarget::Change(_), None) => None,
        }
    }

    fn view(&self) -> AnyView {
        self.view.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.view.read(cx).focus_handle(cx)
    }

    fn clone_item(&self, _window: &mut Window, cx: &mut App) -> Option<Box<dyn PaneItem>> {
        let (store, target, viewed, title) = {
            let v = self.view.read(cx);
            (
                v.store.clone(),
                v.target.clone()?,
                v.viewed.clone(),
                v.title_override.clone(),
            )
        };
        let view = cx.new(|cx| {
            let mut v = DiffView::new(store, true, cx);
            v.set_title(title);
            v.set_target(target, viewed, cx);
            v
        });
        Some(Box::new(DiffItem {
            view,
            key: self.key.clone(),
        }))
    }
}

/// Opens a diff tab for `target` in `workspace` (activating an existing one).
#[allow(clippy::too_many_arguments)]
pub fn open_diff(
    workspace: &WeakEntity<Workspace>,
    store: &Entity<GitStore>,
    target: DiffTarget,
    viewed: Option<ViewedTarget>,
    title: Option<SharedString>,
    key: String,
    preview: bool,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(ws) = workspace.upgrade() else {
        return;
    };
    let pane = ws.read(cx).active_pane().clone();
    let key: SharedString = format!("diff:{key}").into();
    if let Some(ix) = pane.read(cx).index_for_key(&key, cx) {
        pane.update(cx, |p, cx| {
            if !preview {
                p.pin(ix, cx);
            }
            p.activate(ix, true, window, cx);
        });
        return;
    }
    let store = store.clone();
    let view = cx.new(|cx| {
        let mut v = DiffView::new(store, true, cx);
        v.set_title(title);
        v.set_target(target, viewed, cx);
        v
    });
    let item = Box::new(DiffItem::new(view, key));
    ws.update(cx, |ws, cx| ws.add_item(item, preview, window, cx));
}

/// Recreates a diff tab from the session (the change is looked up again).
pub fn restore_item(
    store: &Entity<GitStore>,
    kind: &lector_app::store::TabKind,
    cx: &mut App,
) -> Option<Box<dyn PaneItem>> {
    match kind {
        lector_app::store::TabKind::Other { label } => {
            let (rev, path) = label.strip_prefix("vs:")?.split_once(':')?;
            let (rev, path) = (rev.to_string(), path.to_string());
            let name = split_path(&path).0.to_string();
            let short = if rev.len() == 40 {
                rev[..7].to_string()
            } else {
                rev.clone()
            };
            let title: SharedString = format!("{name} vs {short}").into();
            let key = format!("diff:vs:{rev}:{path}");
            let store = store.clone();
            let view = cx.new(|cx| {
                let mut v = DiffView::new(store, true, cx);
                v.set_title(Some(title));
                v.set_target(DiffTarget::FileVsRev { path, rev }, None, cx);
                v
            });
            Some(Box::new(DiffItem::new(view, key)))
        }
        lector_app::store::TabKind::Diff {
            changeset_key,
            path,
        } => {
            let cs = review::parse_changeset_key(changeset_key)?;
            let repo = store.read(cx).repo()?.clone();
            let opts = settings_diff_options(store.read(cx), cx);
            let key = format!("diff:{changeset_key}:{path}");
            let (ck, path) = (changeset_key.clone(), path.clone());
            let store = store.clone();
            let service = lector_ui::commands_bridge::core(cx).review();
            let view = cx.new(|cx| {
                let svc = service.clone();
                let key = view_key(JobKind::GitFileSummary, cx.entity_id());
                let task = background(cx, key, Priority::Visible, move |cancel| {
                    let change = repo
                        .changes(&cs)
                        .ok()?
                        .into_iter()
                        .find(|c| c.path() == path)?;
                    let summary = svc.file_summary(&repo, &change, &opts, cancel);
                    let (stats, binary, hash) = summary.ok()?;
                    Some((change, stats, binary, hash))
                });
                cx.spawn(async move |this, cx| {
                    let Ok(Some((change, stats, binary, hash))) = task.await else {
                        return;
                    };
                    let _ = this.update(cx, |v: &mut DiffView, cx| {
                        let viewed = {
                            let s = v.store.read(cx);
                            s.repo()
                                .map(|r| service.is_viewed(r.workdir(), &ck, change.path(), &hash))
                        }
                        .unwrap_or(false);
                        let file = ReviewFile {
                            display_path: change.path().to_string(),
                            change: change.clone(),
                            stats,
                            viewed,
                            content_hash: hash,
                            binary,
                        };
                        v.set_target(
                            DiffTarget::Change(change),
                            Some(ViewedTarget {
                                key: ck.clone(),
                                file,
                            }),
                            cx,
                        );
                    });
                })
                .detach();
                DiffView::new(store, true, cx)
            });
            Some(Box::new(DiffItem::new(view, key)))
        }
        _ => None,
    }
}

/// Default diff options from settings (used by the panels for stats).
pub fn settings_diff_options(store: &GitStore, cx: &App) -> DiffOptions {
    diff_options(store.settings(cx).diff.ignore_whitespace)
}
