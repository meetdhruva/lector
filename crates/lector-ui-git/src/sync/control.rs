//! Views over the [`SyncController`]: the status bar control
//! ([`SyncControl`] and its dropdown, [`sync_menu`]) and the Changes panel
//! bar ([`SyncPanelBar`]).

use std::time::SystemTime;

use gpui::{
    div, prelude::*, App, Corner, Entity, MouseButton, MouseDownEvent, SharedString, Subscription,
    Window,
};
use lector_ui::actions::{git as git_actions, workspace as ws_actions};
use lector_ui::components::context_menu::ContextMenu;
use lector_ui::components::popover::Popover;
use lector_ui::components::{h_flex, metrics, Button, IconButton, Tooltip};
use lector_ui::icons::{Icon, IconName, IconSize};
use lector_ui::scale::px;
use lector_ui::{ActiveTheme, ColorExt};

use super::SyncController;
use crate::sync_model::{control_view, last_fetched_label};

// ---------------------------------------------------------------------------------------------
// Status bar control
// ---------------------------------------------------------------------------------------------

/// The status bar sync control: branch, ahead/behind, state word; click = the obvious next
/// step; the chevron opens the sync menu; while an operation runs: phase, percent, Cancel.
pub struct SyncControl {
    controller: Entity<SyncController>,
    menu: Popover<ContextMenu>,
    _subscriptions: Vec<Subscription>,
}

impl SyncControl {
    /// The control over `controller`.
    pub fn new(controller: Entity<SyncController>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&controller, |_, _, cx| cx.notify())];
        SyncControl {
            controller,
            menu: Popover::new(),
            _subscriptions: subs,
        }
    }

    /// Opens the sync menu above `position`.
    pub fn deploy_menu(
        &mut self,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let busy = self.controller.read(cx).busy_reason(cx);
        let menu = sync_menu(busy.is_some(), window, cx);
        self.menu.show(
            menu,
            position,
            Corner::BottomLeft,
            window,
            cx,
            |this: &mut Self, _, cx| {
                this.menu.close();
                cx.notify();
            },
        );
        cx.notify();
    }
}

/// The sync menu (status bar dropdown); every item dispatches a `git::*` action.
pub fn sync_menu(busy: bool, window: &mut Window, cx: &mut App) -> Entity<ContextMenu> {
    ContextMenu::build(window, cx, move |menu, _, _| {
        let item =
            |menu: ContextMenu, label: &str, icon: IconName, action: Box<dyn gpui::Action>| {
                if busy {
                    menu.disabled_action(label.to_string(), action)
                } else {
                    menu.icon_action(label.to_string(), icon, action)
                }
            };
        let mut menu = menu.header("Sync");
        menu = item(
            menu,
            "Fetch",
            IconName::Refresh,
            Box::new(git_actions::Fetch),
        );
        menu = item(
            menu,
            "Pull (Fast-Forward)",
            IconName::ArrowDown,
            Box::new(git_actions::Pull),
        );
        menu = item(
            menu,
            "Pull (Rebase)",
            IconName::ArrowDown,
            Box::new(git_actions::PullRebase),
        );
        menu = item(menu, "Push", IconName::ArrowUp, Box::new(git_actions::Push));
        menu = item(
            menu,
            "Push (Force with Lease)…",
            IconName::AlertTriangle,
            Box::new(git_actions::ForcePushWithLease),
        );
        menu = item(
            menu,
            "Publish Branch",
            IconName::Globe,
            Box::new(git_actions::PublishBranch),
        );
        menu = item(
            menu,
            "Set Upstream…",
            IconName::Link,
            Box::new(git_actions::SetUpstream),
        );
        if busy {
            menu = menu.icon_action("Cancel", IconName::Close, Box::new(git_actions::CancelSync));
        }
        menu.separator()
            .icon_action(
                "New Branch…",
                IconName::Plus,
                Box::new(git_actions::NewBranch),
            )
            .icon_action(
                "Switch Branch…",
                IconName::GitBranch,
                Box::new(git_actions::SwitchBranch),
            )
            .icon_action(
                "Branches",
                IconName::ListTree,
                Box::new(ws_actions::ShowBranches),
            )
    })
}

