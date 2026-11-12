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
            // v0.7 update notice and local crash log (handled by lector-ui-agent).
            /// Check GitHub for a newer Lector now and show the answer (Help menu).
            CheckForUpdates,
            /// Turn the once-a-day automatic update check on or off.
            ToggleAutoUpdateCheck,
            /// Show the local crash log and session health (nothing is sent anywhere).
            ShowCrashLog,
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
            // v0.7: completion and signature help.
            /// Ask the language server for completions at the caret (Ctrl+Space).
            TriggerSuggest,
            /// Ask the language server for the signature of the call at the caret.
            TriggerParameterHints,
        ]
    );
}

/// `completion::*` actions: only bound while the completion popup is open (key context
/// `CodeView && completion`), all rebindable. Handled by `editor::completion`.
pub mod completion {
    use gpui::actions;
    actions!(
        completion,
        [
            /// Select the next suggestion (wraps).
            SelectNext,
            /// Select the previous suggestion (wraps).
            SelectPrev,
            /// Select the suggestion one page further down.
            PageDown,
            /// Select the suggestion one page further up.
            PageUp,
            /// Accept the selected suggestion (Tab).
            Accept,
            /// Accept the selected suggestion, or close the list and start a new line when
            /// `editor.completion.accept_on_enter` is off (Enter).
            AcceptOnEnter,
            /// Close the popup (and nothing else).
            Dismiss,
        ]
    );
}

/// `signature::*` actions: bound while the signature help popup is open
/// (key context `CodeView && signature_help`). Handled by `editor::signature_help`.
pub mod signature {
    use gpui::actions;
    actions!(
        signature,
        [
            /// Show the next overload.
            NextSignature,
            /// Show the previous overload.
            PrevSignature,
            /// Close the popup (and nothing else).
            Dismiss,
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
            // v0.7: sync with the remote, branches and stash (handled by lector-ui-git).
            /// Run the obvious next sync step (push, pull, publish or fetch).
            Sync,
            /// Fetch every remote.
            Fetch,
            /// Pull, fast-forward only (never a merge commit).
            Pull,
            /// Pull with rebase.
            PullRebase,
            /// Push the current branch.
            Push,
            /// Push with `--force-with-lease`, after a confirmation.
            ForcePushWithLease,
            /// Push the branch and set its upstream.
            PublishBranch,
            /// Choose the upstream of the current branch.
            SetUpstream,
            /// Cancel the running fetch / pull / push.
            CancelSync,
            /// Show the sync menu (status bar dropdown).
            ShowSyncMenu,
            /// Create a branch (name, start point, switch to it).
            NewBranch,
            /// Switch to another branch.
            SwitchBranch,
            /// Rename the current branch.
            RenameBranch,
            /// Delete a local branch (undoable).
            DeleteBranch,
            /// Check out a remote branch as a local tracking branch.
            CheckoutRemoteBranch,
            /// Stash local changes (message, include untracked).
            StashChanges,
            /// Apply a stash, keeping it.
            ApplyStash,
            /// Apply a stash and drop it.
            PopStash,
            /// Drop a stash (undoable).
            DropStash,
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
            // v0.7 inline agent edit (handled by lector-ui-agent).
            /// Ask the agent to change the selection (or the current line) in place.
            EditWithAgent,
            /// Keep the inline edit hunk under the cursor.
            AcceptInlineHunk,
            /// Undo the inline edit hunk under the cursor (restores the exact original text).
            RejectInlineHunk,
            /// Keep every pending hunk of the inline edit.
            AcceptAllInlineEdits,
            /// Undo every pending hunk of the inline edit.
            RejectAllInlineEdits,
        ]
    );
}

/// `terminal::*` actions (v0.7 integrated terminal; handled by `lector-ui-terminal`).
pub mod terminal {
    use gpui::actions;
    actions!(
        terminal,
        [
            /// Show or hide the Terminal panel (opens a first terminal when there is none).
            TogglePanel,
            /// Open a new terminal tab in the panel.
            NewTerminal,
            /// Open a new terminal with a profile from the settings.
            NewTerminalWithProfile,
            /// Open a terminal in the folder of the active file (or the project root).
            OpenInTerminal,
            /// Split the focused terminal (side by side).
            SplitTerminal,
            /// Split the focused terminal (one above the other).
            SplitTerminalDown,
            /// Focus the next split of the terminal tab.
            FocusNextSplit,
            /// Focus the previous split of the terminal tab.
            FocusPrevSplit,
            /// Close the focused terminal (kills its processes).
            KillTerminal,
            /// Clear the screen and scrollback of the focused terminal.
            Clear,
            /// Rename the focused terminal tab.
            Rename,
            /// Next terminal tab.
            NextTerminal,
            /// Previous terminal tab.
            PrevTerminal,
            /// Scroll to the previous command (shell integration).
            ScrollToPrevCommand,
            /// Scroll to the next command (shell integration).
            ScrollToNextCommand,
            /// Find in the focused terminal.
            Find,
            /// Next match of the terminal find bar.
            FindNext,
            /// Previous match of the terminal find bar.
            FindPrev,
            /// Close the terminal find bar.
            CloseFind,
            /// Copy the terminal selection.
            Copy,
            /// Paste into the terminal.
            Paste,
            /// Select all terminal text (screen and scrollback).
            SelectAll,
            /// Move the focused terminal into the editor area as a tab.
            MoveToEditor,
            /// Move a terminal tab from the editor area back into the panel.
            MoveToPanel,
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
pub const COMMAND_NAMESPACES: &[&str] =
    &["workspace", "editor", "diff", "git", "terminal", "agent"];

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
