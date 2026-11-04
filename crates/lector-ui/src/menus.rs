//! The application menu model: one description of File, Edit, Selection, View,
//! Go, Git, Window and Help, rendered twice:
//!
//! * natively on macOS through [`app_menus`] → `cx.set_menus`, and
//! * in the window's title bar on Linux and Windows by
//!   [`crate::views::app_menu::AppMenuBar`] (dropdowns with live key bindings,
//!   submenus, keyboard navigation; unavailable actions are greyed out).
//!
//! Items name their action as a string (`"workspace::OpenFolder"`) so the menu can
//! list actions declared by extension crates (e.g. `diff::CompareWith` from
//! `lector-ui-git`) without depending on them; names that are not registered in
//! the running binary are skipped. To add an item, add a line to [`menu_model`];
//! the `every_palette_action_is_in_a_menu` test fails when a palette command is
//! reachable from no menu (see [`MENU_EXEMPT_ACTIONS`]).

use gpui::{Action, App, Menu, MenuItem, OsAction, SharedString, SystemMenuType};

/// One entry of a menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MenuEntrySpec {
    /// Dispatches the action named `action` (`namespace::Name`).
    Action {
        /// Label shown in the menu.
        label: &'static str,
        /// Registered action name.
        action: &'static str,
    },
    /// A horizontal rule.
    Separator,
    /// A nested menu.
    Submenu {
        /// Label of the submenu item.
        label: &'static str,
        /// Its entries.
        entries: Vec<MenuEntrySpec>,
    },
    /// The recently opened projects (filled at render time), followed by
    /// "More…" (`workspace::OpenRecent`).
    RecentProjects,
}

/// A top-level menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MenuSpec {
    /// Title in the menu bar.
    pub name: &'static str,
    /// Entries.
    pub entries: Vec<MenuEntrySpec>,
}

/// Palette commands deliberately absent from every menu, with the reason.
pub const MENU_EXEMPT_ACTIONS: &[(&str, &str)] = &[
    (
        "workspace::ShowAppMenu",
        "opens this menu bar itself (alt-f)",
    ),
    (
        "editor::DeleteWordLeft",
        "a typing primitive (ctrl/alt-backspace); no menu editor lists it",
    ),
    (
        "editor::DeleteWordRight",
        "a typing primitive (ctrl/alt-delete); no menu editor lists it",
    ),
];

/// Actions provided by extension crates (not linked into `lector-ui` tests).
pub const EXTERNAL_MENU_ACTIONS: &[&str] = &[
    "diff::CompareWith",
    "diff::ReviewWorkingTree",
    "diff::ReviewBranch",
];

fn a(label: &'static str, action: &'static str) -> MenuEntrySpec {
    MenuEntrySpec::Action { label, action }
}

fn sep() -> MenuEntrySpec {
    MenuEntrySpec::Separator
}

fn sub(label: &'static str, entries: Vec<MenuEntrySpec>) -> MenuEntrySpec {
    MenuEntrySpec::Submenu { label, entries }
}

