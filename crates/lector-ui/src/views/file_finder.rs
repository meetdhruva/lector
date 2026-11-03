//! File finder (go to file): a nucleo picker over `Project::files()`, with
//! recent files first for an empty query, `path:line[:col]` suffixes, file
//! icons and git status colors.

use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{prelude::*, px, AnyElement, App, DismissEvent, SharedString, Task, Window};
use lector_workspace::{FileList, GitStatusMap};

use super::file_tree::git_color;
use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::fuzzy::NucleoList;
use crate::icons::FileIcon;

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
    list: NucleoList<Arc<str>>,
    rows: Vec<Row>,
    selected: usize,
    line: Option<(u32, Option<u32>)>,
    on_open: OpenCallback,
    tick_task: Option<Task<()>>,
    counts: (u32, u32),
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
    ) -> Self {
        let list = NucleoList::new(true);
        let injector = list.injector();
        let feed = files.clone();
        // Pushing a million paths takes a while; do it off the main thread.
        std::thread::spawn(move || {
            for path in &feed.paths {
                injector.push(path.clone(), path);
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
            tick_task: None,
            counts: (0, 0),
        }
    }

    fn refresh_rows(&mut self) {
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

    fn update_matches(
        &mut self,
        query: &str,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) {
        let (path_query, line) = split_line_suffix(query.trim());
        self.line = line;
        self.selected = 0;
        let path_query = path_query.trim().replace('\\', "/");
        if path_query.is_empty() {
            self.tick_task = None;
            self.empty_query_rows();
            return;
        }
        self.list.set_query(&path_query);
        let running = self.list.tick(8);
        self.refresh_rows();
        if running || self.list.counts().1 < self.files.paths.len() as u32 {
            self.tick_task = Some(cx.spawn(async move |this, cx| loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let Ok(done) = this.update(cx, |picker, cx| {
                    let d = &mut picker.delegate;
                    let running = d.list.tick(5);
                    d.refresh_rows();
                    cx.notify();
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
        let Some(row) = self.rows.get(self.selected) else {
            return;
        };
        let path = self.root.join(&*row.rel);
        let line = self.line;
        cx.emit(DismissEvent);
        let on_open = self.on_open.clone();
        window.defer(cx, move |window, cx| on_open(path, line, window, cx));
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
