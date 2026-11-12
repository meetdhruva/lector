//! Keyboard-only operation of the window: F6 region cycling, Tab between controls, the
//! interface zoom and the reduced-motion switch. A child module of `workspace` so it can
//! reach the workspace's private state.
//!
//! Regions, in F6 order: the title bar (the menu bar on Linux / Windows, then the toolbar),
//! the activity bar, the sidebar panel, every editor pane, the status bar. Hidden regions
//! (sidebar toggled off) are skipped. While a modal is open Tab and F6 stay inside it.

use gpui::{prelude::*, App, Context, Focusable, Window};

use super::Workspace;
use crate::actions::workspace::*;

/// A part of the window F6 moves between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Region {
    /// Menu bar and toolbar.
    TitleBar,
    /// The icon column that switches sidebar panels.
    ActivityBar,
    /// The active sidebar panel.
    Sidebar,
    /// Editor pane `n` (layout order).
    Pane(usize),
    /// The panel below the editor area (Terminal), when shown.
    BottomPanel,
    /// The status bar.
    StatusBar,
}

impl Workspace {
    /// The regions F6 visits, in order.
    pub fn regions(&self, cx: &App) -> Vec<Region> {
        let mut out = vec![Region::TitleBar];
        if self.sidebar_visible {
            out.push(Region::ActivityBar);
            out.push(Region::Sidebar);
        }
        out.extend((0..self.center.panes().len()).map(Region::Pane));
        if self.is_bottom_panel_visible() {
            out.push(Region::BottomPanel);
        }
        out.push(Region::StatusBar);
        let _ = cx;
        out
    }

    /// The region that currently holds keyboard focus, if any.
    pub fn focused_region(&self, window: &Window, cx: &App) -> Option<Region> {
        if self.title_focus.contains_focused(window, cx)
            || self.app_menu.focus_handle(cx).contains_focused(window, cx)
        {
            return Some(Region::TitleBar);
        }
        let sidebar = self.sidebar.read(cx);
        if sidebar.activity_handle().contains_focused(window, cx) {
            return Some(Region::ActivityBar);
        }
        if sidebar.panel_handle().contains_focused(window, cx)
            || sidebar.focus_handle(cx).contains_focused(window, cx)
        {
            return Some(Region::Sidebar);
        }
        for (ix, pane) in self.center.panes().iter().enumerate() {
            if pane.read(cx).region_handle().contains_focused(window, cx) {
                return Some(Region::Pane(ix));
            }
        }
        if self
            .bottom_panel_focus()
            .is_some_and(|f| f.contains_focused(window, cx))
        {
            return Some(Region::BottomPanel);
        }
        if self.status_focus.contains_focused(window, cx) {
            return Some(Region::StatusBar);
        }
        None
    }

    /// Moves focus into `region`.
    pub fn focus_region(&mut self, region: Region, window: &mut Window, cx: &mut Context<Self>) {
        match region {
            Region::TitleBar => {
                if cfg!(target_os = "macos") {
                    // The menu bar is the system's; land on the first toolbar control.
                    self.focus_first_stop_in(&self.title_focus.clone(), window, cx);
                } else {
                    self.app_menu.update(cx, |m, cx| m.focus_bar(window, cx));
                }
            }
            Region::ActivityBar => {
                self.sidebar
                    .update(cx, |s, cx| s.focus_activity_bar(window, cx));
            }
            Region::Sidebar => {
                self.sidebar.update(cx, |s, cx| s.focus_panel(window, cx));
            }
            Region::Pane(ix) => {
                let Some(pane) = self.center.panes().get(ix).cloned() else {
                    return;
                };
                self.set_active_pane(pane.clone(), cx);
                if pane.read(cx).item_count() == 0 {
                    self.focus_welcome_if_empty(window, cx);
                } else {
                    let handle = pane.focus_handle(cx);
                    window.focus(&handle);
                }
            }
            Region::BottomPanel => {
                if let Some(focus) = self.bottom_panel_focus() {
                    window.focus(&focus);
                }
            }
            Region::StatusBar => {
                self.focus_first_stop_in(&self.status_focus.clone(), window, cx);
            }
        }
        cx.notify();
    }

    /// Focuses `container`, then its first tab stop (the container stays focused when it has
    /// none, so the ring still shows).
    fn focus_first_stop_in(
        &mut self,
        container: &gpui::FocusHandle,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(container);
        window.focus_next();
        if !container.contains_focused(window, cx) {
            window.focus(container);
        }
    }

