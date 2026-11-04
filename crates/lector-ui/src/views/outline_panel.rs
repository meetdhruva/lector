//! Outline sidebar panel: the active file's symbols as an indented tree with
//! kind badges, a filter field, click-to-jump and cursor following.

use std::ops::Range;
use std::sync::Arc;

use gpui::{
    div, prelude::*, px, App, Entity, FocusHandle, Focusable, SharedString, Subscription, Window,
};
use lector_syntax::OutlineItem;

use crate::actions::picker::{Confirm, SelectNext, SelectPrev};
use crate::components::list::{virtual_list, ListRow, VirtualListState};
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{metrics, v_flex};
use crate::editor::{symbol_badge, symbols_containing_row, FileView, Reveal};
use crate::icons::IconName;
use crate::theme::{ActiveTheme, ColorExt};
use crate::views::search_panel::input_box;

struct Row {
    item: OutlineItem,
    row: u32,
    cols: Range<usize>,
}

/// The outline panel view.
pub struct OutlinePanel {
    file_view: Option<Entity<FileView>>,
    rows: Arc<Vec<Row>>,
    filtered: Vec<usize>,
    active: Option<usize>,
    selected: Option<usize>,
    filter: Entity<TextInput>,
    list: VirtualListState,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl Focusable for OutlinePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl OutlinePanel {
    /// Empty panel.
    pub fn new(cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| TextInput::new("Filter symbols", cx));
        let sub = cx.subscribe(&filter, |this, _, event: &TextInputEvent, cx| {
            let TextInputEvent::Changed(_) = event;
            this.apply_filter(cx);
        });
        OutlinePanel {
            file_view: None,
            rows: Arc::new(Vec::new()),
            filtered: Vec::new(),
            active: None,
            selected: None,
            filter,
            list: VirtualListState::new(),
            focus_handle: cx.focus_handle(),
            _subscriptions: vec![sub],
        }
    }

    /// Shows the outline of `view` (the active file).
    pub fn set_file_view(&mut self, view: Option<Entity<FileView>>, cx: &mut Context<Self>) {
        self.file_view = view;
        self.rebuild(cx);
    }

