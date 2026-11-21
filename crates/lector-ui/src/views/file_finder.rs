//! File finder (go to file): a nucleo picker over `Project::files()`, with
//! recent files first for an empty query, `path:line[:col]` suffixes, file
//! icons and git status colors.

use crate::background::{background, JobError, JobKey, JobKind, Priority, Subject};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use crate::scale::px;
use gpui::{prelude::*, AnyElement, App, DismissEvent, SharedString, Task, Window};
use lector_app::project::{FileList, GitStatusMap};

use super::file_tree::git_color;
use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::icons::FileIcon;
use path_list::PathList;

mod path_list;

type OpenCallback = Rc<dyn Fn(PathBuf, Option<(u32, Option<u32>)>, &mut Window, &mut App)>;

/// Splits a `path:line[:col]` query (1-based numbers).
pub fn split_line_suffix(query: &str) -> (&str, Option<(u32, Option<u32>)>) {
    let mut parts = query.rsplitn(3, ':');
    let last = parts.next();
    let mid = parts.next();
    let first = parts.next();
    match (first, mid, last) {
        (Some(path), Some(line), Some(col)) => {
            if let (Ok(l), Ok(c)) = (line.parse::<u32>(), col.parse::<u32>()) {
                return (path, Some((l, Some(c))));
            }
            if let Ok(c) = col.parse::<u32>() {
                let path_len = query.len() - col.len() - 1;
                return (&query[..path_len], Some((c, None)));
            }
            (query, None)
        }
        (None, Some(path), Some(line)) => match line.parse::<u32>() {
            Ok(l) => (path, Some((l, None))),
            Err(_) if line.is_empty() => (path, None),
            Err(_) => (query, None),
        },
        _ => (query, None),
    }
}

#[derive(Clone)]
struct Row {
    rel: Arc<str>,
    positions: Vec<usize>,
    recent: bool,
}

/// File finder delegate.
pub struct FileFinderDelegate {
    root: PathBuf,
    files: Arc<FileList>,
    recent: Vec<Arc<str>>,
    status: Arc<GitStatusMap>,
    list: PathList,
    rows: Vec<Row>,
    selected: usize,
    line: Option<(u32, Option<u32>)>,
    on_open: OpenCallback,
    tick_task: Option<Task<()>>,
    counts: (u32, u32),
    /// The [`PathList::stamp`] the rows were read under; they answer the latest query only when
    /// it equals `Some(list.latest())`.
    rows_stamp: Option<u64>,
    /// Enter was pressed while the rows were still those of an older query.
    confirm_waiting: bool,
    /// Feeds the candidates; dropping it (the finder closed) stops the feed.
    _feed_task: Task<Result<(), JobError>>,
}

const MAX_ROWS: u32 = 300;

impl FileFinderDelegate {
    /// Finder over `files` (relative to `root`); `recent` are relative paths,
    /// most recent first.
    pub fn new(
        root: PathBuf,
        files: Arc<FileList>,
        recent: Vec<Arc<str>>,
        status: Arc<GitStatusMap>,
        on_open: impl Fn(PathBuf, Option<(u32, Option<u32>)>, &mut Window, &mut App) + 'static,
        cx: &App,
    ) -> Self {
        let list = PathList::new();
        let injector = list.injector();
        let feed = files.clone();
        // Pushing a million paths takes a while: off the main thread, stopped when the finder
        // closes (dropping `feed_task` cancels the job).
        let key = JobKey::new(JobKind::PickerFeed, Subject::unique());
        let feed_task = background(cx, key, Priority::Visible, move |cancel| {
            for chunk in feed.paths.chunks(4096) {
                if cancel.is_cancelled() {
                    return;
                }
                for path in chunk {
                    injector.push(path.clone(), path);
                }
            }
        });
        FileFinderDelegate {
            root,
            files,
            recent,
            status,
            list,
            rows: Vec::new(),
            selected: 0,
            line: None,
            on_open: Rc::new(on_open),
            _feed_task: feed_task,
            tick_task: None,
            counts: (0, 0),
            rows_stamp: None,
            confirm_waiting: false,
        }
    }

    /// Whether the rows shown answer the latest query (an empty query is answered at once).
    fn rows_are_current(&self) -> bool {
        self.rows_stamp == Some(self.list.latest())
    }

    /// Opens the selected row, which answers the latest query.
    fn confirm_selected(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let path = self.root.join(&*row.rel);
        let line = self.line;
        cx.emit(DismissEvent);
        let on_open = self.on_open.clone();
        window.defer(cx, move |window, cx| on_open(path, line, window, cx));
    }

