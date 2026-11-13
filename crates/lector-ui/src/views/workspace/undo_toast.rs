//! A toast with one action button ("Discarded 3 hunks in parser.rs · Undo"), shown
//! for several seconds instead of a confirmation dialog (v0.4 "undo instead of confirm").

use std::rc::Rc;
use std::time::Duration;

use crate::scale::px;
use gpui::{div, prelude::*, App, Context, SharedString, Window};

use super::Workspace;
use crate::components::{h_flex, metrics, Button, ButtonStyle};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Callback of the toast's button.
pub type ToastAction = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// What a toast reports; picks its icon (a check only for things that worked).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToastLevel {
    /// It worked.
    #[default]
    Success,
    /// A note: nothing happened or nothing was needed ("No agent is set up yet").
    Info,
    /// It partly worked or needs attention ("Popped ... with conflicts").
    Warning,
}

impl ToastLevel {
    /// The icon of the toast.
    pub(super) fn icon(self) -> IconName {
        match self {
            ToastLevel::Success => IconName::CircleCheck,
            ToastLevel::Info => IconName::Info,
            ToastLevel::Warning => IconName::AlertTriangle,
        }
    }

    /// The color of the icon.
    pub(super) fn color(self, ui: &lector_theme::UiColors) -> gpui::Hsla {
        match self {
            ToastLevel::Success => ui.accent.hsla(),
            ToastLevel::Info => ui.info.hsla(),
            ToastLevel::Warning => ui.warning.hsla(),
        }
    }
}

/// How long the Undo toast stays up.
pub const UNDO_TOAST_DURATION: Duration = Duration::from_secs(8);

/// State of the visible action toast.
pub(super) struct ActionToast {
    message: SharedString,
    label: SharedString,
    level: ToastLevel,
    id: u64,
    action: ToastAction,
}

impl Workspace {
    /// Shows `message` with a button `label` (clicking runs `action` and dismisses the
    /// toast) for `duration`. Replaces a toast that is still visible.
    pub fn show_action_toast(
        &mut self,
        message: impl Into<SharedString>,
        label: impl Into<SharedString>,
        duration: Duration,
        action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
        cx: &mut Context<Self>,
    ) {
        self.show_action_toast_level(message, ToastLevel::Success, label, duration, action, cx);
    }

    /// [`Self::show_action_toast`] with the icon of `level`.
    pub fn show_action_toast_level(
        &mut self,
        message: impl Into<SharedString>,
        level: ToastLevel,
        label: impl Into<SharedString>,
        duration: Duration,
        action: impl Fn(&mut Workspace, &mut Window, &mut Context<Workspace>) + 'static,
        cx: &mut Context<Self>,
    ) {
        let id = self.action_toast.as_ref().map(|t| t.id + 1).unwrap_or(0);
        self.action_toast = Some(ActionToast {
            message: message.into(),
            label: label.into(),
            level,
            id,
            action: Rc::new(action),
        });
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;
            let _ = this.update(cx, |this, cx| {
                if this.action_toast.as_ref().is_some_and(|t| t.id == id) {
                    this.action_toast = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Hides the action toast (e.g. after its operation was undone elsewhere).
    pub fn dismiss_action_toast(&mut self, cx: &mut Context<Self>) {
        if self.action_toast.take().is_some() {
            cx.notify();
        }
    }

    /// Text of the visible action toast (tests, screenshots).
    pub fn action_toast_message(&self) -> Option<SharedString> {
        self.action_toast.as_ref().map(|t| t.message.clone())
    }

    /// Level of the visible plain toast, if one is showing (tests).
    pub fn toast_level_shown(&self) -> Option<ToastLevel> {
        self.toast.as_ref().map(|_| self.toast_level)
    }

    /// Level of the visible action toast, if one is showing (tests).
    pub fn action_toast_level(&self) -> Option<ToastLevel> {
        self.action_toast.as_ref().map(|t| t.level)
    }

    /// Text of the visible plain toast (tests, screenshots).
    pub fn toast_message(&self) -> Option<SharedString> {
        self.toast.as_ref().map(|(message, _)| message.clone())
    }

    pub(super) fn render_action_toast(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let toast = self.action_toast.as_ref()?;
        let (message, label, action, level) = (
            toast.message.clone(),
            toast.label.clone(),
            toast.action.clone(),
            toast.level,
        );
        let ui = cx.theme().ui.clone();
        let this = cx.entity().downgrade();
        Some(
            div()
                .id("action-toast")
                .absolute()
                .bottom(metrics::status_bar_height() + px(12.))
                .right(px(16.))
                .max_w(px(560.))
                .pl(px(12.))
                .pr(px(6.))
                .py(px(5.))
                .rounded(metrics::radius_lg())
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .shadow_lg()
                .text_size(metrics::text_sm())
                .text_color(ui.text.hsla())
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Icon::new(level.icon())
                                .size(IconSize::Small)
                                .color(level.color(&ui)),
                        )
                        .child(div().min_w_0().overflow_hidden().child(message))
                        .child(
                            Button::new("action-toast-button", label)
                                .style(ButtonStyle::Subtle)
                                .compact()
                                .on_click(move |_, window, cx: &mut App| {
                                    let action = action.clone();
                                    if let Some(ws) = this.upgrade() {
                                        ws.update(cx, |ws, cx| {
                                            ws.action_toast = None;
                                            action(ws, window, cx);
                                            cx.notify();
                                        });
                                    }
                                }),
                        ),
                ),
        )
    }
}
