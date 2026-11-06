//! Project search sidebar panel: query with regex / case / whole-word
//! toggles and include / exclude globs; results stream in from
//! `lector_search::search_project` on a background thread (debounced,
//! cancelled on every new query), grouped by file with highlighted matches.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::scale::px;
use futures::StreamExt;
use gpui::{
    div, prelude::*, AnyElement, App, Entity, EventEmitter, FocusHandle, Focusable, FontWeight,
    HighlightStyle, SharedString, StyledText, Subscription, Task, Window,
};
use lector_search::{CancelToken, CaseMode, LineMatch, SearchEvent, SearchQuery, WalkOptions};
use lector_workspace::Project;

use crate::actions::picker::{Confirm, SelectNext, SelectPrev};
use crate::components::context_menu::{ContextMenu, ContextMenuState};
use crate::components::list::{virtual_list, VirtualListState};
use crate::components::text_input::{TextInput, TextInputEvent};
use crate::components::{h_flex, metrics, v_flex, IconButton, Tooltip};
use crate::icons::{FileIcon, Icon, IconName, IconSize};
use crate::theme::{ActiveTheme, ColorExt};

/// A bordered single-line input with a leading icon (sidebar fields).
pub fn input_box(input: Entity<TextInput>, icon: IconName, cx: &App) -> gpui::Div {
    let ui = &cx.theme().ui;
    h_flex()
        .h(px(28.))
        .px(px(8.))
        .gap(px(6.))
        .rounded(px(5.))
        .bg(ui.input_bg.hsla())
        .border_1()
        .border_color(ui.border.hsla())
        .child(
            Icon::new(icon)
                .size(IconSize::XSmall)
                .color(ui.text_faint.hsla()),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .text_size(metrics::text())
                .text_color(ui.text.hsla())
                .child(input),
        )
}

/// Events emitted by [`SearchPanel`].
#[derive(Clone, Debug, PartialEq)]
pub enum SearchPanelEvent {
    /// Open a match.
    Open {
        /// Absolute path.
        path: PathBuf,
        /// Row.
        row: u32,
        /// Byte columns of the match in the row.
        cols: Range<usize>,
        /// Open as a preview tab.
        preview: bool,
    },
}

struct FileResult {
    path: PathBuf,
    name: SharedString,
    dir: SharedString,
    matches: Vec<LineMatch>,
    collapsed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum RowRef {
    File(usize),
    Match(usize, usize),
}

#[derive(Clone, Debug, PartialEq)]
enum Status {
    Idle,
    Searching,
    Done {
        matches: usize,
        files: usize,
        truncated: bool,
        elapsed: Duration,
    },
    Error(SharedString),
}

/// Maximum matching lines per search.
const MAX_RESULTS: usize = 20_000;

/// The project search panel.
pub struct SearchPanel {
    project: Project,
    query: Entity<TextInput>,
    include: Entity<TextInput>,
    exclude: Entity<TextInput>,
    regex: bool,
    case_sensitive: bool,
    whole_word: bool,
    show_filters: bool,
    results: Vec<FileResult>,
    rows: Arc<Vec<RowRef>>,
    selected: Option<usize>,
    status: Status,
    match_count: usize,
    cancel: Option<CancelToken>,
    search_task: Option<Task<()>>,
    debounce_task: Option<Task<()>>,
    list: VirtualListState,
    focus_handle: FocusHandle,
    context_menu: ContextMenuState,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<SearchPanelEvent> for SearchPanel {}

impl Focusable for SearchPanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl SearchPanel {
    /// Panel searching `project`.
    pub fn new(project: Project, cx: &mut Context<Self>) -> Self {
        let query = cx.new(|cx| TextInput::new("Search", cx));
        let include = cx.new(|cx| TextInput::new("Files to include (e.g. src/**, *.rs)", cx));
        let exclude = cx.new(|cx| TextInput::new("Files to exclude", cx));
        let mut subs = Vec::new();
        for input in [&query, &include, &exclude] {
            subs.push(cx.subscribe(input, |this, _, event: &TextInputEvent, cx| {
                let TextInputEvent::Changed(_) = event;
                this.schedule_search(cx);
            }));
        }
        SearchPanel {
            project,
            query,
            include,
            exclude,
            regex: false,
            case_sensitive: false,
            whole_word: false,
            show_filters: false,
            results: Vec::new(),
            rows: Arc::new(Vec::new()),
            selected: None,
            status: Status::Idle,
            match_count: 0,
            cancel: None,
            search_task: None,
            debounce_task: None,
            list: VirtualListState::new(),
            focus_handle: cx.focus_handle().tab_stop(true),
            context_menu: ContextMenuState::default(),
            _subscriptions: subs,
        }
    }

