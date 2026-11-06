//! A toast with one action button ("Discarded 3 hunks in parser.rs · Undo"), shown
//! for several seconds instead of a confirmation dialog (v0.4 "undo instead of confirm").

use std::rc::Rc;
use std::time::Duration;

use gpui::{div, prelude::*, px, App, Context, SharedString, Window};

use super::Workspace;
use crate::components::{h_flex, metrics, Button, ButtonStyle};
use crate::icons::{Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// Callback of the toast's button.
pub type ToastAction = Rc<dyn Fn(&mut Workspace, &mut Window, &mut Context<Workspace>)>;

/// How long the Undo toast stays up.
pub const UNDO_TOAST_DURATION: Duration = Duration::from_secs(8);

/// State of the visible action toast.
pub(super) struct ActionToast {
    message: SharedString,
    label: SharedString,
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
        let id = self.action_toast.as_ref().map(|t| t.id + 1).unwrap_or(0);
        self.action_toast = Some(ActionToast {
            message: message.into(),
            label: label.into(),
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

    /// Text of the visible plain toast (tests, screenshots).
    pub fn toast_message(&self) -> Option<SharedString> {
        self.toast.as_ref().map(|(message, _)| message.clone())
    }

    pub(super) fn render_action_toast(&self, cx: &mut Context<Self>) -> Option<impl IntoElement> {
        let toast = self.action_toast.as_ref()?;
        let (message, label, action) = (
            toast.message.clone(),
            toast.label.clone(),
            toast.action.clone(),
        );
        let ui = cx.theme().ui.clone();
        let this = cx.entity().downgrade();
        Some(
            div()
                .id("action-toast")
                .absolute()
                .bottom(metrics::STATUS_BAR_HEIGHT + px(12.))
                .right(px(16.))
                .max_w(px(560.))
                .pl(px(12.))
                .pr(px(6.))
                .py(px(5.))
                .rounded(metrics::RADIUS_LG)
                .bg(ui.elevated.hsla())
                .border_1()
                .border_color(ui.border.hsla())
                .shadow_lg()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text.hsla())
                .child(
                    h_flex()
                        .gap(px(8.))
                        .child(
                            Icon::new(IconName::CircleCheck)
                                .size(IconSize::Small)
                                .color(ui.accent.hsla()),
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
