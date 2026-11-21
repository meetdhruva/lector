//! Symbol pickers: file outline (`workspace::ToggleOutline`), project
//! symbols (`workspace::ToggleSymbolFinder`) and multiple go-to-definition
//! targets. Matching uses `lector_base::fuzzy`: inline for small lists, on the scheduler for large
//! ones (`components::ranker`), so a project with 300k symbols never ranks on the UI thread.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

use crate::scale::px;
use gpui::{prelude::*, AnyElement, App, DismissEvent, SharedString, Task, Window};
use lector_syntax::SymbolKind;

use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::components::ranker::{ListRanker, Ranking};
use crate::editor::symbol_badge;
use crate::icons::FileIcon;

/// One pickable symbol / location.
#[derive(Clone, Debug)]
pub struct SymbolEntry {
    /// Symbol name (matched).
    pub name: SharedString,
    /// Kind (badge); `None` shows a file icon.
    pub kind: Option<SymbolKind>,
    /// Secondary text (path, container, preview).
    pub detail: SharedString,
    /// Indentation level (outline).
    pub depth: u32,
    /// File.
    pub path: PathBuf,
    /// 0-based row.
    pub row: u32,
    /// Byte columns to select within the row.
    pub cols: std::ops::Range<usize>,
}

/// Most rows shown for a query.
const MAX_MATCHES: usize = 500;

type ConfirmCallback = Rc<dyn Fn(SymbolEntry, &mut Window, &mut App)>;

/// Delegate for symbol pickers.
pub struct SymbolPickerDelegate {
    placeholder: SharedString,
    entries: Arc<Vec<SymbolEntry>>,
    ranker: ListRanker<SymbolEntry>,
    rank_task: Option<Task<()>>,
    rows: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_confirm: ConfirmCallback,
    empty: SharedString,
    loading: bool,
    keep_order: bool,
}

impl SymbolPickerDelegate {
    /// Picker over `entries`. `keep_order` shows the empty query in entry
    /// order (outline); otherwise the first entries are shown.
    pub fn new(
        placeholder: impl Into<SharedString>,
        entries: Vec<SymbolEntry>,
        keep_order: bool,
        on_confirm: impl Fn(SymbolEntry, &mut Window, &mut App) + 'static,
    ) -> Self {
        let entries = Arc::new(entries);
        SymbolPickerDelegate {
            placeholder: placeholder.into(),
            ranker: ListRanker::new(entries.clone(), |e| &*e.name, MAX_MATCHES),
            entries,
            rank_task: None,
            rows: Vec::new(),
            selected: 0,
            on_confirm: Rc::new(on_confirm),
            empty: "No symbols".into(),
            loading: false,
            keep_order,
        }
    }

    /// Message shown when there are no entries.
    pub fn with_empty_message(mut self, message: impl Into<SharedString>) -> Self {
        self.empty = message.into();
        self
    }

    /// Shows a "loading" message while entries are being computed.
    pub fn loading(mut self, loading: bool) -> Self {
        self.loading = loading;
        self
    }

    /// Confirms the selected row, which belongs to the latest query.
    fn confirm_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.rows.get(self.selected) else {
            return;
        };
        let entry = self.entries[*ix].clone();
        cx.emit(DismissEvent);
        let on_confirm = self.on_confirm.clone();
        window.defer(cx, move |window, cx| on_confirm(entry, window, cx));
    }

    /// Selects the entry closest above `row` (outline: current symbol).
    pub fn select_row(&mut self, row: u32) {
        if let Some(ix) = self
            .rows
            .iter()
            .rposition(|(e, _)| self.entries[*e].row <= row)
        {
            self.selected = ix;
        }
    }
}

impl PickerDelegate for SymbolPickerDelegate {
    fn placeholder(&self) -> SharedString {
        self.placeholder.clone()
    }

    fn match_count(&self) -> usize {
        self.rows.len()
    }

    fn selected_index(&self) -> usize {
        self.selected
    }

    fn set_selected_index(&mut self, ix: usize, _cx: &mut Context<Picker<Self>>) {
        self.selected = ix;
    }