/// The menu bar for Linux and Windows (in-window). macOS moves About,
/// Settings and Quit into the application menu (see [`app_menus`]).
pub fn menu_model() -> Vec<MenuSpec> {
    vec![
        MenuSpec {
            name: "File",
            entries: vec![
                a("New Window", "workspace::NewWindow"),
                sep(),
                a("Open File…", "workspace::OpenFile"),
                a("Open Folder…", "workspace::OpenFolder"),
                sub("Open Recent", vec![MenuEntrySpec::RecentProjects]),
                sep(),
                a("Save", "editor::Save"),
                a("Save As…", "editor::SaveAs"),
                a("Save All", "workspace::SaveAll"),
                a("Revert File", "editor::RevertFile"),
                sep(),
                a("Close Tab", "workspace::CloseTab"),
                a("Close All Tabs", "workspace::CloseAllTabs"),
                a("Reopen Closed Tab", "workspace::ReopenClosedTab"),
                a("Close Folder", "workspace::CloseFolder"),
                a("Close Window", "workspace::CloseWindow"),
                sep(),
                sub(
                    "Preferences",
                    vec![
                        a("Settings", "workspace::OpenSettings"),
                        a("Keyboard Shortcuts", "workspace::ShowKeybindings"),
                        a("Key Bindings File", "workspace::OpenKeymap"),
                        a("Reload Settings", "workspace::ReloadSettings"),
                        sep(),
                        a("Select Theme…", "workspace::SelectTheme"),
                    ],
                ),
                sep(),
                a("Quit", "workspace::Quit"),
            ],
        },
        MenuSpec {
            name: "Edit",
            entries: vec![
                a("Undo", "editor::Undo"),
                a("Redo", "editor::Redo"),
                sep(),
                a("Cut", "editor::Cut"),
                a("Copy", "editor::Copy"),
                a("Paste", "editor::Paste"),
                sep(),
                a("Find", "editor::Find"),
                a("Find Next", "editor::FindNext"),
                a("Find Previous", "editor::FindPrev"),
                a("Replace…", "editor::FindReplace"),
                a("Replace Next", "editor::ReplaceNext"),
                a("Replace All", "editor::ReplaceAll"),
                a("Find in Project…", "workspace::ToggleProjectSearch"),
                sep(),
                a("Toggle Comment", "editor::ToggleComment"),
                a("Format Document", "editor::Format"),
                a("Rename Symbol…", "editor::Rename"),
                a("Code Actions…", "editor::CodeActions"),
                sep(),
                a("Copy Path", "editor::CopyPath"),
                a("Copy Relative Path", "editor::CopyRelativePath"),
                a("Copy Permalink", "editor::CopyPermalink"),
                sep(),
                a("Toggle Read-Only", "editor::ToggleReadOnly"),
            ],
        },
        MenuSpec {
            name: "Selection",
            entries: vec![
                a("Select All", "editor::SelectAll"),
                a("Select Line", "editor::SelectLine"),
                sep(),
                a("Add Cursor Above", "editor::AddCursorAbove"),
                a("Add Cursor Below", "editor::AddCursorBelow"),
                a(
                    "Split Selection into Lines",
                    "editor::SplitSelectionIntoLines",
                ),
                a("Select Next Occurrence", "editor::SelectNextOccurrence"),
                a("Select All Occurrences", "editor::SelectAllOccurrences"),
                sep(),
                a("Move Line Up", "editor::MoveLineUp"),
                a("Move Line Down", "editor::MoveLineDown"),
                a("Duplicate Line", "editor::DuplicateLine"),
                a("Delete Line", "editor::DeleteLine"),
                a("Insert Line Above", "editor::NewlineAbove"),
                a("Insert Line Below", "editor::NewlineBelow"),
                sep(),
                a("Indent", "editor::Indent"),
                a("Outdent", "editor::Outdent"),
            ],
        },
        MenuSpec {
            name: "View",
            entries: vec![
                a("Command Palette…", "workspace::ToggleCommandPalette"),
                sep(),
                a("Files", "workspace::ShowFiles"),
                a("Search", "workspace::ToggleProjectSearch"),
                a("Outline", "workspace::ShowOutline"),
                a("Problems", "workspace::ShowProblems"),
                a("Changes", "workspace::ShowChanges"),
                a("History", "workspace::ShowHistory"),
                a("Branches", "workspace::ShowBranches"),
                sep(),
                a("Toggle Sidebar", "workspace::ToggleSidebar"),
                a("Split Right", "workspace::SplitRight"),
                a("Split Down", "workspace::SplitDown"),
                sep(),
                a("Soft Wrap", "editor::ToggleSoftWrap"),
                a("Minimap", "editor::ToggleMinimap"),
                a("Blame", "editor::ToggleBlame"),
                a("Markdown Preview", "editor::ToggleMarkdownPreview"),
                sub(
                    "Folding",
                    vec![
                        a("Toggle Fold", "editor::ToggleFold"),
                        a("Fold All", "editor::FoldAll"),
                        a("Unfold All", "editor::UnfoldAll"),
                    ],
                ),
                sep(),
                a("Zoom In", "workspace::ZoomIn"),
                a("Zoom Out", "workspace::ZoomOut"),
                a("Reset Zoom", "workspace::ZoomReset"),
                sep(),
                sub(
                    "Theme",
                    vec![
                        a("Select Theme…", "workspace::SelectTheme"),
                        a("Toggle Light/Dark", "workspace::ToggleAppearance"),
                        a(
                            "Colorblind-Safe Diff Colors",
                            "workspace::ToggleColorblindDiff",
                        ),
                    ],
                ),
                a("Toggle Full Screen", "workspace::ToggleFullScreen"),
            ],
        },
        MenuSpec {
            name: "Go",
            entries: vec![
                a("Back", "workspace::GoBack"),
                a("Forward", "workspace::GoForward"),
                sep(),
                a("Go to File…", "workspace::ToggleFileFinder"),
                a("Go to Symbol in File…", "workspace::ToggleOutline"),
                a("Go to Symbol in Project…", "workspace::ToggleSymbolFinder"),
                a("Go to Line…", "editor::GoToLine"),
                sep(),
                a("Go to Definition", "editor::GoToDefinition"),
                a("Find References", "editor::FindReferences"),
                a("Show Hover", "editor::Hover"),
                sep(),
                a("Next Change", "editor::NextChange"),
                a("Previous Change", "editor::PrevChange"),
                a("Next Problem", "editor::NextDiagnostic"),
                a("Previous Problem", "editor::PrevDiagnostic"),
                a("Top of File", "editor::ScrollToTop"),
                a("Bottom of File", "editor::ScrollToBottom"),
                sep(),
                a("Next Tab", "workspace::NextTab"),
                a("Previous Tab", "workspace::PrevTab"),
                a("Reveal Active File", "workspace::RevealActiveFile"),
            ],
        },
        MenuSpec {
            name: "Git",
            entries: vec![
                a("Changes", "workspace::ShowChanges"),
                a("History", "workspace::ShowHistory"),
                a("Branches", "workspace::ShowBranches"),
                a("Refresh", "git::Refresh"),
                sep(),
                a("Stage File", "git::Stage"),
                a("Unstage File", "git::Unstage"),
                a("Stage All", "git::StageAll"),
                a("Unstage All", "git::UnstageAll"),
                a("Discard Changes…", "git::DiscardChanges"),
                a("Discard All Changes…", "git::DiscardAll"),
                sub(
                    "Hunks and Lines",
                    vec![
                        a("Stage Hunk", "git::StageHunk"),
                        a("Unstage Hunk", "git::UnstageHunk"),
                        a("Discard Hunk…", "git::DiscardHunk"),
                        sep(),
                        a("Stage Selected Lines", "git::StageSelectedLines"),
                        a("Unstage Selected Lines", "git::UnstageSelectedLines"),
                    ],
                ),
                sep(),
                a("Commit…", "git::Commit"),
                a("Edit Commit Message", "git::FocusCommitMessage"),
                a("Amend Last Commit", "git::ToggleAmend"),
                sep(),
                sub(
                    "Review",
                    vec![
                        a("Review Working Tree", "diff::ReviewWorkingTree"),
                        a("Review Branch", "diff::ReviewBranch"),
                        a("Compare File With…", "diff::CompareWith"),
                        sep(),
                        a("Next Hunk", "diff::NextHunk"),
                        a("Previous Hunk", "diff::PrevHunk"),
                        a("Next File", "diff::NextFile"),
                        a("Previous File", "diff::PrevFile"),
                        a("Mark File as Viewed", "diff::ToggleViewed"),
                        sep(),
                        a("Toggle Split/Unified Diff", "diff::ToggleLayout"),
                        a("Ignore Whitespace", "diff::ToggleIgnoreWhitespace"),
                        a("Expand All Context", "diff::ExpandAllContext"),
                        a("Open File in Editor", "diff::OpenInEditor"),
                    ],
                ),
                sub(
                    "Merge Conflicts",
                    vec![
                        a("Open Merge Editor", "git::OpenMergeEditor"),
                        a("Next Conflict", "git::NextConflict"),
                        a("Previous Conflict", "git::PrevConflict"),
                        sep(),
                        a("Accept Ours", "git::AcceptOurs"),
                        a("Accept Theirs", "git::AcceptTheirs"),
                        a("Accept Both", "git::AcceptBoth"),
                        a("Accept All Current…", "git::AcceptAllOurs"),
                        a("Accept All Incoming…", "git::AcceptAllTheirs"),
                        a("Mark as Resolved", "git::MarkResolved"),
                        sep(),
                        a("Continue Merge/Rebase", "git::ContinueOperation"),
                        a("Abort…", "git::AbortOperation"),
                    ],
                ),
                sep(),
                a("File History", "editor::ShowFileHistory"),
                a("Line History", "editor::ShowLineHistory"),
                a("Toggle Blame", "editor::ToggleBlame"),
                a("Copy Permalink", "editor::CopyPermalink"),
            ],
        },
        MenuSpec {
            name: "Window",
            entries: vec![
                a("Minimize", "workspace::Minimize"),
                a("Maximize / Restore", "workspace::ToggleMaximize"),
                a("Toggle Full Screen", "workspace::ToggleFullScreen"),
                sep(),
                a("New Window", "workspace::NewWindow"),
                sep(),
                a("Next Tab", "workspace::NextTab"),
                a("Previous Tab", "workspace::PrevTab"),
                a("Close Other Tabs", "workspace::CloseOtherTabs"),
                a("Close Tabs to the Right", "workspace::CloseTabsToRight"),
                a("Close All Tabs", "workspace::CloseAllTabs"),
                sep(),
                a("Close Window", "workspace::CloseWindow"),
            ],
        },
        MenuSpec {
            name: "Help",
            entries: vec![
                a("Welcome", "workspace::ShowWelcome"),
                a("Command Palette…", "workspace::ToggleCommandPalette"),
                a("Keyboard Shortcuts", "workspace::ShowKeybindings"),
                a("Documentation", "workspace::OpenDocumentation"),
                sep(),
                a("Show Log File", "workspace::OpenLogFile"),
                a("Report an Issue…", "workspace::ReportIssue"),
                sep(),
                a("About Lector", "workspace::About"),
            ],
        },
    ]
}

