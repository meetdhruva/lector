//! Native application menu bar (File, View, Go, Git, Window, Help).

use gpui::{Menu, MenuItem, SystemMenuType};

use crate::actions::{diff, editor, workspace};

/// The application menus; every item dispatches an action.
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Lector".into(),
            items: vec![
                MenuItem::action("About Lector", workspace::About),
                MenuItem::separator(),
                MenuItem::action("Settings", workspace::OpenSettings),
                MenuItem::action("Key Bindings", workspace::OpenKeymap),
                MenuItem::action("Select Theme…", workspace::SelectTheme),
                MenuItem::separator(),
                MenuItem::os_submenu("Services", SystemMenuType::Services),
                MenuItem::separator(),
                MenuItem::action("Quit Lector", workspace::Quit),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("Open Folder…", workspace::OpenFolder),
                MenuItem::action("Go to File…", workspace::ToggleFileFinder),
                MenuItem::separator(),
                MenuItem::action("Reopen Closed Tab", workspace::ReopenClosedTab),
                MenuItem::action("Close Tab", workspace::CloseTab),
                MenuItem::action("Close Window", workspace::CloseWindow),
            ],
        },
        Menu {
            name: "View".into(),
            items: vec![
                MenuItem::action("Command Palette…", workspace::ToggleCommandPalette),
                MenuItem::separator(),
                MenuItem::action("Toggle Sidebar", workspace::ToggleSidebar),
                MenuItem::action("Files", workspace::ShowFiles),
                MenuItem::action("Changes", workspace::ShowChanges),
                MenuItem::action("History", workspace::ShowHistory),
                MenuItem::action("Branches", workspace::ShowBranches),
                MenuItem::action("Outline", workspace::ToggleOutline),
                MenuItem::action("Search", workspace::ToggleProjectSearch),
                MenuItem::separator(),
                MenuItem::action("Split Right", workspace::SplitRight),
                MenuItem::action("Split Down", workspace::SplitDown),
                MenuItem::separator(),
                MenuItem::action("Soft Wrap", editor::ToggleSoftWrap),
                MenuItem::action("Minimap", editor::ToggleMinimap),
                MenuItem::action("Blame", editor::ToggleBlame),
                MenuItem::action("Markdown Preview", editor::ToggleMarkdownPreview),
                MenuItem::separator(),
                MenuItem::action("Zoom In", workspace::ZoomIn),
                MenuItem::action("Zoom Out", workspace::ZoomOut),
                MenuItem::action("Reset Zoom", workspace::ZoomReset),
                MenuItem::action("Toggle Light/Dark", workspace::ToggleAppearance),
                MenuItem::action(
                    "Colorblind-Safe Diff Colors",
                    workspace::ToggleColorblindDiff,
                ),
            ],
        },
        Menu {
            name: "Go".into(),
            items: vec![
                MenuItem::action("Back", workspace::GoBack),
                MenuItem::action("Forward", workspace::GoForward),
                MenuItem::separator(),
                MenuItem::action("Go to File…", workspace::ToggleFileFinder),
                MenuItem::action("Go to Symbol in Project…", workspace::ToggleSymbolFinder),
                MenuItem::action("Go to Line…", editor::GoToLine),
                MenuItem::separator(),
                MenuItem::action("Go to Definition", editor::GoToDefinition),
                MenuItem::action("Find References", editor::FindReferences),
                MenuItem::separator(),
                MenuItem::action("Next Change", editor::NextChange),
                MenuItem::action("Previous Change", editor::PrevChange),
                MenuItem::action("Reveal Active File", workspace::RevealActiveFile),
            ],
        },
        Menu {
            name: "Git".into(),
            items: vec![
                MenuItem::action("Show Changes", workspace::ShowChanges),
                MenuItem::action("Show History", workspace::ShowHistory),
                MenuItem::action("Show Branches", workspace::ShowBranches),
                MenuItem::separator(),
                MenuItem::action("File History", editor::ShowFileHistory),
                MenuItem::action("Line History", editor::ShowLineHistory),
                MenuItem::action("Toggle Blame", editor::ToggleBlame),
                MenuItem::action("Copy Permalink", editor::CopyPermalink),
                MenuItem::separator(),
                MenuItem::action("Next Hunk", diff::NextHunk),
                MenuItem::action("Previous Hunk", diff::PrevHunk),
                MenuItem::action("Toggle Split/Unified Diff", diff::ToggleLayout),
                MenuItem::action("Ignore Whitespace", diff::ToggleIgnoreWhitespace),
            ],
        },
        Menu {
            name: "Window".into(),
            items: vec![
                MenuItem::action("Next Tab", workspace::NextTab),
                MenuItem::action("Previous Tab", workspace::PrevTab),
                MenuItem::separator(),
                MenuItem::action("Close Window", workspace::CloseWindow),
            ],
        },
        Menu {
            name: "Help".into(),
            items: vec![
                MenuItem::action("Command Palette", workspace::ToggleCommandPalette),
                MenuItem::action("Key Bindings", workspace::OpenKeymap),
                MenuItem::action("About Lector", workspace::About),
            ],
        },
    ]
}
