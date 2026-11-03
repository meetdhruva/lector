//! Modal overlay layer: at most one modal (picker, dialog) at a time,
//! centered near the top of the window, dismissed on Escape or outside click.

use std::any::TypeId;

use gpui::{
    div, prelude::*, px, AnyView, App, DismissEvent, Entity, FocusHandle, Focusable, ManagedView,
    Subscription, Window,
};

use crate::theme::{ActiveTheme, ColorExt};

struct ActiveModal {
    view: AnyView,
    type_id: TypeId,
    focus: FocusHandle,
    previous_focus: Option<FocusHandle>,
    _subscription: Subscription,
}

/// The modal layer view; render it as the last child of the window root.
#[derive(Default)]
pub struct ModalLayer {
    active: Option<ActiveModal>,
}

impl ModalLayer {
    /// No modal.
    pub fn new() -> Self {
        Self::default()
    }

    /// Opens a modal built by `build`, or closes it if a modal of the same
    /// type is already open.
    pub fn toggle_modal<V: ManagedView>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
    ) {
        if self
            .active
            .as_ref()
            .is_some_and(|m| m.type_id == TypeId::of::<V>())
        {
            self.hide_modal(window, cx);
            return;
        }
        let view = cx.new(|cx| build(window, cx));
        self.show_modal(view, window, cx);
    }

    /// Shows `view` as the modal, replacing any open modal.
    pub fn show_modal<V: ManagedView>(
        &mut self,
        view: Entity<V>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let previous_focus = match self.active.take() {
            Some(old) => old.previous_focus,
            None => window.focused(cx),
        };
        let subscription =
            cx.subscribe_in(&view, window, |this, _, _: &DismissEvent, window, cx| {
                this.hide_modal(window, cx);
            });
        let focus = view.focus_handle(cx);
        window.focus(&focus);
        self.active = Some(ActiveModal {
            view: view.into(),
            type_id: TypeId::of::<V>(),
            focus,
            previous_focus,
            _subscription: subscription,
        });
        cx.notify();
    }

    /// Closes the modal and restores the previous focus.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(modal) = self.active.take() {
            if let Some(prev) = modal.previous_focus {
                window.focus(&prev);
            }
            cx.notify();
        }
    }

    /// Whether any modal is open.
    pub fn has_active_modal(&self) -> bool {
        self.active.is_some()
    }

    /// The open modal if it has type `V`.
    pub fn active_modal<V: 'static>(&self) -> Option<Entity<V>> {
        self.active
            .as_ref()
            .and_then(|m| m.view.clone().downcast::<V>().ok())
    }

    /// Focus handle of the modal, if any.
    pub fn modal_focus(&self) -> Option<&FocusHandle> {
        self.active.as_ref().map(|m| &m.focus)
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active) = self.active.as_ref() else {
            return div().into_any_element();
        };
        let overlay = cx.theme().ui.overlay.hsla();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .bg(overlay)
            .flex()
            .flex_col()
            .items_center()
            .pt(px(72.))
            .child(
                div()
                    .occlude()
                    .on_mouse_down_out(
                        cx.listener(|this, _, window, cx| this.hide_modal(window, cx)),
                    )
                    .child(active.view.clone()),
            )
            .child(div().h(px(1.)))
            .into_any_element()
    }
}

/// Convenience for views holding an `Entity<ModalLayer>`.
pub fn toggle_modal<V: ManagedView>(
    layer: &Entity<ModalLayer>,
    window: &mut Window,
    cx: &mut App,
    build: impl FnOnce(&mut Window, &mut Context<V>) -> V,
) {
    layer.update(cx, |layer, cx| layer.toggle_modal(window, cx, build));
}