/// Every action name referenced by `entries` (recursively), in order.
pub fn action_names(entries: &[MenuEntrySpec]) -> Vec<&'static str> {
    let mut out = Vec::new();
    for entry in entries {
        match entry {
            MenuEntrySpec::Action { action, .. } => out.push(*action),
            MenuEntrySpec::Submenu { entries, .. } => out.extend(action_names(entries)),
            MenuEntrySpec::RecentProjects => out.push("workspace::OpenRecent"),
            MenuEntrySpec::Separator => {}
        }
    }
    out
}

/// OS-level action for an item (macOS routes these to focused text fields too).
fn os_action(action: &str) -> Option<OsAction> {
    Some(match action {
        "editor::Cut" => OsAction::Cut,
        "editor::Copy" => OsAction::Copy,
        "editor::Paste" => OsAction::Paste,
        "editor::SelectAll" => OsAction::SelectAll,
        "editor::Undo" => OsAction::Undo,
        "editor::Redo" => OsAction::Redo,
        _ => return None,
    })
}

fn build(action: &str, cx: &App) -> Option<Box<dyn Action>> {
    cx.build_action(action, None).ok()
}

fn native_items(entries: &[MenuEntrySpec], skip: &[&str], cx: &App) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = Vec::new();
    for entry in entries {
        match entry {
            MenuEntrySpec::Action { label, action } => {
                if skip.contains(action) {
                    continue;
                }
                if let Some(built) = build(action, cx) {
                    items.push(MenuItem::Action {
                        name: SharedString::from(*label),
                        action: built,
                        os_action: os_action(action),
                    });
                }
            }
            MenuEntrySpec::Separator => {
                if !items.is_empty() && !matches!(items.last(), Some(MenuItem::Separator)) {
                    items.push(MenuItem::separator());
                }
            }
            MenuEntrySpec::Submenu { label, entries } => {
                let sub_items = native_items(entries, skip, cx);
                if !sub_items.is_empty() {
                    items.push(MenuItem::submenu(Menu {
                        name: SharedString::from(*label),
                        items: sub_items,
                    }));
                }
            }
            MenuEntrySpec::RecentProjects => {
                if let Some(built) = build("workspace::OpenRecent", cx) {
                    items.push(MenuItem::Action {
                        name: "Open Recent…".into(),
                        action: built,
                        os_action: None,
                    });
                }
            }
        }
    }
    while matches!(items.last(), Some(MenuItem::Separator)) {
        items.pop();
    }
    items
}

