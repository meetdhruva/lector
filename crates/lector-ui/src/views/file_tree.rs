//! The project file tree: a [`TreeDelegate`] over the project's file list
//! (gitignore-aware, from `Project::files()`), sorted folders-first, with
//! git status colors and badges (folders roll up their descendants).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{App, ClipboardItem, Entity, Hsla, SharedString, Window};
use lector_workspace::{FileGitState, FileList, GitStatusMap};

use crate::components::context_menu::ContextMenu;
use crate::components::tree_view::{TreeDelegate, TreeIcon, TreeNode};
use crate::icons::IconName;
use crate::theme::{ActiveTheme, ColorExt};

/// One directory entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DirEntry {
    /// File or folder name.
    pub name: Arc<str>,
    /// Whether it is a folder.
    pub is_dir: bool,
    /// Ignored by git (shown dimmed when `files.show_ignored` is on).
    pub ignored: bool,
}

/// Directory listing derived from a [`FileList`]: relative dir → children,
/// folders first, then files, case-insensitively sorted.
#[derive(Debug, Default)]
pub struct DirIndex {
    /// Version of the file list it was built from.
    pub version: u64,
    children: HashMap<String, Vec<DirEntry>>,
}

impl DirIndex {
    /// Builds the index from a project file list (O(n log n); run on a
    /// background thread for big lists).
    pub fn from_list(files: &FileList) -> DirIndex {
        Self::build(files.version, &files.paths, |rel| files.is_ignored(rel))
    }

    /// Builds the index from sorted relative paths.
    pub fn build(version: u64, paths: &[Arc<str>], is_ignored: impl Fn(&str) -> bool) -> DirIndex {
        let mut children: HashMap<String, Vec<DirEntry>> = HashMap::new();
        let mut seen_dirs: std::collections::HashSet<String> = std::collections::HashSet::new();
        for path in paths {
            let mut parent = String::new();
            let mut components = path.split('/').peekable();
            while let Some(component) = components.next() {
                let is_dir = components.peek().is_some();
                let full = if parent.is_empty() {
                    component.to_string()
                } else {
                    format!("{parent}/{component}")
                };
                if is_dir {
                    if seen_dirs.insert(full.clone()) {
                        children.entry(parent.clone()).or_default().push(DirEntry {
                            name: component.into(),
                            is_dir: true,
                            ignored: false,
                        });
                    }
                } else {
                    children.entry(parent.clone()).or_default().push(DirEntry {
                        name: component.into(),
                        is_dir: false,
                        ignored: is_ignored(path),
                    });
                }
                parent = full;
            }
        }
        for list in children.values_mut() {
            list.sort_by(|a, b| {
                b.is_dir
                    .cmp(&a.is_dir)
                    .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
                    .then_with(|| a.name.cmp(&b.name))
            });
        }
        DirIndex { version, children }
    }

    /// Children of the relative directory `dir` ("" = root).
    pub fn children(&self, dir: &str) -> &[DirEntry] {
        self.children.get(dir).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// Commands offered by the tree's context menu (handled by the workspace).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeCommand {
    /// Open the file.
    Open,
    /// Open in a split to the right.
    OpenToSide,
    /// Show in the OS file manager.
    RevealInOs,
    /// Search inside this folder.
    FindInFolder,
    /// Collapse all folders.
    CollapseAll,
}

type CommandHandler = Rc<dyn Fn(TreeCommand, PathBuf, &mut Window, &mut App)>;

/// A [`TreeDelegate`] over the project's file list.
pub struct FsTreeDelegate {
    root: PathBuf,
    root_name: SharedString,
    index: Arc<DirIndex>,
    status: Arc<GitStatusMap>,
    on_command: Option<CommandHandler>,
}

/// Single-letter badge for a git state.
pub fn git_badge(state: FileGitState) -> &'static str {
    match state {
        FileGitState::Modified => "M",
        FileGitState::Added => "A",
        FileGitState::Deleted => "D",
        FileGitState::Renamed => "R",
        FileGitState::Untracked => "U",
        FileGitState::Conflicted => "!",
        FileGitState::Ignored => "I",
    }
}

/// Theme color for a git state.
pub fn git_color(state: FileGitState, cx: &App) -> Hsla {
    let git = &cx.theme().git;
    match state {
        FileGitState::Modified => git.modified,
        FileGitState::Added => git.added,
        FileGitState::Deleted => git.deleted,
        FileGitState::Renamed => git.renamed,
        FileGitState::Untracked => git.untracked,
        FileGitState::Conflicted => git.conflicted,
        FileGitState::Ignored => git.ignored,
    }
    .hsla()
}

impl FsTreeDelegate {
    /// Tree rooted at `root` (empty until [`Self::set_files`]).
    pub fn new(root: PathBuf) -> Self {
        let root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string())
            .into();
        FsTreeDelegate {
            root,
            root_name,
            index: Arc::new(DirIndex::default()),
            status: Arc::new(GitStatusMap::default()),
            on_command: None,
        }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Replaces the listing (call `TreeView::refresh` afterwards).
    pub fn set_index(&mut self, index: Arc<DirIndex>) {
        self.index = index;
    }

    /// Replaces the git status (call `TreeView::refresh` afterwards).
    pub fn set_git_status(&mut self, status: Arc<GitStatusMap>) {
        self.status = status;
    }

    /// Current listing.
    pub fn index(&self) -> &Arc<DirIndex> {
        &self.index
    }

