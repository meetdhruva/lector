//! A generic fuzzy picker modal: a query input above a virtualized list of
//! matches. Data and behavior come from a [`PickerDelegate`].

use std::ops::Range;

use gpui::{
    div, prelude::*, px, uniform_list, AnyElement, App, DismissEvent, Entity, EventEmitter,
    FocusHandle, Focusable, FontWeight, HighlightStyle, Hsla, MouseButton, Pixels, ScrollStrategy,
    SharedString, StyledText, Subscription, UniformListScrollHandle, Window,
};

use super::popover::surface;
use super::scrollbar::{vertical_scrollbar, ScrollbarState};
use super::text_input::{TextInput, TextInputEvent};
use super::{h_flex, metrics, v_flex};
use crate::actions::picker::{Cancel, Confirm, SelectNext, SelectPrev};
use crate::icons::{Icon, IconName, IconSize};
use crate::keymap::contexts;
use crate::theme::{ActiveTheme, ColorExt};

/// Supplies items, matching and confirmation behavior to a [`Picker`].
pub trait PickerDelegate: Sized + 'static {
    /// Placeholder of the query input.
    fn placeholder(&self) -> SharedString;
    /// Number of current matches.
    fn match_count(&self) -> usize;
    /// Selected match index.
    fn selected_index(&self) -> usize;
    /// Changes the selected match.
    fn set_selected_index(&mut self, ix: usize, cx: &mut Context<Picker<Self>>);
    /// Recomputes matches for `query`. May finish asynchronously, calling
    /// `cx.notify()` when done.
    fn update_matches(&mut self, query: &str, window: &mut Window, cx: &mut Context<Picker<Self>>);
    /// Confirms the selected match. Emit [`DismissEvent`] to close.
    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>);
    /// Called when the picker is dismissed without confirming.
    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}
    /// Renders match `ix`.
    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> AnyElement;
    /// Shown when there are no matches.
    fn empty_message(&self) -> SharedString {
        "No matches".into()
    }
    /// Modal width.
    fn width(&self) -> Pixels {
        px(620.)
    }
    /// Optional element shown at the bottom (hints, counts).
    fn render_footer(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<AnyElement> {
        None
    }
}

/// The picker view.
pub struct Picker<D: PickerDelegate> {
    /// The delegate.
    pub delegate: D,
    input: Entity<TextInput>,
    scroll_handle: UniformListScrollHandle,
    scrollbar: ScrollbarState,
    focus_handle: FocusHandle,
    _subscription: Subscription,
}

impl<D: PickerDelegate> EventEmitter<DismissEvent> for Picker<D> {}

impl<D: PickerDelegate> Focusable for Picker<D> {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.input.focus_handle(cx)
    }
}

impl<D: PickerDelegate> Picker<D> {
    /// Creates a picker and runs an initial empty query.
    pub fn new(delegate: D, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let placeholder = delegate.placeholder();
        let input = cx.new(|cx| TextInput::new(placeholder, cx));
        let subscription =
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                TextInputEvent::Changed(query) => {
                    let query = query.clone();
                    this.delegate.update_matches(&query, window, cx);
                    this.scroll_to_selected();
                    cx.notify();
                }
            });
        let mut picker = Picker {
            delegate,
            input,
            scroll_handle: UniformListScrollHandle::new(),
            scrollbar: ScrollbarState::default(),
            focus_handle: cx.focus_handle(),
            _subscription: subscription,
        };
        picker.delegate.update_matches("", window, cx);
        picker
    }

    /// Current query text.
    pub fn query(&self, cx: &App) -> SharedString {
        self.input.read(cx).text().clone()
    }

    /// Sets the query (and re-runs matching).
    pub fn set_query(
        &mut self,
        query: impl Into<SharedString>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let query = query.into();
        self.input
            .update(cx, |input, cx| input.set_text(query.clone(), cx));
        self.delegate.update_matches(&query, window, cx);
        cx.notify();
    }

    /// Re-runs matching with the current query (after the delegate's items changed).
    pub fn refresh(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.query(cx);
        self.delegate.update_matches(&query, window, cx);
        cx.notify();
    }

    /// Scrolls the selected match into view.
    pub fn scroll_to_selected(&self) {
        self.scroll_handle
            .scroll_to_item(self.delegate.selected_index(), ScrollStrategy::Top);
    }

    /// Moves the selection by `delta`, wrapping.
    pub fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let count = self.delegate.match_count();
        if count == 0 {
            return;
        }
        let ix =
            (self.delegate.selected_index() as isize + delta).rem_euclid(count as isize) as usize;
        self.delegate.set_selected_index(ix, cx);
        self.scroll_to_selected();
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.delegate.match_count() > 0 {
            self.delegate.confirm(window, cx);
        }
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.delegate.dismissed(window, cx);
        cx.emit(DismissEvent);
    }

    fn render_list(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let count = self.delegate.match_count();
        if count == 0 {
            let ui = &cx.theme().ui;
            return div()
                .px(px(14.))
                .py(px(12.))
                .text_size(metrics::TEXT)
                .text_color(ui.text_faint.hsla())
                .child(self.delegate.empty_message())
                .into_any_element();
        }
        let list = uniform_list(
            "picker-matches",
            count,
            cx.processor(|this, range: Range<usize>, window, cx| {
                let selected = this.delegate.selected_index();
                range
                    .map(|ix| {
                        let item = this.delegate.render_match(ix, ix == selected, window, cx);
                        div()
                            .id(ix)
                            .px(px(6.))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, window, cx| {
                                    this.delegate.set_selected_index(ix, cx);
                                    this.delegate.confirm(window, cx);
                                }),
                            )
                            .child(item)
                    })
                    .collect()
            }),
        )
        .track_scroll(self.scroll_handle.clone())
        .with_sizing_behavior(gpui::ListSizingBehavior::Infer)
        .py(px(4.))
        .max_h(px(420.));
        div()
            .relative()
            .child(list)
            .child(vertical_scrollbar(
                "picker-scrollbar",
                &self.scroll_handle.0.borrow().base_handle.clone(),
                &self.scrollbar,
                cx,
            ))
            .into_any_element()
    }
}