    fn refresh_rows(&mut self) {
        // Read before the rows: published results only ever move on to newer queries, so rows
        // read after a "current" stamp answer that query.
        self.rows_stamp = self.list.stamp();
        let recent: std::collections::HashSet<&str> =
            self.recent.iter().map(|r| r.as_ref()).collect();
        self.rows = self
            .list
            .top(MAX_ROWS)
            .into_iter()
            .map(|m| Row {
                recent: recent.contains(m.item.as_ref()),
                rel: m.item,
                positions: m.positions,
            })
            .collect();
        self.counts = self.list.counts();
    }

    fn empty_query_rows(&mut self) {
        let mut rows: Vec<Row> = self
            .recent
            .iter()
            .filter(|r| self.files.contains(r) || self.files.paths.is_empty())
            .map(|r| Row {
                rel: r.clone(),
                positions: Vec::new(),
                recent: true,
            })
            .collect();
        let recent: std::collections::HashSet<Arc<str>> = self.recent.iter().cloned().collect();
        rows.extend(
            self.files
                .paths
                .iter()
                .filter(|p| !recent.contains(*p))
                .take(MAX_ROWS as usize)
                .map(|p| Row {
                    rel: p.clone(),
                    positions: Vec::new(),
                    recent: false,
                }),
        );
        self.rows = rows;
        self.counts = (self.files.paths.len() as u32, self.files.paths.len() as u32);
        self.rows_stamp = Some(self.list.latest());
    }
}