    /// Focuses the query field (selecting its text).
    pub fn focus_query(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.query.focus_handle(cx);
        window.focus(&focus);
        self.query.update(cx, |q, cx| q.select_all_text(cx));
    }

    /// Sets the query text and searches.
    pub fn set_query(&mut self, text: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.query
            .update(cx, |q, cx| q.set_text(text.to_string(), cx));
        self.schedule_search(cx);
    }

    /// Sets the include globs ("Find in Folder") and shows the filters.
    pub fn set_include(&mut self, globs: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.include
            .update(cx, |q, cx| q.set_text(globs.to_string(), cx));
        self.show_filters = true;
        self.schedule_search(cx);
    }

    fn schedule_search(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        self.debounce_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            let _ = this.update(cx, |this, cx| this.search(cx));
        }));
    }

    fn globs(input: &Entity<TextInput>, cx: &App) -> Vec<String> {
        input
            .read(cx)
            .text()
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn search(&mut self, cx: &mut Context<Self>) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
        let pattern = self.query.read(cx).text().to_string();
        self.results.clear();
        self.rows = Arc::new(Vec::new());
        self.selected = None;
        self.match_count = 0;
        if pattern.is_empty() {
            self.status = Status::Idle;
            self.search_task = None;
            cx.notify();
            return;
        }
        let settings = self.project.settings();
        let query = SearchQuery {
            pattern,
            regex: self.regex,
            case: if self.case_sensitive {
                CaseMode::Sensitive
            } else {
                CaseMode::Smart
            },
            whole_word: self.whole_word,
            include: Self::globs(&self.include, cx),
            exclude: Self::globs(&self.exclude, cx),
            max_results: MAX_RESULTS,
        };
        let walk = WalkOptions {
            hidden: settings.files.show_hidden,
            git_ignore: true,
            exclude: settings.files.exclude.clone(),
            max_depth: None,
        };
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        self.status = Status::Searching;
        let root = self.project.root().to_path_buf();
        let (tx, mut rx) = futures::channel::mpsc::unbounded::<SearchEvent>();
        let started = Instant::now();
        std::thread::spawn(move || {
            let _ = lector_search::search_project(&root, &query, &walk, &cancel, |event| {
                let _ = tx.unbounded_send(event);
            });
        });
        cx.notify();
        self.search_task = Some(cx.spawn(async move |this, cx| {
            while let Some(event) = rx.next().await {
                let mut events = vec![event];
                while let Ok(e) = rx.try_recv() {
                    events.push(e);
                }
                let alive = this.update(cx, |this, cx| {
                    for e in events {
                        this.apply_event(e, started);
                    }
                    this.rebuild_rows();
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
                // Coalesce bursts so streaming never floods the main thread.
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;
            }
        }));
    }

    fn apply_event(&mut self, event: SearchEvent, started: Instant) {
        match event {
            SearchEvent::File(file) => {
                let rel = file.path.to_string_lossy().replace('\\', "/");
                let (dir, name) = match rel.rfind('/') {
                    Some(i) => (rel[..i].to_string(), rel[i + 1..].to_string()),
                    None => (String::new(), rel.clone()),
                };
                let path = if file.path.is_absolute() {
                    file.path.clone()
                } else {
                    self.project.root().join(&file.path)
                };
                self.match_count += file.matches.len();
                self.results.push(FileResult {
                    path,
                    name: name.into(),
                    dir: dir.into(),
                    matches: file.matches,
                    collapsed: false,
                });
            }
            SearchEvent::Done {
                files_searched: _,
                matches,
                truncated,
            } => {
                self.results
                    .sort_by(|a, b| a.dir.cmp(&b.dir).then_with(|| a.name.cmp(&b.name)));
                self.status = Status::Done {
                    matches,
                    files: self.results.len(),
                    truncated,
                    elapsed: started.elapsed(),
                };
            }
            SearchEvent::Error(err) => self.status = Status::Error(err.into()),
        }
    }

    fn rebuild_rows(&mut self) {
        let mut rows = Vec::new();
        for (fi, file) in self.results.iter().enumerate() {
            rows.push(RowRef::File(fi));
            if !file.collapsed {
                rows.extend((0..file.matches.len()).map(|mi| RowRef::Match(fi, mi)));
            }
        }
        self.rows = Arc::new(rows);
    }

    fn toggle(&mut self, which: u8, cx: &mut Context<Self>) {
        match which {
            0 => self.regex = !self.regex,
            1 => self.case_sensitive = !self.case_sensitive,
            2 => self.whole_word = !self.whole_word,
            _ => self.show_filters = !self.show_filters,
        }
        if which < 3 {
            self.schedule_search(cx);
        }
        cx.notify();
    }

    /// Turns off case/word/regex matching and clears the folder filters.
    fn reset_options(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.regex = false;
        self.case_sensitive = false;
        self.whole_word = false;
        self.include.update(cx, |i, cx| i.set_text("", cx));
        self.exclude.update(cx, |i, cx| i.set_text("", cx));
        self.schedule_search(cx);
        cx.notify();
    }

    fn activate_row(&mut self, ix: usize, preview: bool, cx: &mut Context<Self>) {
        self.selected = Some(ix);
        match self.rows.get(ix).copied() {
            Some(RowRef::File(fi)) => {
                if let Some(f) = self.results.get_mut(fi) {
                    f.collapsed = !f.collapsed;
                }
                self.rebuild_rows();
            }
            Some(RowRef::Match(fi, mi)) => {
                let file = &self.results[fi];
                let m = &file.matches[mi];
                let cols = m
                    .ranges
                    .first()
                    .cloned()
                    .map(|r| match_columns(m, r))
                    .unwrap_or(0..0);
                cx.emit(SearchPanelEvent::Open {
                    path: file.path.clone(),
                    row: m.line,
                    cols,
                    preview,
                });
            }
            None => {}
        }
        cx.notify();
    }

    fn deploy_menu(
        &mut self,
        ix: usize,
        position: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (fi, line) = match self.rows.get(ix).copied() {
            Some(RowRef::File(fi)) => (fi, None),
            Some(RowRef::Match(fi, mi)) => (fi, Some(self.results[fi].matches[mi].text.clone())),
            None => return,
        };
        let path = self.results[fi].path.clone();
        let rel = self
            .project
            .relative_path(&path)
            .unwrap_or_else(|| path.display().to_string());
        let weak = cx.entity().downgrade();
        let menu = ContextMenu::build(window, cx, move |menu, _, _| {
            let open = weak.clone();
            let abs = path.display().to_string();
            let rel = rel.clone();
            let reveal = path.clone();
            let mut menu = menu.icon_entry("Open", IconName::File, move |_, cx| {
                let _ = open.update(cx, |p, cx| p.activate_row(ix, false, cx));
            });
            if let Some(line) = line.clone() {
                menu = menu.icon_entry("Copy Line", IconName::Copy, move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(line.trim().to_string()))
                });
            }
            menu.separator()
                .icon_entry("Copy Path", IconName::Link, move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(abs.clone()))
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(rel.clone()))
                })
                .icon_entry(
                    crate::os::REVEAL_LABEL,
                    IconName::FolderOpen,
                    move |_, _| crate::os::reveal_in_file_manager(&reveal),
                )
        });
        self.context_menu
            .deploy(menu, position, window, cx, |this: &mut Self| {
                &mut this.context_menu
            });
        cx.notify();
    }

    fn select_next(&mut self, _: &SelectNext, _: &mut Window, cx: &mut Context<Self>) {
        self.step(1, cx);
    }

    fn select_prev(&mut self, _: &SelectPrev, _: &mut Window, cx: &mut Context<Self>) {
        self.step(-1, cx);
    }

    fn step(&mut self, delta: isize, cx: &mut Context<Self>) {
        let n = self.rows.len();
        if n == 0 {
            return;
        }
        let cur = self.selected.map(|s| s as isize).unwrap_or(-1);
        let mut next = (cur + delta).rem_euclid(n as isize) as usize;
        // Skip file headers when stepping through matches.
        if matches!(self.rows[next], RowRef::File(_)) && n > 1 {
            next = (next as isize + delta.signum()).rem_euclid(n as isize) as usize;
        }
        self.selected = Some(next);
        self.list.reveal(next);
        if matches!(self.rows[next], RowRef::Match(..)) {
            self.activate_row(next, true, cx);
        }
        cx.notify();
    }

    fn confirm(&mut self, _: &Confirm, _: &mut Window, cx: &mut Context<Self>) {
        let ix = self.selected.or_else(|| {
            self.rows
                .iter()
                .position(|r| matches!(r, RowRef::Match(..)))
        });
        if let Some(ix) = ix {
            self.activate_row(ix, false, cx);
        }
    }

    /// Empty states: before searching (hints) and when nothing matched.
    fn render_empty(&self, cx: &mut Context<Self>) -> AnyElement {
        let ui = cx.theme().ui.clone();
        let no_results = matches!(self.status, Status::Done { matches: 0, .. });
        if !no_results && !matches!(self.status, Status::Idle) {
            return div().into_any_element();
        }
        let has_filters = self.show_filters
            && (!self.include.read(cx).text().is_empty()
                || !self.exclude.read(cx).text().is_empty());
        let (title, body): (&str, &str) = if no_results {
            (
                "No results",
                if has_filters {
                    "Nothing matched. Check the include and exclude filters."
                } else if self.regex || self.whole_word || self.case_sensitive {
                    "Nothing matched. Try turning off match case, whole word or regex."
                } else {
                    "Nothing in the project matches this text."
                },
            )
        } else {
            (
                "Search the project",
                "Type to search every file (respects .gitignore). Use the buttons for case, whole words, regex and folder filters.",
            )
        };
        let this = cx.entity().downgrade();
        v_flex()
            .items_center()
            .pt(px(40.))
            .px(px(20.))
            .gap(px(6.))
            .child(
                Icon::new(IconName::TextSearch)
                    .size(IconSize::Custom(px(26.)))
                    .color(ui.text_faint.hsla()),
            )
            .child(
                div()
                    .mt(px(4.))
                    .text_size(metrics::text())
                    .text_color(ui.text_muted.hsla())
                    .child(title),
            )
            .child(
                div()
                    .text_size(metrics::text_sm())
                    .text_color(ui.text_faint.hsla())
                    .text_center()
                    .child(body),
            )
            .when(
                no_results && (has_filters || self.regex || self.whole_word || self.case_sensitive),
                |el| {
                    el.child(
                        crate::components::Button::new(
                            "search-reset-options",
                            "Reset Search Options",
                        )
                        .style(crate::components::ButtonStyle::Outlined)
                        .on_click(move |_, window, cx| {
                            let _ = this.update(cx, |p, cx| p.reset_options(window, cx));
                        }),
                    )
                },
            )
            .into_any_element()
    }

    fn render_status(&self, cx: &App) -> AnyElement {
        let ui = &cx.theme().ui;
        let text: SharedString = match &self.status {
            Status::Idle => return div().into_any_element(),
            Status::Searching => format!(
                "Searching… {} results in {} files",
                self.match_count,
                self.results.len()
            )
            .into(),
            Status::Done {
                matches,
                files,
                truncated,
                elapsed,
            } => {
                if *matches == 0 {
                    "No results".into()
                } else {
                    format!(
                        "{matches} result{} in {files} file{}{} · {} ms",
                        if *matches == 1 { "" } else { "s" },
                        if *files == 1 { "" } else { "s" },
                        if *truncated { " (limit reached)" } else { "" },
                        elapsed.as_millis()
                    )
                    .into()
                }
            }
            Status::Error(e) => e.clone(),
        };
        let error = matches!(self.status, Status::Error(_));
        div()
            .px(px(12.))
            .py(px(4.))
            .text_size(metrics::text_xs())
            .text_color(if error {
                cx.theme().git.deleted.hsla()
            } else {
                ui.text_faint.hsla()
            })
            .child(text)
            .into_any_element()
    }
}