impl<D: PickerDelegate> Render for Picker<D> {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let footer = self.delegate.render_footer(window, cx);
        let list = self.render_list(cx);
        surface(cx)
            .key_context(contexts::PICKER)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .on_action(cx.listener(Self::cancel))
            .w(self.delegate.width())
            .child(
                v_flex()
                    .child(
                        h_flex()
                            .h(px(42.))
                            .px(px(12.))
                            .gap(px(8.))
                            .border_b_1()
                            .border_color(ui.border_variant.hsla())
                            .child(
                                Icon::new(IconName::Search)
                                    .size(IconSize::Small)
                                    .color(ui.text_faint.hsla()),
                            )
                            .child(
                                div()
                                    .flex_1()
                                    .text_size(px(14.))
                                    .line_height(px(20.))
                                    .text_color(ui.text.hsla())
                                    .child(self.input.clone()),
                            ),
                    )
                    .child(list)
                    .when_some(footer, |el, footer| {
                        el.child(
                            div()
                                .border_t_1()
                                .border_color(ui.border_variant.hsla())
                                .child(footer),
                        )
                    }),
            )
    }
}

/// Text with highlighted byte positions (fuzzy match indices).
pub fn highlighted_text(text: SharedString, positions: &[usize], color: Hsla) -> StyledText {
    let mut ranges: Vec<(Range<usize>, HighlightStyle)> = Vec::new();
    for &pos in positions {
        let Some(ch) = text.get(pos..).and_then(|s| s.chars().next()) else {
            continue;
        };
        let end = pos + ch.len_utf8();
        match ranges.last_mut() {
            Some((r, _)) if r.end == pos => r.end = end,
            _ => ranges.push((
                pos..end,
                HighlightStyle {
                    color: Some(color),
                    font_weight: Some(FontWeight::SEMIBOLD),
                    ..Default::default()
                },
            )),
        }
    }
    StyledText::new(text).with_highlights(ranges)
}

/// A standard picker row: optional icon, highlighted label, dimmed detail and
/// a right-aligned key binding.
#[derive(IntoElement)]
pub struct PickerItem {
    label: SharedString,
    positions: Vec<usize>,
    detail: Option<SharedString>,
    detail_positions: Vec<usize>,
    icon: Option<AnyElement>,
    end: Option<SharedString>,
    label_color: Option<Hsla>,
    indent: Pixels,
    selected: bool,
}

impl PickerItem {
    /// New row.
    pub fn new(label: impl Into<SharedString>, selected: bool) -> Self {
        PickerItem {
            label: label.into(),
            positions: Vec::new(),
            detail: None,
            detail_positions: Vec::new(),
            icon: None,
            end: None,
            label_color: None,
            indent: px(0.),
            selected,
        }
    }

    /// Highlighted byte positions in the label.
    pub fn positions(mut self, positions: Vec<usize>) -> Self {
        self.positions = positions;
        self
    }

    /// Secondary text after the label, with its own highlights.
    pub fn detail(mut self, detail: impl Into<SharedString>, positions: Vec<usize>) -> Self {
        self.detail = Some(detail.into());
        self.detail_positions = positions;
        self
    }

    /// Leading icon element.
    pub fn icon(mut self, icon: impl IntoElement) -> Self {
        self.icon = Some(icon.into_any_element());
        self
    }

    /// Left indentation (outline depth).
    pub fn indent(mut self, indent: Pixels) -> Self {
        self.indent = indent;
        self
    }

    /// Label color override (e.g. git status).
    pub fn label_color(mut self, color: Hsla) -> Self {
        self.label_color = Some(color);
        self
    }

    /// Right-aligned text (e.g. key binding).
    pub fn end_text(mut self, text: Option<SharedString>) -> Self {
        self.end = text;
        self
    }
}

impl RenderOnce for PickerItem {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let ui = &cx.theme().ui;
        let accent = ui.text_accent.hsla();
        h_flex()
            .h(px(30.))
            .px(px(8.))
            .pl(px(8.) + self.indent)
            .gap(px(8.))
            .rounded(metrics::RADIUS)
            .text_size(metrics::TEXT)
            .text_color(ui.text.hsla())
            .when(self.selected, |el| el.bg(ui.active.hsla()))
            .when(!self.selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
            .cursor_pointer()
            .when_some(self.icon, |el, icon| el.child(icon))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(px(8.))
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .child(
                        div()
                            .flex_none()
                            .when_some(self.label_color, |el, c| el.text_color(c))
                            .child(highlighted_text(self.label, &self.positions, accent)),
                    )
                    .when_some(self.detail, |el, detail| {
                        el.child(
                            div()
                                .min_w_0()
                                .overflow_hidden()
                                .text_ellipsis()
                                .text_size(metrics::TEXT_SM)
                                .text_color(ui.text_faint.hsla())
                                .child(highlighted_text(detail, &self.detail_positions, accent)),
                        )
                    }),
            )
            .when_some(self.end, |el, end| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .bg(ui.keybinding_bg.hsla())
                        .text_size(metrics::TEXT_XS)
                        .text_color(ui.text_muted.hsla())
                        .child(end),
                )
            })
    }
}