impl PickerDelegate for FileFinderDelegate {
    fn placeholder(&self) -> SharedString {
        "Go to file… (append :line to jump)".into()
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
        let (path_query, line) = split_line_suffix(query.trim());
        self.line = line;
        self.selected = 0;
        self.confirm_waiting = false;
        let path_query = path_query.trim().replace('\\', "/");
        if path_query.is_empty() {
            self.tick_task = None;
            self.empty_query_rows();
            return;
        }
        self.list.set_query(&path_query);
        let running = self.list.tick(8);
        self.refresh_rows();
        if running
            || !self.rows_are_current()
            || self.list.counts().1 < self.files.paths.len() as u32
        {
            self.tick_task = Some(cx.spawn_in(window, async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let Ok(done) = this.update_in(cx, |picker, window, cx| {
                    let d = &mut picker.delegate;
                    let running = d.list.tick(5);
                    d.refresh_rows();
                    cx.notify();
                    // An Enter that waited for this query's rows applies to their top row.
                    if d.rows_are_current() && std::mem::take(&mut d.confirm_waiting) {
                        d.selected = 0;
                        d.confirm_selected(window, cx);
                    }
                    !running && d.list.counts().1 >= d.files.paths.len() as u32
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
        // The matcher publishes asynchronously: right after a keystroke the rows may still be
        // the previous query's. Open only rows of the latest query, otherwise wait for them.
        if self.rows_are_current() {
            self.confirm_selected(window, cx);
        } else {
            self.confirm_waiting = true;
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {
        self.confirm_waiting = false;
        self.tick_task = None;
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let row = &self.rows[ix];
        let rel = row.rel.as_ref();
        let (dir, name) = match rel.rfind('/') {
            Some(slash) => (&rel[..slash], &rel[slash + 1..]),
            None => ("", rel),
        };
        let name_offset = rel.len() - name.len();
        let name_positions: Vec<usize> = row
            .positions
            .iter()
            .filter(|p| **p >= name_offset)
            .map(|p| p - name_offset)
            .collect();
        let dir_positions: Vec<usize> = row
            .positions
            .iter()
            .copied()
            .filter(|p| *p < dir.len())
            .collect();
        let status = self.status.get(rel).map(|s| s.state);
        let mut item = PickerItem::new(SharedString::from(name.to_string()), selected)
            .icon(FileIcon::file(SharedString::from(name.to_string())).size(px(16.)))
            .positions(name_positions)
            .detail(SharedString::from(dir.to_string()), dir_positions);
        if let Some(state) = status {
            item = item
                .label_color(git_color(state, cx))
                .end_text(Some(super::file_tree::git_badge(state).into()));
        } else if row.recent && ix < self.recent.len() {
            item = item.end_text(Some("recent".into()));
        }
        item.into_any_element()
    }

    fn empty_message(&self) -> SharedString {
        if self.files.paths.is_empty() {
            "Indexing files…".into()
        } else {
            "No matching files".into()
        }
    }

    fn width(&self) -> gpui::Pixels {
        px(680.)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use gpui::{Focusable as _, TestAppContext};
    use lector_theme::{Appearance, ThemeRegistry};

    use super::*;
    use crate::theme::{ThemeMode, ThemeState};

    type Opened = Rc<RefCell<Vec<PathBuf>>>;

    fn finder(
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<Picker<FileFinderDelegate>>,
        Opened,
        &mut gpui::VisualTestContext,
    ) {
        cx.update(|cx| {
            ThemeState::init(cx, ThemeRegistry::new(), ThemeMode::Dark, Appearance::Dark);
        });
        cx.executor().allow_parking();
        let mut files = FileList::default();
        files.paths = ["src/alpha.rs", "src/beta.rs", "src/gamma.rs"]
            .map(Arc::<str>::from)
            .to_vec();
        let opened = Opened::default();
        let sink = opened.clone();
        let (picker, cx) = cx.add_window_view(|window, cx| {
            let delegate = FileFinderDelegate::new(
                PathBuf::from("/root"),
                Arc::new(files),
                Vec::new(),
                Arc::default(),
                move |path, _, _, _| sink.borrow_mut().push(path),
                cx,
            );
            Picker::new(delegate, window, cx)
        });
        cx.update(|window, cx| window.focus(&picker.focus_handle(cx)));
        (picker, opened, cx)
    }

    fn settle(
        cx: &mut gpui::VisualTestContext,
        done: impl Fn(&mut gpui::VisualTestContext) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            cx.run_until_parked();
            if done(cx) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "timed out");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    fn top(
        picker: &gpui::Entity<Picker<FileFinderDelegate>>,
        cx: &mut gpui::VisualTestContext,
    ) -> Option<String> {
        picker.read_with(cx, |p, _| {
            p.delegate.rows.first().map(|r| r.rel.to_string())
        })
    }

    #[gpui::test]
    fn enter_never_opens_a_row_of_an_older_query(cx: &mut TestAppContext) {
        let (picker, opened, cx) = finder(cx);
        picker.update_in(cx, |p, window, cx| p.set_query("alpha", window, cx));
        settle(cx, |cx| top(&picker, cx).as_deref() == Some("src/alpha.rs"));
        // The user typed "beta" but the matcher has not published for it yet: the rows on screen
        // are still those of "alpha". Enter must not open them.
        picker.update_in(cx, |p, window, cx| {
            p.delegate.list.set_query("beta");
            p.delegate.confirm(window, cx);
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert!(opened.borrow().is_empty(), "opened {:?}", opened.borrow());
    }

    #[gpui::test]
    fn enter_right_after_typing_opens_the_new_querys_top_row(cx: &mut TestAppContext) {
        let (picker, opened, cx) = finder(cx);
        picker.update_in(cx, |p, window, cx| p.set_query("alpha", window, cx));
        settle(cx, |cx| top(&picker, cx).as_deref() == Some("src/alpha.rs"));
        picker.update_in(cx, |p, window, cx| p.set_query("beta", window, cx));
        // Whether the matcher was quick enough or not, Enter ends up on "beta", never "alpha".
        picker.update_in(cx, |p, window, cx| p.delegate.confirm(window, cx));
        settle(cx, |_| !opened.borrow().is_empty());
        assert_eq!(*opened.borrow(), [PathBuf::from("/root/src/beta.rs")]);
    }

    #[gpui::test]
    fn dismissing_drops_a_waiting_enter(cx: &mut TestAppContext) {
        let (picker, opened, cx) = finder(cx);
        picker.update_in(cx, |p, window, cx| p.set_query("alpha", window, cx));
        settle(cx, |cx| top(&picker, cx).as_deref() == Some("src/alpha.rs"));
        picker.update_in(cx, |p, window, cx| {
            p.delegate.list.set_query("beta");
            p.delegate.confirm(window, cx);
            assert!(p.delegate.confirm_waiting);
            p.delegate.dismissed(window, cx);
            assert!(!p.delegate.confirm_waiting);
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
        cx.run_until_parked();
        assert!(opened.borrow().is_empty(), "opened {:?}", opened.borrow());
    }

    use super::split_line_suffix;

    #[test]
    fn line_suffixes() {
        assert_eq!(
            split_line_suffix("main.rs:42"),
            ("main.rs", Some((42, None)))
        );
        assert_eq!(
            split_line_suffix("main.rs:42:7"),
            ("main.rs", Some((42, Some(7))))
        );
        assert_eq!(split_line_suffix("main.rs:"), ("main.rs", None));
        assert_eq!(split_line_suffix("main.rs"), ("main.rs", None));
        assert_eq!(split_line_suffix("a:b"), ("a:b", None));
        assert_eq!(split_line_suffix("C:x:12"), ("C:x", Some((12, None))));
    }
}
