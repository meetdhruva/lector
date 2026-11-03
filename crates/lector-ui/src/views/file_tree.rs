//! Demo file tree delegate reading the file system lazily with
//! `std::fs::read_dir`. Integration replaces it with the project model
//! (git status colors, ignore rules, watcher updates).

use std::path::{Path, PathBuf};

use gpui::{App, ClipboardItem, Entity, SharedString, Window};

use crate::actions::workspace::RevealActiveFile;
use crate::components::context_menu::ContextMenu;
use crate::components::tree_view::{TreeDelegate, TreeIcon, TreeNode};
use crate::icons::IconName;

/// Names hidden from the tree.
const HIDDEN: &[&str] = &[".git", ".DS_Store"];

/// A [`TreeDelegate`] over a directory on disk.
pub struct FsTreeDelegate {
    root: PathBuf,
    root_name: SharedString,
}

impl FsTreeDelegate {
    /// Tree rooted at `root`.
    pub fn new(root: PathBuf) -> Self {
        let root_name = root
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| root.display().to_string())
            .into();
        FsTreeDelegate { root, root_name }
    }

    /// The root directory.
    pub fn root(&self) -> &Path {
        &self.root
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

/// Lists a directory: folders first, then files, case-insensitively sorted.
pub fn list_dir(dir: &Path) -> Vec<(PathBuf, bool)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<(PathBuf, bool, String)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            if HIDDEN.contains(&name.as_str()) {
                return None;
            }
            let path = e.path();
            // Follow symlinks for the dir check, but never fail on them.
            let is_dir = std::fs::metadata(&path)
                .map(|m| m.is_dir())
                .unwrap_or(false);
            Some((path, is_dir, name.to_lowercase()))
        })
        .collect();
    items.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.2.cmp(&b.2)));
    items.into_iter().map(|(p, d, _)| (p, d)).collect()
}

impl TreeDelegate for FsTreeDelegate {
    type Id = PathBuf;

    fn children(&mut self, parent: Option<&PathBuf>, _cx: &mut App) -> Vec<TreeNode<PathBuf>> {
        let Some(parent) = parent else {
            return vec![TreeNode::branch(
                self.root.clone(),
                self.root_name.clone(),
                TreeIcon::RootFolder,
            )];
        };
        list_dir(parent)
            .into_iter()
            .map(|(path, is_dir)| {
                let name: SharedString = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default()
                    .into();
                if is_dir {
                    TreeNode::branch(path, name.clone(), TreeIcon::Folder(name))
                } else {
                    TreeNode::leaf(path, name.clone(), TreeIcon::File(name))
                }
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
        let rel = node
            .id
            .strip_prefix(&self.root)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| abs.clone());
        let is_branch = node.is_branch;
        Some(ContextMenu::build(window, cx, move |menu, _, _| {
            let abs2 = abs.clone();
            let rel2 = rel.clone();
            let menu = menu
                .icon_entry("Copy Path", IconName::Link, move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(abs2.clone()))
                })
                .entry("Copy Relative Path", move |_, cx| {
                    cx.write_to_clipboard(ClipboardItem::new_string(rel2.clone()))
                });
            if is_branch {
                menu
            } else {
                menu.separator()
                    .action("Reveal Active File", Box::new(RevealActiveFile))
            }
        }))
    }
}
