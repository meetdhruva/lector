//! Every user command is a GPUI action (see `docs/ARCHITECTURE.md`, "Actions").
//! Keybindings, the command palette, menus and context menus all dispatch
//! these same actions.

#![allow(missing_docs)]

/// `workspace::*` actions.
pub mod workspace {
    use gpui::actions;
    actions!(
        workspace,
        [
            OpenFolder,
            CloseWindow,
            Quit,
            ToggleCommandPalette,
            ToggleFileFinder,
            ToggleSymbolFinder,
            ToggleOutline,
            ToggleProjectSearch,
            ToggleSidebar,
            ShowFiles,
            ShowChanges,
            ShowHistory,
            ShowBranches,
            SplitRight,
            SplitDown,
            CloseTab,
            ReopenClosedTab,
            NextTab,
            PrevTab,
            GoBack,
            GoForward,
            RevealActiveFile,
            OpenSettings,
            OpenKeymap,
            ReloadSettings,
            ZoomIn,
            ZoomOut,
            ZoomReset,
            ToggleAppearance,
            SelectTheme,
            // Lector-UI additions (documented in ARCHITECTURE.md).
            CloseOtherTabs,
            CloseTabsToRight,
            ToggleColorblindDiff,
            About,
            ShowOutline,
        ]
    );
}

/// `editor::*` actions (code view).
pub mod editor {
    use gpui::actions;
    actions!(
        editor,
        [
            GoToLine,
            Find,
            FindNext,
            FindPrev,
            GoToDefinition,
            FindReferences,
            Hover,
            ToggleFold,
            FoldAll,
            UnfoldAll,
            Copy,
            SelectAll,
            CopyPath,
            CopyRelativePath,
            CopyPermalink,
            ToggleBlame,
            ToggleSoftWrap,
            ToggleMinimap,
            ToggleMarkdownPreview,
            ShowFileHistory,
            ShowLineHistory,
            NextChange,
            PrevChange,
            MoveUp,
            MoveDown,
            MoveLeft,
            MoveRight,
            PageUp,
            PageDown,
            ScrollToTop,
            ScrollToBottom,
            SelectUp,
            SelectDown,
            SelectLeft,
            SelectRight,
            // Lector-UI additions (documented in ARCHITECTURE.md).
            MoveToLineStart,
            MoveToLineEnd,
            MoveWordLeft,
            MoveWordRight,
            SelectToLineStart,
            SelectToLineEnd,
            SelectWordLeft,
            SelectWordRight,
            SelectPageUp,
            SelectPageDown,
            SelectToTop,
            SelectToBottom,
            ClearSelection,
        ]
    );
}

/// `diff::*` actions.
pub mod diff {
    use gpui::actions;
    actions!(
        diff,
        [
            ToggleLayout,
            NextHunk,
            PrevHunk,
            NextFile,
            PrevFile,
            ToggleViewed,
            ToggleIgnoreWhitespace,
            ExpandAllContext,
            OpenInEditor,
        ]
    );
}

/// `picker::*` actions (also used by menus and lists).
pub mod picker {
    use gpui::actions;
    actions!(picker, [Confirm, Cancel, SelectNext, SelectPrev]);
}

/// `tree::*` actions (tree views).
pub mod tree {
    use gpui::actions;
    actions!(tree, [ExpandSelected, CollapseSelected]);
}

/// `text_input::*` actions (single-line inputs).
pub mod text_input {
    use gpui::actions;
    actions!(
        text_input,
        [
            Backspace,
            Delete,
            Left,
            Right,
            SelectLeft,
            SelectRight,
            SelectAll,
            Home,
            End,
            Paste,
            Cut,
            Copy,
            ShowCharacterPalette,
        ]
    );
}

/// Namespaces whose actions are user commands (shown in the palette).
pub const COMMAND_NAMESPACES: &[&str] = &["workspace", "editor", "diff"];

/// Actions hidden from the command palette (motion primitives etc.).
pub fn is_palette_hidden(name: &str) -> bool {
    let Some((ns, action)) = name.split_once("::") else {
        return true;
    };
    if !COMMAND_NAMESPACES.contains(&ns) {
        return true;
    }
    ns == "editor"
        && (action.starts_with("Move")
            || action.starts_with("Select") && action != "SelectAll"
            || matches!(action, "PageUp" | "PageDown" | "ClearSelection"))
}

/// Human-readable label for an action name: `"workspace::ToggleSidebar"` →
/// `"Workspace: Toggle Sidebar"`.
pub fn humanize_action_name(name: &str) -> String {
    let (ns, action) = name.split_once("::").unwrap_or(("", name));
    let mut out = String::new();
    for (i, word) in ns.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = word.chars();
        if let Some(c) = chars.next() {
            out.extend(c.to_uppercase());
            out.push_str(chars.as_str());
        }
    }
    if !out.is_empty() {
        out.push_str(": ");
    }
    let chars: Vec<char> = action.chars().collect();
    for (i, &c) in chars.iter().enumerate() {
        let prev_lower = i > 0 && chars[i - 1].is_lowercase();
        let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
        let prev_upper = i > 0 && chars[i - 1].is_uppercase();
        if i > 0 && c.is_uppercase() && (prev_lower || (prev_upper && next_lower)) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize() {
        assert_eq!(
            humanize_action_name("workspace::ToggleSidebar"),
            "Workspace: Toggle Sidebar"
        );
        assert_eq!(
            humanize_action_name("editor::GoToLine"),
            "Editor: Go To Line"
        );
        assert_eq!(humanize_action_name("text_input::Copy"), "Text Input: Copy");
    }

    #[test]
    fn palette_filter() {
        assert!(is_palette_hidden("editor::MoveUp"));
        assert!(is_palette_hidden("editor::SelectLeft"));
        assert!(!is_palette_hidden("editor::SelectAll"));
        assert!(!is_palette_hidden("workspace::ToggleSidebar"));
        assert!(is_palette_hidden("picker::Confirm"));
    }
}
