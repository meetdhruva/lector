//! Tooltips: a small floating label with an optional key binding hint.

use gpui::{div, prelude::*, px, Action, AnyView, App, SharedString, Window};

use super::{binding_text_for_action, h_flex, metrics, v_flex};
use crate::theme::{ActiveTheme, ColorExt};

/// Tooltip view. Build with [`Tooltip::text`] or [`Tooltip::for_action`].
pub struct Tooltip {
    title: SharedString,
    meta: Option<SharedString>,
    key_binding: Option<SharedString>,
}

impl Tooltip {
    /// A tooltip builder showing `title`.
    pub fn text(title: impl Into<SharedString>) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let title = title.into();
        move |_, cx| {
            cx.new(|_| Tooltip {
                title: title.clone(),
                meta: None,
                key_binding: None,
            })
            .into()
        }
    }

    /// A tooltip builder showing `title` and the binding of `action`.
    pub fn for_action(
        title: impl Into<SharedString>,
        action: &dyn Action,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let title = title.into();
        let action = action.boxed_clone();
        move |_, cx| {
            let key_binding = binding_text_for_action(action.as_ref(), cx);
            cx.new(|_| Tooltip {
                title: title.clone(),
                meta: None,
                key_binding,
            })
            .into()
        }
    }

    /// A tooltip builder with a title and a secondary line.
    pub fn with_meta(
        title: impl Into<SharedString>,
        meta: impl Into<SharedString>,
    ) -> impl Fn(&mut Window, &mut App) -> AnyView {
        let title = title.into();
        let meta = meta.into();
        move |_, cx| {
            cx.new(|_| Tooltip {
                title: title.clone(),
                meta: Some(meta.clone()),
                key_binding: None,
            })
            .into()
        }
    }
}

impl Render for Tooltip {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = &cx.theme().ui;
        // Offset from the cursor so the tooltip doesn't sit under the pointer.
        div().pl(px(8.)).pt(px(10.)).child(
            v_flex()
                .gap(px(2.))
                .px(px(8.))
                .py(px(5.))
                .bg(ui.tooltip_bg.hsla())
                .border_1()
                .border_color(ui.tooltip_border.hsla())
                .rounded(metrics::RADIUS)
                .shadow_md()
                .text_size(metrics::TEXT_SM)
                .text_color(ui.tooltip_fg.hsla())
                .child(
                    h_flex()
                        .gap(px(10.))
                        .justify_between()
                        .child(self.title.clone())
                        .when_some(self.key_binding.clone(), |el, kb| {
                            el.child(
                                div()
                                    .text_size(metrics::TEXT_XS)
                                    .text_color(ui.text_muted.hsla())
                                    .child(kb),
                            )
                        }),
                )
                .when_some(self.meta.clone(), |el, meta| {
                    el.child(
                        div()
                            .text_size(metrics::TEXT_XS)
                            .text_color(ui.text_muted.hsla())
                            .child(meta),
                    )
                }),
        )
    }
}
