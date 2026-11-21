//! Rendering of the [`FileView`]: banners (deleted, conflict, read-only),
//! breadcrumbs, the hover popover, binary and image previews, and the shared
//! panel and symbol-badge helpers.

use std::path::Path;
use std::rc::Rc;

use gpui::{
    anchored, deferred, div, prelude::*, AnyElement, App, Corner, Focusable, FontWeight,
    SharedString, Window,
};
use lector_fs::FileKind;
use lector_syntax::SymbolKind;

use super::{format_size, Content, FileView, FileViewEvent};
use crate::components::{h_flex, metrics, v_flex, Button, ButtonStyle};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::markdown::{self, RenderOptions};
use crate::scale::px;
use crate::theme::{ActiveTheme, ColorExt};

impl FileView {
    /// Whether the "deleted on disk" banner is up: unsaved changes of a file
    /// that is gone (a clean tab of a deleted file only gets a struck-through
    /// title).
    pub fn shows_deleted_banner(&self, cx: &App) -> bool {
        let model = self.model.read(cx);
        model.is_deleted() && model.is_dirty(cx) && model.conflict().is_none()
    }

    /// The banner of a dirty buffer whose file was deleted on disk: Save
    /// writes it again, Close closes the tab (asking about the changes).
    fn render_deleted_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.shows_deleted_banner(cx) {
            return None;
        }
        let ui = cx.theme().ui.clone();
        let warn = cx.theme().git.modified.hsla();
        let weak = cx.entity().downgrade();
        Some(
            h_flex()
                .id("file-deleted-banner")
                .h(px(32.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(warn.opacity(0.12))
                .border_b_1()
                .border_color(warn.opacity(0.45))
                .text_size(metrics::text_sm())
                .text_color(ui.text.hsla())
                .child(
                    Icon::new(IconName::AlertTriangle)
                        .size(IconSize::Small)
                        .color(warn),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child("This file was deleted on disk."),
                )
                .child(
                    Button::new("deleted-save", "Save")
                        .compact()
                        .style(ButtonStyle::Accent)
                        .on_click({
                            let weak = weak.clone();
                            move |_, window, cx| {
                                let _ = weak.update(cx, |this, cx| {
                                    let task = this.save(window, cx);
                                    this.save_task = Some(cx.spawn(async move |_, _| {
                                        if let Err(err) = task.await {
                                            tracing::warn!("save failed: {err:#}");
                                        }
                                    }));
                                });
                            }
                        }),
                )
                .child(
                    Button::new("deleted-close", "Close")
                        .compact()
                        .style(ButtonStyle::Outlined)
                        .on_click({
                            let weak = weak.clone();
                            move |_, _, cx| {
                                let _ =
                                    weak.update(cx, |_, cx| cx.emit(FileViewEvent::CloseRequested));
                            }
                        }),
                )
                .into_any_element(),
        )
    }

    fn render_conflict_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        self.model.read(cx).conflict()?;
        let ui = cx.theme().ui.clone();
        let warn = cx.theme().git.modified.hsla();
        let weak = cx.entity().downgrade();
        let button = |id: &'static str, label: &'static str, style: ButtonStyle| {
            Button::new(id, label).compact().style(style)
        };
        Some(
            h_flex()
                .id("disk-conflict-banner")
                .h(px(32.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(warn.opacity(0.12))
                .border_b_1()
                .border_color(warn.opacity(0.45))
                .text_size(metrics::text_sm())
                .text_color(ui.text.hsla())
                .child(
                    Icon::new(IconName::AlertTriangle)
                        .size(IconSize::Small)
                        .color(warn),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .whitespace_nowrap()
                        .child("This file changed on disk. You have unsaved changes."),
                )
                .child(
                    button(
                        "conflict-reload",
                        "Reload (discard mine)",
                        ButtonStyle::Outlined,
                    )
                    .on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak
                                .update(cx, |this, cx| this.model.update(cx, |m, cx| m.revert(cx)));
                        }
                    }),
                )
                .child(
                    button("conflict-keep", "Keep Mine", ButtonStyle::Outlined).on_click({
                        let weak = weak.clone();
                        move |_, _, cx| {
                            let _ = weak.update(cx, |this, cx| {
                                this.model.update(cx, |m, cx| m.keep_mine(cx))
                            });
                        }
                    }),
                )
                .child(
                    button("conflict-compare", "Compare", ButtonStyle::Accent).on_click({
                        let weak = weak.clone();
                        move |_, window, cx| {
                            let _ = weak.update(cx, |this, cx| this.compare_with_disk(window, cx));
                        }
                    }),
                )
                .into_any_element(),
        )
    }

    fn render_breadcrumbs(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let symbols = self.symbols_at_cursor(cx);
        let sep = || {
            Icon::new(IconName::ChevronRight)
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla())
        };
        let segments: Vec<SharedString> = self
            .rel_path
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| SharedString::from(s.to_string()))
            .collect();
        let n = segments.len();
        let mut row = h_flex()
            .id("breadcrumbs")
            .h(px(26.))
            .flex_none()
            .px(px(12.))
            .gap(px(4.))
            .overflow_hidden()
            .whitespace_nowrap()
            .text_size(metrics::text_sm())
            .text_color(ui.text_muted.hsla())
            .bg(ui.background.hsla())
            .border_b_1()
            .border_color(ui.border_variant.hsla());
        for (i, seg) in segments.into_iter().enumerate() {
            if i > 0 {
                row = row.child(sep());
            }
            let last = i + 1 == n;
            let icon = if last {
                FileIcon::file(seg.clone()).size(px(14.)).into_any_element()
            } else {
                FileIcon::folder(seg.clone(), false, false)
                    .size(px(14.))
                    .into_any_element()
            };
            row = row.child(
                h_flex()
                    .id(("crumb-path", i))
                    .flex_none()
                    .gap(px(4.))
                    .px(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                    .tab_index(0)
                    .focus(crate::a11y::ring_focus(cx))
                    .when(last && symbols.is_empty(), |el| {
                        el.text_color(ui.text.hsla())
                    })
                    .on_click(move |_, window, cx| {
                        window.dispatch_action(
                            Box::new(crate::actions::workspace::RevealActiveFile),
                            cx,
                        );
                    })
                    .child(icon)
                    .child(seg),
            );
        }
        let count = symbols.len();
        for (i, item) in symbols.into_iter().enumerate() {
            row = row.child(sep());
            let last = i + 1 == count;
            let weak = cx.entity().downgrade();
            let item_for_click = item.clone();
            row = row.child(
                h_flex()
                    .id(("crumb-sym", i))
                    .flex_none()
                    .gap(px(5.))
                    .px(px(3.))
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|s| s.bg(ui.hover.hsla()).text_color(ui.text.hsla()))
                    .tab_index(0)
                    .focus(crate::a11y::ring_focus(cx))
                    .when(last, |el| el.text_color(ui.text.hsla()))
                    .on_click(move |_, window, cx| {
                        let item = item_for_click.clone();
                        let _ = weak.update(cx, |this, cx| {
                            this.jump_to_symbol(&item, cx);
                            let focus = this.code_view.focus_handle(cx);
                            window.focus(&focus);
                        });
                    })
                    .child(symbol_badge(item.kind, cx))
                    .child(SharedString::from(item.name.clone())),
            );
        }
        row
    }

    fn render_banner(&self, cx: &App) -> Option<AnyElement> {
        let model = self.model.read(cx);
        let doc = model.live_document()?;
        let ui = &cx.theme().ui;
        let message: SharedString = if doc.is_large() {
            format!(
                "Large file ({}) — syntax highlighting off, read-only",
                format_size(doc.size())
            )
            .into()
        } else if doc.lossy() || (!model.is_editable() && matches!(doc.kind(), FileKind::Text)) {
            format!(
                "Opened as {} — some bytes could not be decoded and are shown as \u{FFFD}; read-only to keep the file intact",
                doc.encoding()
            )
            .into()
        } else if doc.language().is_some()
            && doc.syntax().is_none()
            && doc.size() > lector_syntax::MAX_PARSE_BYTES as u64
        {
            "File too large to parse — syntax highlighting off".into()
        } else {
            return None;
        };
        Some(
            h_flex()
                .h(px(26.))
                .flex_none()
                .px(px(12.))
                .gap(px(8.))
                .bg(ui.surface.hsla())
                .border_b_1()
                .border_color(ui.border_variant.hsla())
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .child(
                    Icon::new(IconName::Info)
                        .size(IconSize::XSmall)
                        .color(ui.text_muted.hsla()),
                )
                .child(message)
                .into_any_element(),
        )
    }

    fn render_hover(&self, window: &mut Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let hover = self.hover.as_ref()?;
        let ui = cx.theme().ui.clone();
        let opts = RenderOptions {
            base_dir: self.path.parent().map(Path::to_path_buf),
            on_link: Some(Rc::new(
                |target: &str, _window: &mut Window, cx: &mut App| {
                    if target.contains("://") {
                        cx.open_url(target);
                    }
                },
            )),
            compact: true,
            id: "hover-md".into(),
        };
        let elements = markdown::render_blocks(&hover.blocks, &opts, window, cx);
        let weak = cx.entity().downgrade();
        Some(
            deferred(
                anchored()
                    .position(hover.position)
                    .anchor(Corner::BottomLeft)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("hover-popover")
                            .occlude()
                            .mb(px(4.))
                            .max_w(px(620.))
                            .max_h(px(380.))
                            .overflow_y_scroll()
                            .px(px(12.))
                            .pt(px(10.))
                            .pb(px(4.))
                            .bg(ui.elevated.hsla())
                            .border_1()
                            .border_color(ui.border.hsla())
                            .rounded(metrics::radius_lg())
                            .shadow_lg()
                            .text_size(metrics::text())
                            .text_color(ui.text.hsla())
                            .on_hover(move |hovered, _, cx| {
                                let hovered = *hovered;
                                let _ = weak.update(cx, |this, cx| {
                                    if let Some(h) = &mut this.hover {
                                        h.hovered = hovered;
                                        if !hovered {
                                            this.hover = None;
                                        }
                                        cx.notify();
                                    }
                                });
                            })
                            .children(elements),
                    ),
            )
            .with_priority(1)
            .into_any_element(),
        )
    }

    fn render_binary(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let size = self.live_document(cx).map_or(0, |d| d.size());
        let path = self.path.clone();
        centered_panel(
            IconName::Binary,
            "Binary file".into(),
            format!("{} · not shown", format_size(size)).into(),
            cx,
        )
        .child(
            div()
                .id("reveal-binary")
                .mt(px(10.))
                .px(px(10.))
                .py(px(4.))
                .rounded(px(5.))
                .border_1()
                .border_color(ui.border.hsla())
                .text_size(metrics::text_sm())
                .cursor_pointer()
                .hover(|s| s.bg(ui.hover.hsla()))
                .tab_index(0)
                .focus(crate::a11y::ring_focus(cx))
                .on_click(move |_, _, _| crate::os::reveal_in_file_manager(&path))
                .child(crate::os::REVEAL_LABEL),
        )
        .into_any_element()
    }
}

