//! Application views: the workspace shell and its modals.

pub mod about;
pub mod app_menu;
pub mod command_palette;
pub mod file_finder;
pub mod file_tree;
pub mod find_bar;
pub mod go_to_line;
pub mod keybindings;
pub mod lsp_status;
pub mod markdown_preview;
pub mod outline_panel;
pub mod pane;
pub mod recent;
pub mod references;
pub mod search_panel;
pub mod sidebar;
pub mod symbol_picker;
pub mod theme_selector;
pub mod welcome;
pub mod window_controls;
pub mod workspace;

pub use workspace::{Workspace, WorkspaceEvent, WorkspaceRoot};