/// Native macOS menus built from [`menu_model`]: an application menu (About,
/// Settings, Services, Quit) followed by the shared menus.
pub fn app_menus(cx: &App) -> Vec<Menu> {
    // Items that live in the macOS application menu instead.
    const APP_MENU_ITEMS: &[&str] = &[
        "workspace::About",
        "workspace::Quit",
        "workspace::OpenSettings",
    ];
    let mut app_items = native_items(
        &[
            a("About Lector", "workspace::About"),
            sep(),
            a("Settings…", "workspace::OpenSettings"),
            a("Keyboard Shortcuts", "workspace::ShowKeybindings"),
            a("Select Theme…", "workspace::SelectTheme"),
            sep(),
        ],
        &[],
        cx,
    );
    app_items.push(MenuItem::separator());
    app_items.push(MenuItem::os_submenu("Services", SystemMenuType::Services));
    app_items.push(MenuItem::separator());
    app_items.extend(native_items(
        &[a("Quit Lector", "workspace::Quit")],
        &[],
        cx,
    ));
    let mut menus = vec![Menu {
        name: "Lector".into(),
        items: app_items,
    }];
    for spec in menu_model() {
        let items = native_items(&spec.entries, APP_MENU_ITEMS, cx);
        menus.push(Menu {
            name: spec.name.into(),
            items,
        });
    }
    menus
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use crate::actions::is_palette_hidden;

    #[gpui::test]
    fn every_palette_action_is_in_a_menu(cx: &mut gpui::TestAppContext) {
        let menus = menu_model();
        let in_menus: HashSet<&str> = menus
            .iter()
            .flat_map(|m| action_names(&m.entries))
            .collect();
        let exempt: HashSet<&str> = MENU_EXEMPT_ACTIONS.iter().map(|(a, _)| *a).collect();
        cx.update(|cx| {
            let registered: HashSet<&str> = cx.all_action_names().iter().copied().collect();
            let missing: Vec<&str> = cx
                .all_action_names()
                .iter()
                .copied()
                .filter(|name| !is_palette_hidden(name))
                .filter(|name| !in_menus.contains(name) && !exempt.contains(name))
                .collect();
            assert!(
                missing.is_empty(),
                "palette commands missing from the menu model: {missing:?}"
            );
            // Every menu item names a registered action (or an extension crate's).
            for name in &in_menus {
                assert!(
                    registered.contains(name) || EXTERNAL_MENU_ACTIONS.contains(name),
                    "menu references unknown action {name}"
                );
            }
            // Exemptions must stay meaningful.
            for name in &exempt {
                assert!(registered.contains(name), "stale exemption {name}");
                assert!(
                    !in_menus.contains(name),
                    "exempt action {name} is in a menu"
                );
            }
            // The macOS menus build without panicking and keep the app menu first.
            let native = app_menus(cx);
            assert_eq!(native[0].name.as_ref(), "Lector");
            assert_eq!(native.len(), menus.len() + 1);
        });
    }

    #[test]
    fn menu_titles_are_unique_and_ordered() {
        let names: Vec<&str> = menu_model().iter().map(|m| m.name).collect();
        assert_eq!(
            names,
            [
                "File",
                "Edit",
                "Selection",
                "View",
                "Go",
                "Git",
                "Window",
                "Help"
            ]
        );
    }
}
