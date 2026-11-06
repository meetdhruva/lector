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
            // v0.2: ease of access (menus, welcome screen, window controls).
            OpenFile,
            OpenRecent,
            NewWindow,
            CloseFolder,
            CloseAllTabs,
            ShowWelcome,
            ShowKeybindings,
            OpenLogFile,
            OpenDocumentation,
            ReportIssue,
            Minimize,
            ToggleMaximize,
            ToggleFullScreen,
            ShowAppMenu,
            // v0.3: editing.
            SaveAll,
            ShowProblems,
            // v0.4: undo instead of confirm.
            ShowActivity,
            UndoLastOperation,
            // v0.4 accessibility: keyboard-only operation, interface zoom, motion.
            FocusNextRegion,
            FocusPrevRegion,
            FocusNextControl,
            FocusPrevControl,
            ShowContextMenu,
            UiZoomIn,
            UiZoomOut,
            UiZoomReset,
            ToggleReduceMotion,
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
            FindReplace,
            ReplaceNext,
            ReplaceAll,
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
            // v0.3: editing.
            Undo,
            Redo,
            Cut,
            Paste,
            Save,
            SaveAs,
            RevertFile,
            ToggleReadOnly,
            Newline,
            NewlineBelow,
            NewlineAbove,
            Backspace,
            Delete,
            DeleteWordLeft,
            DeleteWordRight,
            DeleteToLineStart,
            DeleteLine,
            Tab,
            Indent,
            Outdent,
            ToggleComment,
            DuplicateLine,
            MoveLineUp,
            MoveLineDown,
            SelectLine,
            SelectNextOccurrence,
            SelectAllOccurrences,
            AddCursorAbove,
            AddCursorBelow,
            SplitSelectionIntoLines,
            Format,
            CodeActions,
            Rename,
            NextDiagnostic,
            PrevDiagnostic,
        ]
    );
}

/// `git::*` actions (v0.2+: staging, committing, discarding, conflicts).
pub mod git {
    use gpui::actions;
    actions!(
        git,
        [
            Stage,
            Unstage,
            StageAll,
            UnstageAll,
            DiscardChanges,
            DiscardAll,
            StageHunk,
            UnstageHunk,
            DiscardHunk,
            StageSelectedLines,
            UnstageSelectedLines,
            Commit,
            ToggleAmend,
            FocusCommitMessage,
            Refresh,
            OpenMergeEditor,
            AcceptOurs,
            AcceptTheirs,
            AcceptBoth,
            NextConflict,
            PrevConflict,
            MarkResolved,
            AcceptAllOurs,
            AcceptAllTheirs,
            ContinueOperation,
            AbortOperation,
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
            // v0.4 review UX.
            ToggleMovedCode,
            ToggleStructural,
        ]
    );
}

/// `review::*` actions (v0.4 review queue, read-only contexts).
pub mod review {
    use gpui::actions;
    actions!(
        review,
        [
            /// Jump to the next unreviewed hunk across files (risk order).
            NextUnreviewed,
            /// Mark the hunk at the cursor reviewed / unreviewed.
            ToggleHunkReviewed,
            /// Comment on the line or selection at the cursor (v0.6).
            AddComment,
            /// Jump to the next comment thread.
            NextComment,
            /// Jump to the previous comment thread.
            PrevComment,
            /// Show the Pull Requests panel.
            ShowPullRequests,
            /// Submit the pending pull-request review (asks first).
            SubmitReview,
            /// Explain how to give Lector a GitHub / GitLab token.
            SignIn,
        ]
    );
}

/// `agent::*` actions (v0.5 agent sessions; handled by `lector-ui-agent`).
pub mod agent {
    use gpui::actions;
    actions!(
        agent,
        [
            /// Show or hide the Agent panel.
            TogglePanel,
            /// Start a new agent session (pick the agent).
            NewSession,
            /// Stop the agent's current turn.
            Cancel,
            /// Open the last turn's changes in a review tab.
            ReviewLastTurn,
            /// Restore the files the last turn changed (undoable).
            UndoLastTurn,
            /// Send the selection (and open review comments) to the agent as feedback.
            SendFeedback,
            /// Send the prompt typed in the Agent panel.
            SendPrompt,
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
pub const COMMAND_NAMESPACES: &[&str] = &["workspace", "editor", "diff", "git"];

/// Actions hidden from the command palette (motion primitives etc.).
pub fn is_palette_hidden(name: &str) -> bool {
    let Some((ns, action)) = name.split_once("::") else {
        return true;
    };
    if !COMMAND_NAMESPACES.contains(&ns) {
        return true;
    }
    ns == "editor"
        && (action.starts_with("Move") && !action.starts_with("MoveLine")
            || action.starts_with("Select")
                && !matches!(
                    action,
                    "SelectAll" | "SelectLine" | "SelectNextOccurrence" | "SelectAllOccurrences"
                )
            || matches!(
                action,
                "PageUp"
                    | "PageDown"
                    | "ClearSelection"
                    | "Newline"
                    | "Backspace"
                    | "Delete"
                    | "Tab"
                    | "DeleteToLineStart"
            ))
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
    // "Ui Zoom In" reads as a typo; "UI" is the word.
    out.replace(": Ui ", ": UI ")
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