    /// F6 (`delta` = 1) and Shift-F6 (`delta` = -1): the next / previous region. With
    /// nothing focused, starts at the editor. Does nothing while a modal is open.
    pub fn cycle_region(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        if self.modal_layer.read(cx).has_active_modal() {
            return;
        }
        let regions = self.regions(cx);
        let n = regions.len() as isize;
        let current = self
            .focused_region(window, cx)
            .and_then(|r| regions.iter().position(|x| *x == r));
        let next = match current {
            Some(ix) => (ix as isize + delta).rem_euclid(n) as usize,
            None => regions
                .iter()
                .position(|r| matches!(r, Region::Pane(_)))
                .unwrap_or(0),
        };
        // Leaving the menu bar with a menu open: close it without moving focus back first.
        if self.app_menu.read(cx).is_open() {
            self.app_menu.update(cx, |m, cx| m.close(window, cx));
        }
        self.focus_region(regions[next], window, cx);
    }

    /// Tab / Shift-Tab: the next / previous focusable control. Inside a modal the focus
    /// wraps around within it.
    pub fn focus_control(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let modal = self.modal_layer.read(cx).modal_container().cloned();
        let before = window.focused(cx);
        let step = |window: &mut Window| {
            if forward {
                window.focus_next()
            } else {
                window.focus_prev()
            }
        };
        match modal {
            None => step(window),
            Some(container) => {
                // At most one lap over every tab stop of the window.
                for _ in 0..256 {
                    step(window);
                    if container.contains_focused(window, cx) {
                        return;
                    }
                }
                if let Some(before) = before {
                    window.focus(&before);
                }
            }
        }
    }

    /// Zoom UI In / Out / Reset: `Some(true)` larger, `Some(false)` smaller, `None` 100 %.
    pub fn ui_zoom(&mut self, larger: Option<bool>, cx: &mut Context<Self>) {
        let current = crate::scale::scale();
        let next = match larger {
            Some(true) => crate::scale::zoom_in(current),
            Some(false) => crate::scale::zoom_out(current),
            None => 1.0,
        };
        crate::scale::set_runtime_scale(next, cx);
        self.show_toast(
            format!(
                "Interface scale {}%",
                (crate::scale::scale() * 100.0).round() as u32
            ),
            cx,
        );
    }

    /// Switches reduced motion on or off for this session (the setting in `settings.toml`
    /// is what persists).
    pub fn toggle_reduce_motion(&mut self, cx: &mut Context<Self>) {
        let on = !crate::a11y::reduce_motion(cx);
        let current = crate::settings::AppSettings::get(cx);
        let mut settings = (*current).clone();
        settings.ui.reduce_motion = Some(on);
        let warnings = cx
            .try_global::<crate::settings::AppSettings>()
            .map(|g| g.warnings.clone())
            .unwrap_or_default();
        cx.set_global(crate::settings::AppSettings {
            settings: std::sync::Arc::new(settings),
            warnings,
        });
        cx.refresh_windows();
        self.show_toast(
            if on {
                "Reduced motion on"
            } else {
                "Reduced motion off"
            },
            cx,
        );
    }

    /// Registers the handlers of this module on the workspace element.
    pub(super) fn register_region_actions(
        &mut self,
        div: gpui::Div,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        div.on_action(cx.listener(|this, _: &FocusNextRegion, w, cx| this.cycle_region(1, w, cx)))
            .on_action(cx.listener(|this, _: &FocusPrevRegion, w, cx| this.cycle_region(-1, w, cx)))
            .on_action(
                cx.listener(|this, _: &FocusNextControl, w, cx| this.focus_control(true, w, cx)),
            )
            .on_action(
                cx.listener(|this, _: &FocusPrevControl, w, cx| this.focus_control(false, w, cx)),
            )
            .on_action(cx.listener(|this, _: &UiZoomIn, _, cx| this.ui_zoom(Some(true), cx)))
            .on_action(cx.listener(|this, _: &UiZoomOut, _, cx| this.ui_zoom(Some(false), cx)))
            .on_action(cx.listener(|this, _: &UiZoomReset, _, cx| this.ui_zoom(None, cx)))
            .on_action(
                cx.listener(|this, _: &ToggleReduceMotion, _, cx| this.toggle_reduce_motion(cx)),
            )
    }
}