impl Render for SyncControl {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let fg = ui.status_bar_fg.hsla();
        let c = self.controller.read(cx);
        let view = control_view(c.status());
        let fetched = last_fetched_label(c.status().last_fetch, SystemTime::now());
        let auto = lector_ui::settings::AppSettings::get(cx).git.auto_fetch;
        let meta: SharedString = {
            let mut m = fetched;
            if let lector_config::AutoFetch::Minutes(n) = auto {
                m.push_str(&format!(" · auto-fetch every {n} min"));
            }
            if let Some(e) = c.auto_fetch_error() {
                m.push_str(&format!(" · last automatic fetch failed: {e}"));
            }
            m.into()
        };
        let running = c.running().cloned();
        let entity = cx.entity().downgrade();
        let mut row = h_flex().id("status-sync").gap(px(1.)).flex_none();
        if let Some(r) = running {
            row = row
                .child(
                    h_flex()
                        .id("status-sync-progress")
                        .px(px(4.))
                        .gap(px(4.))
                        .text_color(fg)
                        .child(Icon::new(IconName::Loader).size(IconSize::XSmall).color(fg))
                        .child(SharedString::from(r.progress_label()))
                        .tooltip(Tooltip::text("A git network operation is running")),
                )
                .child(
                    IconButton::new("status-sync-cancel", IconName::Close)
                        .icon_size(IconSize::XSmall)
                        .color(fg)
                        .tooltip(Tooltip::for_action(
                            "Cancel (stops git and its helpers)",
                            &git_actions::CancelSync,
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(git_actions::CancelSync), cx)
                        }),
                );
        } else {
            let mut label = view.branch.clone();
            if !view.counts.is_empty() {
                label.push_str(&format!(" {}", view.counts));
            }
            if !view.state_word.is_empty() {
                label.push_str(&format!(" · {}", view.state_word));
            }
            let primary = view.primary;
            row = row.child(
                Button::new("status-branch", label)
                    .compact()
                    .color(fg)
                    .icon(IconName::GitBranch)
                    .tooltip(Tooltip::with_meta(view.tooltip.clone(), meta))
                    .on_click(move |_, window, cx| match primary.action_name() {
                        Some(_) => window.dispatch_action(Box::new(git_actions::Sync), cx),
                        None => window.dispatch_action(Box::new(ws_actions::ShowBranches), cx),
                    }),
            );
        }
        row.child(
            div()
                .id("status-sync-menu")
                .on_mouse_down(MouseButton::Left, move |e: &MouseDownEvent, window, cx| {
                    let _ = entity.update(cx, |this, cx| this.deploy_menu(e.position, window, cx));
                    cx.stop_propagation();
                })
                .child(
                    IconButton::new("status-sync-chevron", IconName::ChevronUp)
                        .icon_size(IconSize::XSmall)
                        .color(fg)
                        .tooltip(Tooltip::for_action(
                            "Sync menu: fetch, pull, push, branches",
                            &git_actions::ShowSyncMenu,
                        )),
                ),
        )
        .children(self.menu.render())
    }
}

// ---------------------------------------------------------------------------------------------
// Changes panel bar
// ---------------------------------------------------------------------------------------------

/// The Fetch / Pull / Push row at the top of the Changes panel, with progress and Cancel
/// while an operation runs.
pub struct SyncPanelBar {
    controller: Entity<SyncController>,
    _subscriptions: Vec<Subscription>,
}

impl SyncPanelBar {
    /// The bar over `controller`.
    pub fn new(controller: Entity<SyncController>, cx: &mut Context<Self>) -> Self {
        let subs = vec![cx.observe(&controller, |_, _, cx| cx.notify())];
        SyncPanelBar {
            controller,
            _subscriptions: subs,
        }
    }
}

impl Render for SyncPanelBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let c = self.controller.read(cx);
        let view = control_view(c.status());
        let busy = c.busy_reason(cx);
        let running = c.running().cloned();
        let mut text = view.branch.clone();
        if !view.counts.is_empty() {
            text.push_str(&format!(" {}", view.counts));
        }
        if !view.state_word.is_empty() {
            text.push_str(&format!(" · {}", view.state_word));
        }
        let button =
            |id: &'static str, icon: IconName, tip: &'static str, action: Box<dyn gpui::Action>| {
                let busy = busy.clone();
                IconButton::new(id, icon)
                    .icon_size(IconSize::Small)
                    .disabled(busy.is_some())
                    .tooltip(move |window, cx| match &busy {
                        Some(b) => Tooltip::text(format!("{b} is running"))(window, cx),
                        None => Tooltip::for_action(tip, action.as_ref())(window, cx),
                    })
                    .on_click({
                        let name = id;
                        move |_, window, cx| {
                            let action: Box<dyn gpui::Action> = match name {
                                "sync-bar-fetch" => Box::new(git_actions::Fetch),
                                "sync-bar-pull" => Box::new(git_actions::Pull),
                                _ => Box::new(git_actions::Push),
                            };
                            window.dispatch_action(action, cx)
                        }
                    })
            };
        h_flex()
            .id("sync-panel-bar")
            .flex_none()
            .h(px(26.))
            .gap(px(2.))
            .text_size(metrics::text_sm())
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_color(ui.text_muted.hsla())
                    .child(SharedString::from(match &running {
                        Some(r) => r.progress_label(),
                        None => text,
                    })),
            )
            .when_some(
                running.as_ref().and_then(|r| r.progress.as_ref()?.percent),
                |el, pc| {
                    el.child(
                        div()
                            .w(px(48.))
                            .h(px(4.))
                            .rounded(px(2.))
                            .bg(ui.border.hsla())
                            .child(
                                div()
                                    .h_full()
                                    .rounded(px(2.))
                                    .bg(ui.accent.hsla())
                                    .w(px(48. * f32::from(pc.min(100)) / 100.)),
                            ),
                    )
                },
            )
            .when(running.is_some(), |el| {
                el.child(
                    Button::new("sync-bar-cancel", "Cancel")
                        .compact()
                        .tooltip(Tooltip::for_action(
                            "Stop git and everything it started",
                            &git_actions::CancelSync,
                        ))
                        .on_click(|_, window, cx| {
                            window.dispatch_action(Box::new(git_actions::CancelSync), cx)
                        }),
                )
            })
            .when(running.is_none(), |el| {
                el.child(button(
                    "sync-bar-fetch",
                    IconName::Refresh,
                    "Fetch from all remotes",
                    Box::new(git_actions::Fetch),
                ))
                .child(button(
                    "sync-bar-pull",
                    IconName::ArrowDown,
                    "Pull (fast-forward only)",
                    Box::new(git_actions::Pull),
                ))
                .child(button(
                    "sync-bar-push",
                    IconName::ArrowUp,
                    "Push",
                    Box::new(git_actions::Push),
                ))
            })
    }
}
