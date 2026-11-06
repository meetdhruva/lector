//! Symbol pickers: file outline (`workspace::ToggleOutline`), project
//! symbols (`workspace::ToggleSymbolFinder`) and multiple go-to-definition
//! targets. Fuzzy matching runs on nucleo.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::scale::px;
use gpui::{prelude::*, AnyElement, App, DismissEvent, SharedString, Task, Window};
use lector_syntax::SymbolKind;

use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::editor::symbol_badge;
use crate::fuzzy::NucleoList;
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

type ConfirmCallback = Rc<dyn Fn(SymbolEntry, &mut Window, &mut App)>;

/// Delegate for symbol pickers.
pub struct SymbolPickerDelegate {
    placeholder: SharedString,
    entries: Arc<Vec<SymbolEntry>>,
    list: NucleoList<usize>,
    rows: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_confirm: ConfirmCallback,
    tick_task: Option<Task<()>>,
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
        let list = NucleoList::new(false);
        let injector = list.injector();
        let feed = entries.clone();
        if feed.len() > 2000 {
            std::thread::spawn(move || {
                for (ix, e) in feed.iter().enumerate() {
                    injector.push(ix, &e.name);
                }
            });
        } else {
            for (ix, e) in feed.iter().enumerate() {
                injector.push(ix, &e.name);
            }
        }
        SymbolPickerDelegate {
            placeholder: placeholder.into(),
            entries,
            list,
            rows: Vec::new(),
            selected: 0,
            on_confirm: Rc::new(on_confirm),
            tick_task: None,
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

    fn refresh(&mut self) {
        self.rows = self
            .list
            .top(500)
            .into_iter()
            .map(|m| (m.item, m.positions))
            .collect();
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

    fn update_matches(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        self.selected = 0;
        let query = query.trim();
        if query.is_empty() {
            self.tick_task = None;
            let limit = if self.keep_order { usize::MAX } else { 500 };
            self.rows = (0..self.entries.len().min(limit))
                .map(|ix| (ix, Vec::new()))
                .collect();
            return;
        }
        self.list.set_query(query);
        let running = self.list.tick(8);
        self.refresh();
        if running || (self.list.counts().1 as usize) < self.entries.len() {
            self.tick_task = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let Ok(done) = this.update(cx, |picker, cx| {
                    let d = &mut picker.delegate;
                    let running = d.list.tick(5);
                    d.refresh();
                    cx.notify();
                    !running && d.list.counts().1 as usize >= d.entries.len()
                }) else {
                    break;
                };
                if done {
                    break;
                }
            }));
        }
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.rows.get(self.selected) else {
            return;
        };
        let entry = self.entries[*ix].clone();
        cx.emit(DismissEvent);
        let on_confirm = self.on_confirm.clone();
        window.defer(cx, move |window, cx| on_confirm(entry, window, cx));
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
