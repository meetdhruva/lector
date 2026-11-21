//! Session persistence of the [`Workspace`]: debounced saving, building the
//! [`Session`] snapshot and restoring panes and tabs from one.

use std::time::Duration;

use gpui::{App, Axis, Window};
use lector_app::store::{PaneState, Session, TabKind, TabState};

use super::LINKED_FILE_READ_ONLY;
use crate::editor::Reveal;
use crate::settings::AppSettings;
use crate::views::pane::{new_pane, PaneNode};
use crate::views::sidebar::SidebarPanel;
use crate::views::workspace::Workspace;

impl Workspace {
    pub(in crate::views::workspace) fn schedule_session_save(
        &mut self,
        cx: &mut gpui::Context<Self>,
    ) {
        if !self.integ.session_ready {
            return;
        }
        self.integ.session_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(1000))
                .await;
            let _ = this.update(cx, |this, cx| this.save_session_now(cx));
        }));
    }

    pub(crate) fn session(&self, cx: &App) -> Session {
        let panes = self.center.panes();
        let active_pane = panes
            .iter()
            .position(|p| *p == self.active_pane)
            .unwrap_or(0);
        let axis = match &self.center {
            PaneNode::Split { state, .. } if state.read(cx).axis() == Axis::Vertical => {
                lector_app::store::Axis::Vertical
            }
            _ => lector_app::store::Axis::Horizontal,
        };
        let panes = panes
            .iter()
            .map(|pane| {
                let p = pane.read(cx);
                let active = p.active_index();
                PaneState {
                    tabs: p
                        .items()
                        .enumerate()
                        .filter_map(|(ix, item)| {
                            let kind = item.tab_kind(cx)?;
                            let scroll_row = item
                                .code_view()
                                .map(|v| v.read(cx).top_visible_row())
                                .unwrap_or(0);
                            let read_only_locked = item
                                .file_view()
                                .is_some_and(|v| v.read(cx).read_only_lock().is_some());
                            Some(TabState {
                                kind,
                                scroll_row,
                                active: ix == active,
                                read_only_locked,
                            })
                        })
                        .collect(),
                }
            })
            .collect();
        Session {
            panes,
            axis,
            active_pane,
            sidebar_visible: self.sidebar_visible,
            sidebar_width: f32::from(self.sidebar_width),
            sidebar_tab: self.sidebar.read(cx).active_panel().title().to_string(),
            window: None,
        }
    }

    pub(crate) fn save_session_now(&mut self, cx: &mut gpui::Context<Self>) {
        if !self.has_folder || !self.integ.session_ready || !AppSettings::get(cx).session.restore {
            return;
        }
        let session = self.session(cx);
        let root = self.project.root().to_path_buf();
        if let Err(err) = crate::commands_bridge::session_store().save(&root, &session) {
            tracing::warn!("session save failed: {err}");
        }
    }

    /// Restores tabs, splits and sidebar state from the last session.
    /// Returns whether anything was restored.
    pub fn restore_session(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> bool {
        let restore = AppSettings::get(cx).session.restore && self.has_folder;
        let session = if restore {
            crate::commands_bridge::session_store().load(self.project.root())
        } else {
            None
        };
        let Some(session) = session else {
            self.integ.session_ready = true;
            return false;
        };
        self.restore_session_from(session, window, cx)
    }

    /// Restores `session` (tabs, splits, sidebar). Returns whether any tab came back.
    pub(crate) fn restore_session_from(
        &mut self,
        session: Session,
        window: &mut Window,
        cx: &mut gpui::Context<Self>,
    ) -> bool {
        self.sidebar_visible = session.sidebar_visible;
        if session.sidebar_width >= 120. {
            self.sidebar_width = gpui::px(session.sidebar_width.min(900.));
        }
        if let Some(panel) = SidebarPanel::ALL
            .iter()
            .find(|p| p.title() == session.sidebar_tab)
        {
            self.sidebar
                .update(cx, |s, cx| s.set_active_panel(*panel, cx));
        }
        let axis = match session.axis {
            lector_app::store::Axis::Vertical => Axis::Vertical,
            _ => Axis::Horizontal,
        };
        let mut restored_any = false;
        let mut panes = vec![self.active_pane.clone()];
        for (ix, pane_state) in session.panes.iter().enumerate() {
            if pane_state.tabs.is_empty() {
                continue;
            }
            if ix > 0 && restored_any {
                let last = panes
                    .last()
                    .cloned()
                    .unwrap_or_else(|| self.active_pane.clone());
                let new = new_pane(Some(self.root.clone()), cx);
                self.center.split(&last, new.clone(), axis, cx);
                let sub = Self::subscribe_pane(&new, window, cx);
                self._subscriptions.push(sub);
                panes.push(new.clone());
                self.set_active_pane(new, cx);
            }
            let mut active_ix = None;
            for tab in &pane_state.tabs {
                let opened = match &tab.kind {
                    TabKind::File { path } => {
                        if !path.is_file() {
                            continue;
                        }
                        // A tab that was locked read-only (a link outside the project)
                        // comes back locked.
                        let lock = tab.read_only_locked.then(|| LINKED_FILE_READ_ONLY.into());
                        let view =
                            self.open_file_view_locked(path.clone(), false, lock, window, cx);
                        view.update(cx, |v, cx| {
                            v.reveal(
                                Reveal {
                                    row: tab.scroll_row,
                                    cols: 0..0,
                                    center: false,
                                },
                                cx,
                            )
                        });
                        true
                    }
                    kind => {
                        let restorers = std::mem::take(&mut self.integ.restorers);
                        let item = restorers.iter().find_map(|r| r(kind, window, cx));
                        self.integ.restorers = restorers;
                        match item {
                            Some(item) => {
                                self.active_pane
                                    .update(cx, |p, cx| p.add_item(item, false, window, cx));
                                true
                            }
                            None => false,
                        }
                    }
                };
                if opened {
                    restored_any = true;
                    if tab.active {
                        active_ix = Some(self.active_pane.read(cx).active_index());
                    }
                }
            }
            if let Some(ix) = active_ix {
                self.active_pane
                    .update(cx, |p, cx| p.activate(ix, false, window, cx));
            }
        }
        let panes = self.center.panes();
        if let Some(p) = panes.get(session.active_pane).cloned() {
            self.set_active_pane(p, cx);
        }
        if let Some(item) = self.active_pane.read(cx).active_item() {
            let focus = item.focus_handle(cx);
            window.focus(&focus);
        }
        self.integ.session_ready = true;
        cx.notify();
        restored_any
    }
}
