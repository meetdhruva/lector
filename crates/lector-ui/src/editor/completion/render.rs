//! Rendering of the completion popup: kind badges, list rows, the
//! documentation pane and the controller's `Render` implementation.

use gpui::{
    div, prelude::*, AnyElement, App, Context, Hsla, IntoElement, MouseButton, Render,
    ScrollWheelEvent, SharedString, StyledText, Window,
};
use lector_lsp::CompletionKind;

use super::{
    badge_for, docs_width, label_runs, Badge, CompletionController, RowView, VISIBLE_ROWS,
};
use crate::code_view::EditorSettings;
use crate::components::popover::surface;
use crate::components::{h_flex, metrics, v_flex};
use crate::markdown::{self, RenderOptions};
use crate::scale::px;
use crate::theme::{ActiveTheme, ColorExt};

fn letter_badge(letter: &'static str, color: Hsla) -> AnyElement {
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .size(px(15.))
        .rounded(px(3.))
        .bg(color.opacity(0.16))
        .text_color(color)
        .text_size(px(10.))
        .font_weight(gpui::FontWeight::BOLD)
        .child(letter)
        .into_any_element()
}

fn kind_badge(kind: CompletionKind, cx: &App) -> AnyElement {
    match badge_for(kind) {
        Badge::Symbol(symbol) => crate::editor::symbol_badge(symbol, cx).into_any_element(),
        Badge::Letter(letter) => {
            let color = match kind {
                CompletionKind::Keyword => cx
                    .theme()
                    .syntax
                    .style_for("keyword")
                    .map(|s| s.color.hsla())
                    .unwrap_or(cx.theme().ui.text_muted.hsla()),
                _ => cx.theme().ui.text_muted.hsla(),
            };
            letter_badge(letter, color)
        }
    }
}

impl CompletionController {
    fn render_row(&self, row: &RowView, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme().clone();
        let ui = &theme.ui;
        let editor = EditorSettings::get(cx);
        let label: SharedString = row.label.clone().into();
        let runs = label_runs(
            &row.label,
            &row.highlights,
            row.deprecated,
            ui.text_accent.hsla(),
            ui.text_muted.hsla(),
        );
        let styled = StyledText::new(label).with_highlights(runs);
        let index = row.row;
        h_flex()
            .id(("completion-row", index))
            .h(metrics::row_height())
            .px(px(6.))
            .gap(px(6.))
            .flex_none()
            .when(row.selected, |el| el.bg(ui.active.hsla()))
            .when(!row.selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
            .cursor_pointer()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    this.click_row(index, cx)
                }),
            )
            .child(kind_badge(row.kind, cx))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .gap(px(4.))
                    .font_family(editor.font_family.clone())
                    .text_size(metrics::text())
                    .text_color(if row.deprecated {
                        ui.text_muted.hsla()
                    } else {
                        ui.text.hsla()
                    })
                    .child(styled)
                    .children(row.label_detail.clone().map(|d| {
                        div()
                            .text_color(ui.text_faint.hsla())
                            .text_size(metrics::text_sm())
                            .child(d)
                    })),
            )
            .children(row.description.clone().map(|d| {
                div()
                    .flex_none()
                    .max_w(px(180.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_faint.hsla())
                    .child(d)
            }))
            .into_any_element()
    }

    fn render_docs(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let open = self.open.as_ref()?;
        if open.docs.is_empty() {
            return None;
        }
        let ui = cx.theme().ui.clone();
        let editor = EditorSettings::get(cx);
        let item = open.docs.key.map(|(i, _)| i).unwrap_or(0);
        let detail = open.docs.detail.clone();
        let plain = open.docs.plain.clone();
        let blocks = open.docs.blocks.clone();
        let rendered = blocks.map(|blocks| {
            let opts = RenderOptions {
                base_dir: None,
                on_link: None,
                compact: true,
                id: "completion-docs".into(),
            };
            markdown::render_blocks(&blocks, &opts, window, cx)
        });
        Some(
            v_flex()
                .id(("completion-docs", item))
                .w(docs_width())
                .h_full()
                .flex_none()
                .overflow_y_scroll()
                .border_l_1()
                .border_color(ui.border_variant.hsla())
                .px(px(10.))
                .py(px(8.))
                .gap(px(6.))
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .children(detail.map(|d| {
                    div()
                        .font_family(editor.font_family.clone())
                        .text_color(ui.text.hsla())
                        .child(d)
                }))
                .children(plain)
                .children(rendered.into_iter().flatten())
                .into_any_element(),
        )
    }
}

impl Render for CompletionController {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.popup_visible() {
            return div().into_any_element();
        }
        if let Some(t) = self.open.as_mut().and_then(|o| o.reply_at.take()) {
            tracing::debug!(
                target: "lector_ui::perf",
                elapsed_us = t.elapsed().as_micros() as u64,
                "completion reply -> first render"
            );
        }
        let ui = cx.theme().ui.clone();
        let total = self.match_count();
        let rows = self.visible_rows();
        let list: Vec<AnyElement> = rows.iter().map(|r| self.render_row(r, cx)).collect();
        // A thin scroll thumb when the list is longer than the window.
        let thumb = (total > VISIBLE_ROWS).then(|| {
            let height = metrics::row_height() * VISIBLE_ROWS as f32;
            let thumb_h = (height * (VISIBLE_ROWS as f32 / total as f32)).max(px(14.));
            let travel = height - thumb_h;
            let max_top = (total - VISIBLE_ROWS) as f32;
            let y = travel * (self.list_top as f32 / max_top);
            div()
                .absolute()
                .right(px(1.))
                .top(y)
                .w(px(3.))
                .h(thumb_h)
                .rounded(px(2.))
                .bg(ui.scrollbar_thumb.hsla())
        });
        let docs = self.render_docs(window, cx);
        surface(cx)
            .size_full()
            .flex()
            .flex_row()
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, _, cx| {
                this.scroll_by(event, cx);
                cx.stop_propagation();
            }))
            .child(
                v_flex()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .overflow_hidden()
                    .children(list)
                    .children(thumb),
            )
            .children(docs)
            .into_any_element()
    }
}
