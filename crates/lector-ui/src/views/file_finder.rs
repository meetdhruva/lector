//! File finder (go to file): a picker over project-relative paths.
//! Integration swaps the walker and matcher for `lector_search`.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{prelude::*, px, AnyElement, App, DismissEvent, SharedString, Window};

use crate::components::picker::{Picker, PickerDelegate, PickerItem};
use crate::fuzzy::match_and_sort;
use crate::icons::FileIcon;

const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    "target",
    ".hg",
    ".svn",
    "dist",
    "build",
    ".venv",
    "__pycache__",
];
const MAX_FILES: usize = 200_000;

/// Walks `root` (depth-first, skipping VCS/build directories) and returns
/// relative paths, sorted. Blocking; run it on a background thread.
pub fn walk_project(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            if ft.is_dir() {
                if !SKIP_DIRS.contains(&name.as_ref()) {
                    stack.push(path);
                }
            } else if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_string_lossy().replace('\\', "/"));
                if out.len() >= MAX_FILES {
                    out.sort();
                    return out;
                }
            }
        }
    }
    out.sort();
    out
}

type OpenCallback = Rc<dyn Fn(PathBuf, &mut Window, &mut App)>;

/// File finder delegate.
pub struct FileFinderDelegate {
    root: PathBuf,
    files: Arc<Vec<String>>,
    matches: Vec<(usize, Vec<usize>)>,
    selected: usize,
    on_open: OpenCallback,
}

impl FileFinderDelegate {
    /// Finder over `files` (relative to `root`).
    pub fn new(
        root: PathBuf,
        files: Arc<Vec<String>>,
        on_open: impl Fn(PathBuf, &mut Window, &mut App) + 'static,
    ) -> Self {
        FileFinderDelegate {
            root,
            files,
            matches: Vec::new(),
            selected: 0,
            on_open: Rc::new(on_open),
        }
    }
}

impl PickerDelegate for FileFinderDelegate {
    fn placeholder(&self) -> SharedString {
        "Go to file…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
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
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.matches = if query.trim().is_empty() {
            (0..self.files.len().min(200))
                .map(|ix| (ix, Vec::new()))
                .collect()
        } else {
            let mut m = match_and_sort(query, self.files.iter().map(String::as_str));
            m.truncate(500);
            m.into_iter().map(|(ix, m)| (ix, m.positions)).collect()
        };
        self.selected = 0;
    }

    fn confirm(&mut self, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        let Some((ix, _)) = self.matches.get(self.selected) else {
            return;
        };
        let path = self.root.join(&self.files[*ix]);
        cx.emit(DismissEvent);
        let on_open = self.on_open.clone();
        window.defer(cx, move |window, cx| on_open(path, window, cx));
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> AnyElement {
        let (file_ix, positions) = &self.matches[ix];
        let rel = &self.files[*file_ix];
        let (dir, name) = match rel.rfind('/') {
            Some(slash) => (&rel[..slash], &rel[slash + 1..]),
            None => ("", rel.as_str()),
        };
        let name_offset = rel.len() - name.len();
        let name_positions: Vec<usize> = positions
            .iter()
            .filter(|p| **p >= name_offset)
            .map(|p| p - name_offset)
            .collect();
        let dir_positions: Vec<usize> = positions
            .iter()
            .copied()
            .filter(|p| *p < dir.len())
            .collect();
        PickerItem::new(SharedString::from(name.to_string()), selected)
            .icon(FileIcon::file(SharedString::from(name.to_string())).size(px(16.)))
            .positions(name_positions)
            .detail(SharedString::from(dir.to_string()), dir_positions)
            .into_any_element()
    }

    fn empty_message(&self) -> SharedString {
        if self.files.is_empty() {
            "Indexing files…".into()
        } else {
            "No matching files".into()
        }
    }
}