/// Byte columns of a match within the original line (`text` may be a
/// window of a long line; we keep the offsets within `text`).
fn match_columns(m: &LineMatch, range: Range<usize>) -> Range<usize> {
    range.start.min(m.text.len())..range.end.min(m.text.len())
}

fn match_row(m: &LineMatch, selected: bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let ui = &theme.ui;
    // Trim leading whitespace, shifting highlight ranges.
    let trimmed = m.text.trim_start();
    let lead = m.text.len() - trimmed.len();
    let text: String = trimmed.chars().take(240).collect();
    let highlights: Vec<(Range<usize>, HighlightStyle)> = m
        .ranges
        .iter()
        .filter_map(|r| {
            let s = r.start.checked_sub(lead)?;
            let e = (r.end - lead).min(text.len());
            (s < e && text.is_char_boundary(s) && text.is_char_boundary(e)).then(|| {
                (
                    s..e,
                    HighlightStyle {
                        background_color: Some(ui.search_match.hsla()),
                        font_weight: Some(FontWeight::SEMIBOLD),
                        color: Some(ui.text.hsla()),
                        ..Default::default()
                    },
                )
            })
        })
        .collect();
    h_flex()
        .h(px(22.))
        .pl(px(30.))
        .pr(px(8.))
        .gap(px(8.))
        .text_size(metrics::text_sm())
        .when(selected, |el| el.bg(ui.active.hsla()))
        .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
        .child(
            div()
                .flex_none()
                .min_w(px(28.))
                .text_color(ui.text_faint.hsla())
                .text_size(metrics::text_xs())
                .child(format!("{}", m.line + 1)),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .whitespace_nowrap()
                .text_color(ui.text_muted.hsla())
                .child(StyledText::new(text).with_highlights(highlights)),
        )
        .into_any_element()
}

