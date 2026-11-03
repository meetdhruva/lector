//! Virtualized lists: fixed-height rows rendered on demand via
//! `uniform_list`, with an overlay scrollbar, plus a standard list row.

use std::ops::Range;

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, ElementId, ScrollStrategy, SharedString,
    UniformListScrollHandle, Window,
};

use super::scrollbar::{vertical_scrollbar, ScrollbarState};
use super::{h_flex, metrics};
use crate::theme::{ActiveTheme, ColorExt};

/// Scroll + scrollbar state for a [`virtual_list`]; keep one in the owning view.
#[derive(Clone, Default)]
pub struct VirtualListState {
    /// The uniform list scroll handle.
    pub scroll: UniformListScrollHandle,
    scrollbar: ScrollbarState,
}

impl VirtualListState {
    /// New state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Scrolls so `ix` is visible.
    pub fn reveal(&self, ix: usize) {
        self.scroll.scroll_to_item(ix, ScrollStrategy::Top);
    }

    /// Scrolls so `ix` is centered.
    pub fn center(&self, ix: usize) {
        self.scroll.scroll_to_item(ix, ScrollStrategy::Center);
    }
}

/// A virtualized list of `count` uniform rows rendered by `render_rows`
/// (called only for visible ranges). Use `cx.processor(..)` for view access.
pub fn virtual_list(
    id: impl Into<ElementId>,
    count: usize,
    state: &VirtualListState,
    cx: &App,
    render_rows: impl 'static + Fn(Range<usize>, &mut Window, &mut App) -> Vec<AnyElement>,
) -> impl IntoElement {
    let id: ElementId = id.into();
    let base = state.scroll.0.borrow().base_handle.clone();
    div()
        .relative()
        .size_full()
        .child(
            uniform_list(id.clone(), count, render_rows)
                .track_scroll(state.scroll.clone())
                .size_full(),
        )
        .child(vertical_scrollbar(
            ElementId::Name(format!("{id}-scrollbar").into()),
            &base,
            &state.scrollbar,
            cx,
        ))
}

/// A standard list row: leading element, label, optional detail and end element.
#[derive(IntoElement)]
pub struct ListRow {
    id: ElementId,
    label: SharedString,
    detail: Option<SharedString>,
    start: Option<AnyElement>,
    end: Option<AnyElement>,
    selected: bool,
    indent: gpui::Pixels,
}

impl ListRow {
    /// New row.
    pub fn new(id: impl Into<ElementId>, label: impl Into<SharedString>) -> Self {
        ListRow {
            id: id.into(),
            label: label.into(),
            detail: None,
            start: None,
            end: None,
            selected: false,
            indent: px(0.),
        }
    }

    /// Dimmed detail text.
    pub fn detail(mut self, detail: impl Into<SharedString>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    /// Leading element.
    pub fn start(mut self, el: impl IntoElement) -> Self {
        self.start = Some(el.into_any_element());
        self
    }

    /// Trailing element.
    pub fn end(mut self, el: impl IntoElement) -> Self {
        self.end = Some(el.into_any_element());
        self
    }

    /// Selected state.
    pub fn selected(mut self, selected: bool) -> Self {
        self.selected = selected;
        self
    }

    /// Left indent.
    pub fn indent(mut self, indent: gpui::Pixels) -> Self {
        self.indent = indent;
        self
    }
}

impl RenderOnce for ListRow {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let hover = ui.hover.hsla();
        h_flex()
            .id(self.id)
            .h(metrics::ROW_HEIGHT)
            .pl(px(8.) + self.indent)
            .pr(px(8.))
            .gap(px(6.))
            .text_size(metrics::TEXT)
            .text_color(ui.text.hsla())
            .when(self.selected, |el| el.bg(ui.active.hsla()))
            .when(!self.selected, |el| el.hover(move |s| s.bg(hover)))
            .when_some(self.start, |el, s| el.child(s))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(self.label),
            )
            .when_some(self.detail, |el, d| {
                el.child(
                    div()
                        .flex_none()
                        .text_size(metrics::TEXT_SM)
                        .text_color(ui.text_faint.hsla())
                        .child(d),
                )
            })
            .when_some(self.end, |el, e| el.child(e))
    }
}
