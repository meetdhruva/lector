//! Floating surfaces anchored to a point (popovers, context menus).

use crate::scale::px;
use gpui::{
    anchored, deferred, div, prelude::*, App, Corner, DismissEvent, Div, Entity, Focusable,
    ManagedView, Pixels, Point, Subscription, Window,
};

use super::metrics;
use crate::theme::{ActiveTheme, ColorExt};

/// A styled floating surface (elevated background, border, shadow).
pub fn surface(cx: &App) -> Div {
    let ui = &cx.theme().ui;
    div()
        .bg(ui.elevated.hsla())
        .border_1()
        .border_color(ui.border.hsla())
        .rounded(metrics::radius_lg())
        .shadow_lg()
        .overflow_hidden()
}

/// Holds an open floating view (`V: ManagedView`) anchored at a window
/// position; closes itself when the view emits [`DismissEvent`].
pub struct Popover<V: ManagedView> {
    open: Option<(Entity<V>, Point<Pixels>, Corner, Subscription)>,
}

impl<V: ManagedView> Default for Popover<V> {
    fn default() -> Self {
        Popover { open: None }
    }
}

impl<V: ManagedView> Popover<V> {
    /// No popover open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens `view` at `position` (window coordinates), focusing it.
    pub fn show<T: 'static>(
        &mut self,
        view: Entity<V>,
        position: Point<Pixels>,
        anchor: Corner,
        window: &mut Window,
        cx: &mut Context<T>,
        on_dismiss: impl Fn(&mut T, &mut Window, &mut Context<T>) + 'static,
    ) {
        let previous_focus = window.focused(cx);
        let subscription = cx.subscribe_in(
            &view,
            window,
            move |this, _, _: &DismissEvent, window, cx| {
                on_dismiss(this, window, cx);
                if let Some(focus) = previous_focus.as_ref() {
                    window.focus(focus);
                }
                cx.notify();
            },
        );
        window.focus(&view.focus_handle(cx));
        self.open = Some((view, position, anchor, subscription));
    }

    /// Closes the popover without emitting events.
    pub fn close(&mut self) {
        self.open = None;
    }

    /// Whether a popover is open.
    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    /// The open view, if any.
    pub fn view(&self) -> Option<&Entity<V>> {
        self.open.as_ref().map(|(v, ..)| v)
    }

    /// Element to add as a child of the owner's root (renders on top via
    /// `deferred`).
    pub fn render(&self) -> Option<impl IntoElement> {
        let (view, position, anchor, _) = self.open.as_ref()?;
        Some(
            deferred(
                anchored()
                    .position(*position)
                    .anchor(*anchor)
                    .snap_to_window_with_margin(px(8.))
                    .child(div().occlude().child(view.clone())),
            )
            .with_priority(2),
        )
    }
}