/// A centered icon + title + detail panel (empty / binary / error states).
pub fn centered_panel(
    icon: IconName,
    title: SharedString,
    detail: SharedString,
    cx: &App,
) -> gpui::Div {
    let ui = &cx.theme().ui;
    v_flex()
        .size_full()
        .items_center()
        .justify_center()
        .gap(px(6.))
        .child(
            div()
                .mb(px(6.))
                .p(px(14.))
                .rounded(px(12.))
                .bg(ui.surface.hsla())
                .child(
                    Icon::new(icon)
                        .size(IconSize::Custom(px(30.)))
                        .color(ui.text_faint.hsla()),
                ),
        )
        .child(
            div()
                .text_size(px(15.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(ui.text.hsla())
                .child(title),
        )
        .child(
            div()
                .text_size(metrics::text_sm())
                .text_color(ui.text_muted.hsla())
                .child(detail),
        )
}

/// Capture name used to color a symbol kind.
fn kind_capture(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "namespace",
        SymbolKind::Class | SymbolKind::Struct | SymbolKind::Type => "type",
        SymbolKind::Enum => "type",
        SymbolKind::Interface | SymbolKind::Trait => "type",
        SymbolKind::Function | SymbolKind::Method => "function",
        SymbolKind::Field => "property",
        SymbolKind::Variable => "variable",
        SymbolKind::Constant => "constant",
        SymbolKind::Macro => "function.macro",
        SymbolKind::Heading => "markup.heading",
        SymbolKind::Other => "comment",
    }
}

fn kind_letter(kind: SymbolKind) -> &'static str {
    match kind {
        SymbolKind::Module => "M",
        SymbolKind::Class => "C",
        SymbolKind::Struct => "S",
        SymbolKind::Enum => "E",
        SymbolKind::Interface => "I",
        SymbolKind::Trait => "T",
        SymbolKind::Function => "ƒ",
        SymbolKind::Method => "m",
        SymbolKind::Field => "F",
        SymbolKind::Variable => "v",
        SymbolKind::Constant => "K",
        SymbolKind::Type => "T",
        SymbolKind::Macro => "!",
        SymbolKind::Heading => "#",
        SymbolKind::Other => "{}",
    }
}

