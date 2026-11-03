//! Application views: the workspace shell and its modals.

pub mod command_palette;
pub mod file_finder;
pub mod file_tree;
pub mod find_bar;
pub mod go_to_line;
pub mod pane;
pub mod sidebar;
pub mod theme_selector;
pub mod workspace;

pub use workspace::{Workspace, WorkspaceRoot};
