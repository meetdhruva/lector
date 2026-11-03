use std::path::{Path, PathBuf};

use ignore::{WalkBuilder, WalkState};

use crate::globs::build_set;
use crate::CancelToken;

/// Options for [`walk_files`].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct WalkOptions {
    /// Include hidden (dot) files and directories. `.git` is always skipped.
    pub hidden: bool,
    /// Respect `.gitignore`, `.ignore`, `.git/info/exclude` and the global gitignore
    /// (also outside of git repositories).
    pub git_ignore: bool,
    /// Glob patterns applied to root-relative paths; matching directories are pruned.
    /// Patterns without `/` match at any depth (`node_modules`, `*.min.js`).
    pub exclude: Vec<String>,
    /// Maximum directory depth below the root (`None` = unlimited).
    pub max_depth: Option<usize>,
}

/// Builds the shared walker used by [`walk_files`] and project search.
pub(crate) fn build_walker(
    root: &Path,
    opts: &WalkOptions,
    extra_exclude: &[String],
) -> WalkBuilder {
    let mut patterns = opts.exclude.clone();
    patterns.extend_from_slice(extra_exclude);
    let (exclude, _invalid) = build_set(&patterns);
    let root_owned = root.to_path_buf();

    let mut b = WalkBuilder::new(root);
    b.hidden(!opts.hidden)
        .git_ignore(opts.git_ignore)
        .git_global(opts.git_ignore)
        .git_exclude(opts.git_ignore)
        .ignore(opts.git_ignore)
        .parents(opts.git_ignore)
        .require_git(false)
        .follow_links(false)
        .max_depth(opts.max_depth.map(|d| d.max(1)));
    b.filter_entry(move |entry| {
        if entry.depth() == 0 {
            return true;
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir && entry.file_name() == ".git" {
            return false;
        }
        if exclude.is_empty() {
            return true;
        }
        match entry.path().strip_prefix(&root_owned) {
            Ok(rel) => !exclude.is_match(rel),
            Err(_) => true,
        }
    });
    b
}

/// Walks `root` in parallel and calls `sink` with every regular file's path **relative to
/// `root`**. Order is unspecified. Returns early (promptly) once `cancel` is set.
pub fn walk_files(
    root: &Path,
    opts: &WalkOptions,
    cancel: &CancelToken,
    sink: impl Fn(PathBuf) + Send + Sync,
) {
    let walker = build_walker(root, opts, &[]).build_parallel();
    let sink = &sink;
    walker.run(|| {
        Box::new(move |entry| {
            if cancel.is_cancelled() {
                return WalkState::Quit;
            }
            let Ok(entry) = entry else {
                return WalkState::Continue;
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                return WalkState::Continue;
            }
            if let Ok(rel) = entry.path().strip_prefix(root) {
                sink(rel.to_path_buf());
            }
            WalkState::Continue
        })
    });
}