    fn rebuild(&mut self, cx: &mut Context<Self>) {
        let rows = self
            .file_view
            .as_ref()
            .and_then(|v| v.read(cx).document(cx).cloned())
            .and_then(|doc| {
                let text = doc.text()?.clone();
                Some(
                    doc.outline()
                        .iter()
                        .map(|item| {
                            let p = text.offset_to_point(item.name_range.start);
                            let e = text.offset_to_point(item.name_range.end);
                            Row {
                                item: item.clone(),
                                row: p.row,
                                cols: p.column as usize..if e.row == p.row {
                                    e.column as usize
                                } else {
                                    p.column as usize
                                },
                            }
                        })
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_default();
        self.rows = Arc::new(rows);
        self.apply_filter(cx);
        self.follow_cursor(cx);
    }

    fn apply_filter(&mut self, cx: &mut Context<Self>) {
        let query = self.filter.read(cx).text().to_lowercase();
        self.filtered = if query.is_empty() {
            (0..self.rows.len()).collect()
        } else {
            self.rows
                .iter()
                .enumerate()
                .filter(|(_, r)| r.item.name.to_lowercase().contains(&query))
                .map(|(ix, _)| ix)
                .collect()
        };
        self.selected = None;
        cx.notify();
    }

    /// Highlights the innermost symbol containing the cursor.
    pub fn follow_cursor(&mut self, cx: &mut Context<Self>) {
        let Some(view) = &self.file_view else {
            return;
        };
        let v = view.read(cx);
        let Some(doc) = v.document(cx) else { return };
        let Some(text) = doc.text() else { return };
        let row = v.code_view().read(cx).cursor().row;
        let innermost = symbols_containing_row(doc, text, row).pop();
        let active = innermost.and_then(|item| {
            self.rows
                .iter()
                .position(|r| r.item.range == item.range && r.item.name == item.name)
        });
        if active != self.active {
            self.active = active;
            if let Some(pos) = active.and_then(|a| self.filtered.iter().position(|f| *f == a)) {
                self.list.reveal(pos);
            }
            cx.notify();
        }
    }

    fn jump(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(view), Some(row)) = (self.file_view.clone(), self.rows.get(ix)) else {
            return;
        };
        let reveal = Reveal {
            row: row.row,
            cols: row.cols.clone(),
            center: true,
        };
        view.update(cx, |v, cx| v.reveal(reveal, cx));
        let focus = view.read(cx).code_view().focus_handle(cx);
        window.focus(&focus);
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.move_selection(-1, cx);
    }

    fn move_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.filtered.len();
        if n == 0 {
            return;
        }
        let cur = self.selected.map(|s| s as isize).unwrap_or(-1);
        let next = (cur + delta).rem_euclid(n as isize) as usize;
        self.selected = Some(next);
        self.list.reveal(next);
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(&ix) = self.filtered.get(self.selected.unwrap_or(0)) {
            self.jump(ix, window, cx);
        }
    }
}

impl Render for OutlinePanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let count = self.filtered.len();
        let body = if self.rows.is_empty() {
            let message: SharedString = match &self.file_view {
                None => "Open a file to see its symbols.".into(),
                Some(v) if v.read(cx).is_loading() => "Loading…".into(),
                Some(_) => "No symbols in this file.".into(),
            };
            div()
                .px(px(14.))
                .py(px(10.))
                .text_size(metrics::TEXT_SM)
                .text_color(ui.text_faint.hsla())
                .child(message)
                .into_any_element()
        } else {
            let this = cx.entity().downgrade();
            virtual_list(
                "outline-list",
                count,
                &self.list,
                cx,
                move |range, _window, cx| {
                    let Some(this) = this.upgrade() else {
                        return Vec::new();
                    };
                    let panel = this.read(cx);
                    let rows = panel.rows.clone();
                    let filtered: Vec<usize> = panel.filtered[range.clone()].to_vec();
                    let active = panel.active;
                    let selected = panel.selected;
                    let filtering = !panel.filter.read(cx).text().is_empty();
                    filtered
                        .into_iter()
                        .zip(range)
                        .map(|(ix, pos)| {
                            let row = &rows[ix];
                            let weak = this.downgrade();
                            let depth = if filtering { 0 } else { row.item.depth.min(10) };
                            ListRow::new(
                                ("outline-row", ix),
                                SharedString::from(row.item.name.clone()),
                            )
                            .indent(px(depth as f32 * 14.))
                            .start(symbol_badge(row.item.kind, cx))
                            .detail(SharedString::from(format!("{}", row.row + 1)))
                            .selected(Some(ix) == active || Some(pos) == selected)
                            .into_any_element()
                            .map_click(weak, ix)
                        })
                        .collect()
                },
            )
            .into_any_element()
        };
        v_flex()
            .size_full()
            .key_context("Picker")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::select_next))
            .on_action(cx.listener(Self::select_prev))
            .on_action(cx.listener(Self::confirm))
            .child(div().px(px(8.)).pb(px(6.)).child(input_box(
                self.filter.clone(),
                IconName::Filter,
                cx,
            )))
            .child(div().flex_1().min_h_0().child(body))
    }
}

/// Wraps a list row so a click jumps to outline entry `ix`.
trait MapClick {
    fn map_click(self, panel: gpui::WeakEntity<OutlinePanel>, ix: usize) -> gpui::AnyElement;
}

impl MapClick for gpui::AnyElement {
    fn map_click(self, panel: gpui::WeakEntity<OutlinePanel>, ix: usize) -> gpui::AnyElement {
        div()
            .id(("outline-click", ix))
            .cursor_pointer()
            .on_click(move |_, window, cx| {
                let _ = panel.update(cx, |p, cx| p.jump(ix, window, cx));
            })
            .child(self)
            .into_any_element()
    }
}