impl Render for SearchPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let ui = cx.theme().ui.clone();
        let this = cx.entity().downgrade();
        let toggle = |id: &'static str, icon: IconName, on: bool, which: u8, tip: &'static str| {
            let this = this.clone();
            IconButton::new(id, icon)
                .icon_size(IconSize::Small)
                .selected(on)
                .tooltip(Tooltip::text(tip))
                .on_click(move |_, _, cx| {
                    let _ = this.update(cx, |p, cx| p.toggle(which, cx));
                })
        };
        let count = self.rows.len();
        let body = if count == 0 {
            self.render_empty(cx)
        } else {
            let weak = cx.entity().downgrade();
            virtual_list(
                "search-results",
                count,
                &self.list,
                cx,
                move |range, _window, cx| {
                    let Some(panel) = weak.upgrade() else {
                        return Vec::new();
                    };
                    let p = panel.read(cx);
                    let rows = p.rows.clone();
                    range
                        .map(|ix| {
                            let selected = p.selected == Some(ix);
                            let el = match rows[ix] {
                                RowRef::File(fi) => {
                                    let f = &p.results[fi];
                                    file_row(f, selected, cx)
                                }
                                RowRef::Match(fi, mi) => {
                                    match_row(&p.results[fi].matches[mi], selected, cx)
                                }
                            };
                            let weak = panel.downgrade();
                            let menu_weak = panel.downgrade();
                            div()
                                .id(("search-row", ix))
                                .cursor_pointer()
                                .on_click(move |event, _, cx| {
                                    let preview = event.click_count() < 2;
                                    let _ =
                                        weak.update(cx, |p, cx| p.activate_row(ix, preview, cx));
                                })
                                .on_mouse_down(
                                    gpui::MouseButton::Right,
                                    move |event, window, cx| {
                                        let position = event.position;
                                        let _ = menu_weak.update(cx, |p, cx| {
                                            p.deploy_menu(ix, position, window, cx)
                                        });
                                    },
                                )
                                .child(el)
                                .into_any_element()
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
            .on_action(cx.listener(
                |this, _: &crate::actions::workspace::ShowContextMenu, window, cx| {
                    match this.selected {
                        Some(ix) => {
                            let position = this.list.row_anchor(ix, px(22.));
                            this.deploy_menu(ix, position, window, cx)
                        }
                        None => cx.propagate(),
                    }
                },
            ))
            .child(
                v_flex()
                    .px(px(8.))
                    .pb(px(4.))
                    .gap(px(5.))
                    .child(
                        h_flex()
                            .gap(px(2.))
                            .child(div().flex_1().min_w_0().child(input_box(
                                self.query.clone(),
                                IconName::Search,
                                cx,
                            )))
                            .child(toggle(
                                "search-case",
                                IconName::CaseSensitive,
                                self.case_sensitive,
                                1,
                                "Match Case",
                            ))
                            .child(toggle(
                                "search-word",
                                IconName::WholeWord,
                                self.whole_word,
                                2,
                                "Match Whole Word",
                            ))
                            .child(toggle(
                                "search-regex",
                                IconName::Regex,
                                self.regex,
                                0,
                                "Use Regular Expression",
                            ))
                            .child(toggle(
                                "search-filters",
                                IconName::Filter,
                                self.show_filters,
                                3,
                                "Include / Exclude Files",
                            )),
                    )
                    .when(self.show_filters, |el| {
                        el.child(input_box(self.include.clone(), IconName::Files, cx))
                            .child(input_box(self.exclude.clone(), IconName::EyeOff, cx))
                    }),
            )
            .children(self.context_menu.render())
            .child(self.render_status(cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .border_t_1()
                    .border_color(ui.border_variant.hsla())
                    .pt(px(2.))
                    .child(body),
            )
    }
}

fn file_row(f: &FileResult, selected: bool, cx: &App) -> AnyElement {
    let ui = &cx.theme().ui;
    h_flex()
        .h(px(22.))
        .pl(px(8.))
        .pr(px(8.))
        .gap(px(5.))
        .text_size(metrics::text())
        .when(selected, |el| el.bg(ui.active.hsla()))
        .when(!selected, |el| el.hover(|s| s.bg(ui.hover.hsla())))
        .child(
            Icon::new(if f.collapsed {
                IconName::ChevronRight
            } else {
                IconName::ChevronDown
            })
            .size(IconSize::XSmall)
            .color(ui.text_faint.hsla()),
        )
        .child(FileIcon::file(f.name.clone()).size(px(15.)))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .gap(px(6.))
                .overflow_hidden()
                .whitespace_nowrap()
                .child(
                    div()
                        .flex_none()
                        .text_color(ui.text.hsla())
                        .child(f.name.clone()),
                )
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_size(metrics::text_xs())
                        .text_color(ui.text_faint.hsla())
                        .child(f.dir.clone()),
                ),
        )
        .child(
            div()
                .flex_none()
                .px(px(6.))
                .rounded(px(8.))
                .bg(ui.hover.hsla())
                .text_size(metrics::text_xs())
                .text_color(ui.text_muted.hsla())
                .child(format!("{}", f.matches.len())),
        )
        .into_any_element()
}