    fn update_matches(&mut self, query: &str, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selected = 0;
        let query = query.trim();
        if query.is_empty() {
            self.ranker.cancel();
            self.rank_task = None;
            let limit = if self.keep_order {
                usize::MAX
            } else {
                MAX_MATCHES
            };
            self.rows = (0..self.entries.len().min(limit))
                .map(|ix| (ix, Vec::new()))
                .collect();
            return;
        }
        match self.ranker.set_query(query) {
            Ranking::Ready(rows) => {
                self.rank_task = None;
                self.rows = rows;
            }
            // A large list: rows of the previous query stay until the new ones arrive in one
            // piece, and an Enter in the meantime waits for them (`confirm`).
            Ranking::Pending(pending) => {
                self.rank_task = Some(cx.spawn_in(window, async move |this, cx| {
                    let (generation, rows) = pending.wait().await;
                    let Some(rows) = rows else { return };
                    let _ = this.update_in(cx, |picker, window, cx| {
                        let d = &mut picker.delegate;
                        if d.ranker.land(generation) {
                            d.rows = rows;
                            d.selected = 0;
                            cx.notify();
                            if d.ranker.take_deferred_confirm() {
                                d.confirm_selected(window, cx);
                            }
                        }
                    });
                }));
            }
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        // The rows on screen may belong to an older query while a large list is being ranked:
        // confirm only rows of the latest query, otherwise wait for them.
        if self.ranker.confirm_or_defer() {
            self.confirm_selected(window, cx);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {
        self.ranker.dismiss();
        self.rank_task = None;
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (entry_ix, positions) = &self.rows[ix];
        let entry = &self.entries[*entry_ix];
        let mut item = PickerItem::new(entry.name.clone(), selected)
            .positions(positions.clone())
            .detail(entry.detail.clone(), Vec::new())
            .indent(px(entry.depth.min(8) as f32 * 14.));
        item = match entry.kind {
            Some(kind) => item.icon(symbol_badge(kind, cx)),
            None => {
                let name = entry
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                item.icon(FileIcon::file(SharedString::from(name)).size(px(16.)))
            }
        };
        item.end_text(Some(format!("{}", entry.row + 1).into()))
            .into_any_element()
    }

    fn empty_message(&self) -> SharedString {
        if self.loading {
            "Indexing symbols…".into()
        } else {
            self.empty.clone()
        }
    }

    fn width(&self) -> gpui::Pixels {
        px(680.)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{Entity, Focusable as _, TestAppContext, VisualTestContext};
    use lector_theme::{Appearance, ThemeRegistry};

    use super::*;
    use crate::actions::picker as picker_actions;
    use crate::theme::{ThemeMode, ThemeState};

    type Picked = Rc<RefCell<Vec<String>>>;

    /// A picker over more symbols than fit the inline work budget (62,500 candidates at the very
    /// least), so every query is answered later.
    fn big_picker(
        cx: &mut TestAppContext,
    ) -> (
        Entity<Picker<SymbolPickerDelegate>>,
        Picked,
        &mut VisualTestContext,
    ) {
        cx.update(|cx| {
            ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        });
        cx.executor().allow_parking();
        let entries = (0..70_000)
            .map(|i| SymbolEntry {
                name: format!("sym_{i}_x").into(),
                kind: None,
                detail: "".into(),
                depth: 0,
                path: PathBuf::from("a.rs"),
                row: i,
                cols: 0..0,
            })
            .collect();
        let picked = Picked::default();
        let sink = picked.clone();
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let delegate = SymbolPickerDelegate::new("Symbol", entries, false, move |e, _, _| {
                sink.borrow_mut().push(e.name.to_string());
            });
            Picker::new(delegate, window, cx)
        });
        cx.update(|window, cx| window.focus(&picker.focus_handle(cx)));
        (picker, picked, cx)
    }

    fn query(picker: &Entity<Picker<SymbolPickerDelegate>>, q: &str, cx: &mut VisualTestContext) {
        let q = q.to_string();
        picker.update_in(cx, |p, window, cx| p.set_query(q, window, cx));
    }

    /// Lets the scheduler and the foreground tasks run until `done` holds.
    fn settle(cx: &mut VisualTestContext, done: impl Fn(&mut VisualTestContext) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "ranking never landed");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn top(picker: &Entity<Picker<SymbolPickerDelegate>>, cx: &mut VisualTestContext) -> String {
        picker.read_with(cx, |p, _| {
            p.delegate
                .rows
                .first()
                .map(|(ix, _)| p.delegate.entries[*ix].name.to_string())
                .unwrap_or_default()
        })
    }

    #[gpui::test]
    fn enter_never_confirms_a_row_of_an_older_query(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "sym_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "sym_12_x");

        // Query B is typed and Enter pressed before B's ranking has landed: the visible rows are
        // still A's, and must not be confirmed.
        // Nothing yields to the executor between the keystrokes, so B's ranking cannot land yet.
        query(&picker, "sym_5999", cx);
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));

        // The Enter is honoured against B's rows once they arrive.
        settle(cx, |_| !picked.borrow().is_empty());
        assert_eq!(*picked.borrow(), ["sym_5999_x"]);
    }

    #[gpui::test]
    fn dismissing_cancels_a_deferred_enter(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "sym_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "sym_12_x");
        query(&picker, "sym_5999", cx);
        picker.update_in(cx, |p, window, cx| {
            p.delegate.confirm(window, cx);
            p.delegate.dismissed(window, cx);
        });
        std::thread::sleep(std::time::Duration::from_millis(300));
        cx.run_until_parked();
        assert!(
            picked.borrow().is_empty(),
            "confirmed {:?}",
            picked.borrow()
        );
    }

    #[gpui::test]
    fn typing_again_drops_a_deferred_enter(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "sym_12_", cx);
        settle(cx, |cx| top(&picker, cx) == "sym_12_x");
        query(&picker, "sym_5999", cx);
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));
        query(&picker, "sym_4000", cx);
        settle(cx, |cx| top(&picker, cx) == "sym_4000_x");
        cx.run_until_parked();
        assert!(
            picked.borrow().is_empty(),
            "confirmed {:?}",
            picked.borrow()
        );
    }

    #[gpui::test]
    fn enter_on_settled_rows_confirms_at_once(cx: &mut TestAppContext) {
        let (picker, picked, cx) = big_picker(cx);
        query(&picker, "sym_4000", cx);
        settle(cx, |cx| top(&picker, cx) == "sym_4000_x");
        cx.dispatch_action(picker_actions::Confirm);
        cx.run_until_parked();
        assert_eq!(*picked.borrow(), ["sym_4000_x"]);
    }
}