    /// Sets the handler for context-menu commands.
    pub fn set_command_handler(
        &mut self,
        handler: impl Fn(TreeCommand, PathBuf, &mut Window, &mut App) + 'static,
    ) {
        self.on_command = Some(Rc::new(handler));
    }

    fn rel(&self, path: &Path) -> Option<String> {
        let rel = path.strip_prefix(&self.root).ok()?;
        Some(rel.to_string_lossy().replace('\\', "/"))
    }

    /// Ancestors of `path` from the root (inclusive) to its parent, for
    /// [`crate::components::tree_view::TreeView::reveal`].
    pub fn ancestors_of(&self, path: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let Ok(rel) = path.strip_prefix(&self.root) else {
            return out;
        };
        out.push(self.root.clone());
        let mut cur = self.root.clone();
        let components: Vec<_> = rel.components().collect();
        for c in components.iter().take(components.len().saturating_sub(1)) {
            cur.push(c);
            out.push(cur.clone());
        }
        out
    }
}

impl TreeDelegate for FsTreeDelegate {
    type Id = PathBuf;

    fn children(&mut self, parent: Option<&PathBuf>, cx: &mut App) -> Vec<TreeNode<PathBuf>> {
        let Some(parent) = parent else {
            return vec![TreeNode::branch(
                self.root.clone(),
                self.root_name.clone(),
                TreeIcon::RootFolder,
            )];
        };
        let Some(rel_dir) = self.rel(parent) else {
            return Vec::new();
        };
        let ignored = cx.theme().git.ignored.hsla();
        self.index
            .children(&rel_dir)
            .iter()
            .map(|entry| {
                let name: SharedString = entry.name.to_string().into();
                let rel = if rel_dir.is_empty() {
                    entry.name.to_string()
                } else {
                    format!("{rel_dir}/{}", entry.name)
                };
                let path = parent.join(&*entry.name);
                let mut node = if entry.is_dir {
                    TreeNode::branch(path, name.clone(), TreeIcon::Folder(name))
                } else {
                    TreeNode::leaf(path, name.clone(), TreeIcon::File(name))
                };
                let state = if entry.is_dir {
                    self.status.dir_state(&rel)
                } else {
                    self.status
                        .get(&rel)
                        .map(|s| s.state)
                        .or(entry.ignored.then_some(FileGitState::Ignored))
                };
                if let Some(state) = state {
                    let color = if state == FileGitState::Ignored {
                        ignored
                    } else {
                        git_color(state, cx)
                    };
                    node.color = Some(color);
                    if entry.is_dir {
                        if state != FileGitState::Ignored {
                            node.badge = Some(("•".into(), color));
                        }
                    } else {
                        node.badge = Some((git_badge(state).into(), color));
                    }
                }
                node
            })
            .collect()
    }

    fn context_menu(
        &mut self,
        node: &TreeNode<PathBuf>,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<Entity<ContextMenu>> {
        let abs = node.id.display().to_string();
        let rel = self.rel(&node.id).unwrap_or_else(|| abs.clone());
        let is_branch = node.is_branch;
        let path = node.id.clone();
        let handler = self.on_command.clone();
        Some(ContextMenu::build(window, cx, move |menu, _, _| {
            let cmd = |command: TreeCommand| {
                let handler = handler.clone();
                let path = path.clone();
                move |window: &mut Window, cx: &mut App| {
                    if let Some(h) = &handler {
                        h(command, path.clone(), window, cx);
                    }
                }
            };
            let mut menu = menu;
            if !is_branch {
                menu = menu
                    .icon_entry("Open", IconName::File, cmd(TreeCommand::Open))
                    .icon_entry(
                        "Open to the Side",
                        IconName::Columns,
                        cmd(TreeCommand::OpenToSide),
                    )
                    .separator();
            }
            let abs2 = abs.clone();
            let rel2 = rel.clone();
            menu = menu
                .icon_entry(
                    crate::os::REVEAL_LABEL,
                    IconName::FolderOpen,
                    cmd(TreeCommand::RevealInOs),
                )
                .separator()
                .icon_entry("Copy Path", IconName::Link, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(abs2.clone()))
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(rel2.clone()))
                });
            if is_branch {
                menu = menu
                    .separator()
                    .icon_entry(
                        "Find in Folder…",
                        IconName::Search,
                        cmd(TreeCommand::FindInFolder),
                    )
                    .icon_entry(
                        "Collapse All",
                        IconName::ChevronsDownUp,
                        cmd(TreeCommand::CollapseAll),
                    );
            }
            menu
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_sorts_dirs_first() {
        let paths: Vec<Arc<str>> = [
            "b.txt",
            "A/z.rs",
            "a/y.rs",
            "src/lib.rs",
            "src/x/mod.rs",
            "Cargo.toml",
        ]
        .into_iter()
        .map(Arc::from)
        .collect();
        let index = DirIndex::build(3, &paths, |p| p == "b.txt");
        let root: Vec<_> = index
            .children("")
            .iter()
            .map(|e| (&*e.name, e.is_dir))
            .collect();
        assert_eq!(
            root,
            vec![
                ("A", true),
                ("a", true),
                ("src", true),
                ("b.txt", false),
                ("Cargo.toml", false)
            ]
        );
        let src: Vec<_> = index.children("src").iter().map(|e| &*e.name).collect();
        assert_eq!(src, vec!["x", "lib.rs"]);
        assert!(index.children("nope").is_empty());
        assert!(index
            .children("")
            .iter()
            .any(|e| e.ignored && &*e.name == "b.txt"));
        assert_eq!(index.version, 3);
    }
}