/// A small colored badge for a symbol kind (theme syntax colors).
pub fn symbol_badge(kind: SymbolKind, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    let color = theme
        .syntax
        .style_for(kind_capture(kind))
        .map(|s| s.color.hsla())
        .unwrap_or(theme.ui.text_muted.hsla());
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
        .font_weight(FontWeight::BOLD)
        .child(kind_letter(kind))
}

impl Render for FileView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let svg_toggle = self.show_svg_source
            && matches!(&self.content, Content::Image(v) if v.read(cx).is_svg());
        if std::mem::take(&mut self.focus_pending) {
            let focus = self.focus_handle(cx);
            window.focus(&focus);
        }
        let content: AnyElement = match &self.content {
            Content::Loading => div()
                .size_full()
                .bg(ui.background.hsla())
                .into_any_element(),
            Content::Text => self.code_view.clone().into_any_element(),
            Content::Image(view) => {
                if self.show_svg_source {
                    self.code_view.clone().into_any_element()
                } else {
                    view.clone().into_any_element()
                }
            }
            Content::Binary => self.render_binary(cx),
            Content::Error(message) => centered_panel(
                IconName::AlertTriangle,
                "Cannot open file".into(),
                message.clone(),
                cx,
            )
            .into_any_element(),
        };
        let show_crumbs = self.is_text.get() && self.setup_done;
        let conflict_banner = self
            .render_conflict_banner(cx)
            .or_else(|| self.render_deleted_banner(cx));
        let weak = cx.entity().downgrade();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::hover_action))
            .on_action(cx.listener(Self::clear_selection))
            .on_action(cx.listener(Self::save_action))
            .on_action(cx.listener(Self::save_as_action))
            .on_action(cx.listener(Self::revert_action))
            .when(show_crumbs, |el| el.child(self.render_breadcrumbs(cx)))
            .when(svg_toggle, |el| {
                let showing_source = self.show_svg_source;
                el.child(
                    h_flex()
                        .h(px(30.))
                        .flex_none()
                        .px(px(10.))
                        .gap(px(4.))
                        .border_b_1()
                        .border_color(ui.border_variant.hsla())
                        .child(
                            crate::components::Button::new("svg-preview", "Preview")
                                .compact()
                                .icon(IconName::Image)
                                .selected(!showing_source)
                                .on_click({
                                    let weak = weak.clone();
                                    move |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            if this.show_svg_source {
                                                this.toggle_svg_source(window, cx)
                                            }
                                        });
                                    }
                                }),
                        )
                        .child(
                            crate::components::Button::new("svg-source", "Source")
                                .compact()
                                .icon(IconName::Code)
                                .selected(showing_source)
                                .on_click({
                                    let weak = weak.clone();
                                    move |_, window, cx| {
                                        let _ = weak.update(cx, |this, cx| {
                                            if !this.show_svg_source {
                                                this.toggle_svg_source(window, cx)
                                            }
                                        });
                                    }
                                }),
                        ),
                )
            })
            .children(conflict_banner)
            .when_some(
                self.is_text.get().then(|| self.render_banner(cx)).flatten(),
                |el, banner| el.child(banner),
            )
            .children(self.render_inline_edit_bar(cx))
            .child(div().flex_1().min_h_0().relative().child(content))
            .children(self.render_hover(window, cx))
    }
}
