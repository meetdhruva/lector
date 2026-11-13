use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::paths;

/// Target platform for default keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    /// macOS (`cmd` primary modifier).
    MacOS,
    /// Linux (`ctrl`).
    Linux,
    /// Windows (`ctrl`).
    Windows,
}

impl Platform {
    /// The platform this binary runs on.
    pub fn current() -> Platform {
        if cfg!(target_os = "macos") {
            Platform::MacOS
        } else if cfg!(target_os = "windows") {
            Platform::Windows
        } else {
            Platform::Linux
        }
    }
}

/// One key binding in GPUI keystroke syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyBinding {
    /// Keystrokes, space separated for chords: `"cmd-k cmd-i"`.
    pub keys: String,
    /// Action name (`namespace::Name`); `""` or `"none"` unbinds.
    pub action: String,
    /// Context name: `Workspace`, `CodeView`, `DiffView`, `Picker`; `None` = global.
    #[serde(default)]
    pub context: Option<String>,
}

/// Built-in key bindings for `platform`.
pub fn default_keymap(platform: Platform) -> Vec<KeyBinding> {
    let mac = platform == Platform::MacOS;
    let m = if mac { "cmd" } else { "ctrl" };
    let mut out = Vec::new();
    let mut add = |ctx: &str, keys: String, action: &str| {
        out.push(KeyBinding {
            keys,
            action: action.to_string(),
            context: Some(ctx.to_string()),
        });
    };
    let w = "Workspace";
    add(w, format!("{m}-o"), "workspace::OpenFolder");
    add(w, format!("{m}-p"), "workspace::ToggleFileFinder");
    add(w, format!("{m}-shift-p"), "workspace::ToggleCommandPalette");
    add(w, format!("{m}-t"), "workspace::ToggleSymbolFinder");
    add(w, format!("{m}-shift-o"), "workspace::ToggleOutline");
    add(w, format!("{m}-shift-f"), "workspace::ToggleProjectSearch");
    add(w, format!("{m}-b"), "workspace::ToggleSidebar");
    add(w, format!("{m}-shift-e"), "workspace::ShowFiles");
    add(w, "ctrl-shift-g".into(), "workspace::ShowChanges");
    add(w, "ctrl-shift-h".into(), "workspace::ShowHistory");
    add(w, format!("{m}-w"), "workspace::CloseTab");
    add(w, format!("{m}-shift-t"), "workspace::ReopenClosedTab");
    add(w, "ctrl-tab".into(), "workspace::NextTab");
    add(w, "ctrl-shift-tab".into(), "workspace::PrevTab");
    if mac {
        add(w, "ctrl--".into(), "workspace::GoBack");
        add(w, "ctrl-shift--".into(), "workspace::GoForward");
        add(w, "cmd-q".into(), "workspace::Quit");
        add(w, "cmd-shift-w".into(), "workspace::CloseWindow");
    } else {
        add(w, "alt-left".into(), "workspace::GoBack");
        add(w, "alt-right".into(), "workspace::GoForward");
        add(w, "ctrl-q".into(), "workspace::Quit");
    }
    add(w, format!("{m}-\\"), "workspace::SplitRight");
    add(w, format!("{m}-,"), "workspace::OpenSettings");
    add(w, format!("{m}-="), "workspace::ZoomIn");
    add(w, format!("{m}-+"), "workspace::ZoomIn");
    add(w, format!("{m}--"), "workspace::ZoomOut");
    add(w, format!("{m}-0"), "workspace::ZoomReset");
    // v0.2: ease of access.
    add(w, format!("{m}-alt-o"), "workspace::OpenFile");
    add(w, "ctrl-r".into(), "workspace::OpenRecent");
    add(w, format!("{m}-shift-n"), "workspace::NewWindow");
    add(w, format!("{m}-k f"), "workspace::CloseFolder");
    add(w, format!("{m}-k {m}-w"), "workspace::CloseAllTabs");
    add(w, format!("{m}-k {m}-s"), "workspace::ShowKeybindings");
    if !mac {
        add(w, "ctrl-shift-w".into(), "workspace::CloseWindow");
        add(w, "f11".into(), "workspace::ToggleFullScreen");
        add(w, "alt-f".into(), "workspace::ShowAppMenu");
        add(w, "f10".into(), "workspace::ShowAppMenu");
    } else {
        add(w, "cmd-ctrl-f".into(), "workspace::ToggleFullScreen");
        add(w, "cmd-m".into(), "workspace::Minimize");
    }
    // v0.3: editing.
    add(w, format!("{m}-alt-s"), "workspace::SaveAll");

    let c = "CodeView";
    add(c, "ctrl-g".into(), "editor::GoToLine");
    add(c, format!("{m}-f"), "editor::Find");
    add(c, format!("{m}-h"), "editor::FindReplace");
    add("Picker", format!("{m}-shift-1"), "editor::ReplaceNext");
    add("Picker", format!("{m}-alt-enter"), "editor::ReplaceAll");
    add(w, format!("{m}-f"), "editor::Find");
    add(w, format!("{m}-h"), "editor::FindReplace");
    if mac {
        add(c, "cmd-g".into(), "editor::FindNext");
        add(c, "cmd-shift-g".into(), "editor::FindPrev");
    } else {
        add(c, "f3".into(), "editor::FindNext");
        add(c, "shift-f3".into(), "editor::FindPrev");
    }
    add(c, "f12".into(), "editor::GoToDefinition");
    add(c, "shift-f12".into(), "editor::FindReferences");
    add(c, format!("{m}-k {m}-i"), "editor::Hover");
    add(c, format!("{m}-alt-["), "editor::FoldAll");
    add(c, format!("{m}-alt-]"), "editor::UnfoldAll");
    add(c, format!("{m}-k {m}-l"), "editor::ToggleFold");
    add(c, "alt-z".into(), "editor::ToggleSoftWrap");
    add(c, format!("{m}-alt-b"), "editor::ToggleBlame");
    add(c, format!("{m}-shift-v"), "editor::ToggleMarkdownPreview");
    add(c, format!("{m}-c"), "editor::Copy");
    add(c, format!("{m}-a"), "editor::SelectAll");
    add(c, format!("{m}-alt-c"), "editor::CopyPath");
    add(c, format!("{m}-shift-alt-c"), "editor::CopyRelativePath");
    add(c, "alt-]".into(), "editor::NextChange");
    add(c, "alt-[".into(), "editor::PrevChange");
    for (k, a) in [
        ("up", "MoveUp"),
        ("down", "MoveDown"),
        ("left", "MoveLeft"),
        ("right", "MoveRight"),
        ("pageup", "PageUp"),
        ("pagedown", "PageDown"),
        ("shift-up", "SelectUp"),
        ("shift-down", "SelectDown"),
        ("shift-left", "SelectLeft"),
        ("shift-right", "SelectRight"),
    ] {
        add(c, k.into(), &format!("editor::{a}"));
    }
    if mac {
        add(c, "cmd-up".into(), "editor::ScrollToTop");
        add(c, "cmd-down".into(), "editor::ScrollToBottom");
    } else {
        add(c, "ctrl-home".into(), "editor::ScrollToTop");
        add(c, "ctrl-end".into(), "editor::ScrollToBottom");
    }

    // v0.3: editing (CodeView of an editable tab; read-only views ignore them).
    add(c, format!("{m}-z"), "editor::Undo");
    if mac {
        add(c, "cmd-shift-z".into(), "editor::Redo");
    } else {
        add(c, "ctrl-y".into(), "editor::Redo");
        add(c, "ctrl-shift-z".into(), "editor::Redo");
    }
    add(c, format!("{m}-x"), "editor::Cut");
    add(c, format!("{m}-v"), "editor::Paste");
    add(c, format!("{m}-s"), "editor::Save");
    add(c, format!("{m}-shift-s"), "editor::SaveAs");
    add(c, "enter".into(), "editor::Newline");
    add(c, "shift-enter".into(), "editor::Newline");
    add(c, format!("{m}-enter"), "editor::NewlineBelow");
    add(c, format!("{m}-shift-enter"), "editor::NewlineAbove");
    add(c, "backspace".into(), "editor::Backspace");
    add(c, "shift-backspace".into(), "editor::Backspace");
    add(c, "delete".into(), "editor::Delete");
    if mac {
        add(c, "alt-backspace".into(), "editor::DeleteWordLeft");
        add(c, "alt-delete".into(), "editor::DeleteWordRight");
        add(c, "cmd-backspace".into(), "editor::DeleteToLineStart");
    } else {
        add(c, "ctrl-backspace".into(), "editor::DeleteWordLeft");
        add(c, "ctrl-delete".into(), "editor::DeleteWordRight");
    }
    add(c, format!("{m}-shift-k"), "editor::DeleteLine");
    add(c, "tab".into(), "editor::Tab");
    add(c, "shift-tab".into(), "editor::Outdent");
    add(c, format!("{m}-]"), "editor::Indent");
    add(c, format!("{m}-["), "editor::Outdent");
    add(c, format!("{m}-/"), "editor::ToggleComment");
    add(c, format!("{m}-shift-d"), "editor::DuplicateLine");
    add(c, "alt-up".into(), "editor::MoveLineUp");
    add(c, "alt-down".into(), "editor::MoveLineDown");
    add(c, format!("{m}-l"), "editor::SelectLine");
    add(c, format!("{m}-d"), "editor::SelectNextOccurrence");
    add(c, format!("{m}-shift-l"), "editor::SelectAllOccurrences");
    if mac {
        add(c, "cmd-alt-up".into(), "editor::AddCursorAbove");
        add(c, "cmd-alt-down".into(), "editor::AddCursorBelow");
        add(c, "shift-alt-f".into(), "editor::Format");
    } else {
        add(c, "shift-alt-up".into(), "editor::AddCursorAbove");
        add(c, "shift-alt-down".into(), "editor::AddCursorBelow");
        add(c, "ctrl-shift-i".into(), "editor::Format");
    }
    add(c, "shift-alt-i".into(), "editor::SplitSelectionIntoLines");
    add(c, format!("{m}-."), "editor::CodeActions");
    add(c, "f2".into(), "editor::Rename");
    add(c, "f8".into(), "editor::NextDiagnostic");
    add(c, "shift-f8".into(), "editor::PrevDiagnostic");
    add(w, format!("{m}-shift-m"), "workspace::ShowProblems");
    add(w, format!("{m}-alt-z"), "workspace::UndoLastOperation");
    // v0.5 agent sessions.
    add(w, format!("{m}-alt-i"), "agent::TogglePanel");
    add(w, format!("{m}-shift-alt-z"), "agent::UndoLastTurn");
    add(w, format!("{m}-alt-r"), "agent::ReviewLastTurn");
    add(w, format!("{m}-alt-f"), "agent::SendFeedback");
    add("AgentComposer", format!("{m}-enter"), "agent::SendPrompt");
    add("AgentPanel", "escape".into(), "agent::Cancel");
    // v0.7 inline agent edit: ask for a change in place, then decide each hunk. The decision
    // keys apply only while hunks are pending (`inline_edit` is set on the code view then).
    add(c, format!("{m}-i"), "agent::EditWithAgent");
    let ie = "CodeView && inline_edit";
    add(ie, "alt-y".into(), "agent::AcceptInlineHunk");
    add(ie, "alt-n".into(), "agent::RejectInlineHunk");
    add(ie, "alt-shift-y".into(), "agent::AcceptAllInlineEdits");
    add(ie, "alt-shift-n".into(), "agent::RejectAllInlineEdits");
    // v0.7 integrated terminal. `Terminal` is the focused terminal: keys bound in `Workspace`
    // are not seen there (they reach the shell) except a reserved few, see lector-ui-terminal.
    add(w, "ctrl-`".into(), "terminal::TogglePanel");
    // X11/Wayland and macOS report Shift+` as `~` (shift is dropped for symbols). Aliases come
    // first: the last binding of an action is the one tooltips and menus show.
    add(w, "ctrl-~".into(), "terminal::NewTerminal");
    add(w, "ctrl-shift-`".into(), "terminal::NewTerminal");
    let t = "Terminal";
    if mac {
        add(t, "cmd-c".into(), "terminal::Copy");
        add(t, "cmd-v".into(), "terminal::Paste");
        add(t, "cmd-a".into(), "terminal::SelectAll");
        add(t, "cmd-f".into(), "terminal::Find");
        add(t, "cmd-k".into(), "terminal::Clear");
        add(t, "cmd-w".into(), "terminal::KillTerminal");
        add(t, "cmd-\\".into(), "terminal::SplitTerminal");
        add(t, "cmd-shift-]".into(), "terminal::NextTerminal");
        add(t, "cmd-shift-[".into(), "terminal::PrevTerminal");
        add(t, "cmd-up".into(), "terminal::ScrollToPrevCommand");
        add(t, "cmd-down".into(), "terminal::ScrollToNextCommand");
        add(t, "cmd-alt-right".into(), "terminal::FocusNextSplit");
        add(t, "cmd-alt-left".into(), "terminal::FocusPrevSplit");
    } else {
        add(t, "ctrl-shift-c".into(), "terminal::Copy");
        add(t, "ctrl-insert".into(), "terminal::Copy");
        add(t, "ctrl-shift-v".into(), "terminal::Paste");
        add(t, "shift-insert".into(), "terminal::Paste");
        add(t, "ctrl-shift-a".into(), "terminal::SelectAll");
        add(t, "ctrl-shift-f".into(), "terminal::Find");
        add(t, "ctrl-shift-k".into(), "terminal::Clear");
        add(t, "ctrl-shift-w".into(), "terminal::KillTerminal");
        // The panel itself (F6 focus, nothing to kill) must not hand Ctrl+Shift+W to
        // Close Window.
        add(
            "TerminalPanel",
            "ctrl-shift-w".into(),
            "terminal::KillTerminal",
        );
        // Shift+5 arrives as `%` on X11/Wayland (shift is dropped for symbols and digits).
        add(t, "ctrl-%".into(), "terminal::SplitTerminal");
        add(t, "ctrl-shift-5".into(), "terminal::SplitTerminal");
        add(t, "ctrl-pagedown".into(), "terminal::NextTerminal");
        add(t, "ctrl-pageup".into(), "terminal::PrevTerminal");
        add(t, "ctrl-shift-up".into(), "terminal::ScrollToPrevCommand");
        add(t, "ctrl-shift-down".into(), "terminal::ScrollToNextCommand");
        add(t, "alt-shift-right".into(), "terminal::FocusNextSplit");
        add(t, "alt-shift-left".into(), "terminal::FocusPrevSplit");
    }
    add("TerminalFind", "enter".into(), "terminal::FindNext");
    add("TerminalFind", "shift-enter".into(), "terminal::FindPrev");
    add("TerminalFind", "escape".into(), "terminal::CloseFind");
    // v0.4 accessibility: keyboard-only operation, interface zoom.
    add(w, "f6".into(), "workspace::FocusNextRegion");
    add(w, "shift-f6".into(), "workspace::FocusPrevRegion");
    add(w, "shift-f10".into(), "workspace::ShowContextMenu");
    add(w, "menu".into(), "workspace::ShowContextMenu");
    add(w, "tab".into(), "workspace::FocusNextControl");
    add(w, "shift-tab".into(), "workspace::FocusPrevControl");
    add(w, format!("{m}-alt-="), "workspace::UiZoomIn");
    add(w, format!("{m}-alt-+"), "workspace::UiZoomIn");
    add(w, format!("{m}-alt--"), "workspace::UiZoomOut");
    add(w, format!("{m}-alt-0"), "workspace::UiZoomReset");
    add(c, "escape".into(), "editor::ClearSelection");
    // v0.7: completion and signature help. The `completion` / `signature_help` identifiers are
    // added to the CodeView key context only while the popup is open (see
    // `lector_ui::code_view::CodeView::set_context_flag`). These come after the plain CodeView
    // bindings above on purpose: for equal depth GPUI prefers the binding defined last.
    add(c, "ctrl-space".into(), "editor::TriggerSuggest");
    add(
        c,
        "ctrl-shift-space".into(),
        "editor::TriggerParameterHints",
    );
    let cc = "CodeView && completion";
    add(cc, "down".into(), "completion::SelectNext");
    add(cc, "up".into(), "completion::SelectPrev");
    add(cc, "ctrl-n".into(), "completion::SelectNext");
    add(cc, "ctrl-p".into(), "completion::SelectPrev");
    add(cc, "pagedown".into(), "completion::PageDown");
    add(cc, "pageup".into(), "completion::PageUp");
    add(cc, "enter".into(), "completion::AcceptOnEnter");
    add(cc, "tab".into(), "completion::Accept");
    add(cc, "escape".into(), "completion::Dismiss");
    let cs = "CodeView && signature_help";
    add(cs, "alt-down".into(), "signature::NextSignature");
    add(cs, "alt-up".into(), "signature::PrevSignature");
    add(
        "CodeView && signature_overloads && !completion",
        "down".into(),
        "signature::NextSignature",
    );
    add(
        "CodeView && signature_overloads && !completion",
        "up".into(),
        "signature::PrevSignature",
    );
    add(
        "CodeView && signature_help && !completion",
        "escape".into(),
        "signature::Dismiss",
    );
    // A completion request that has not been answered yet has no popup to close, but Escape
    // still cancels it (and nothing else).
    add(
        "CodeView && completion_pending",
        "escape".into(),
        "completion::Dismiss",
    );

    let d = "DiffView";
    add(d, "alt-n".into(), "diff::NextHunk");
    add(d, "alt-p".into(), "diff::PrevHunk");
    // Plain letters bind only where nothing is typed: a text field inside the diff / review
    // tab (comment box, find bar) carries `TextInput`, which switches them off.
    let dp = "DiffView && !TextInput";
    add(dp, "n".into(), "diff::NextHunk");
    add(dp, "p".into(), "diff::PrevHunk");
    add(d, "alt-j".into(), "diff::NextFile");
    add(d, "alt-k".into(), "diff::PrevFile");
    add(dp, "v".into(), "diff::ToggleViewed");
    // v0.4 review queue: mark the hunk at the cursor reviewed, jump to the next unreviewed hunk.
    add(dp, "r".into(), "review::ToggleHunkReviewed");
    add(dp, "]".into(), "review::NextUnreviewed");
    add(d, "alt-shift-j".into(), "review::NextUnreviewed");
    // v0.6 review comments and pull requests.
    add(dp, "c".into(), "review::AddComment");
    add(d, "alt-shift-n".into(), "review::NextComment");
    add(d, "alt-shift-p".into(), "review::PrevComment");
    add(d, format!("{m}-alt-enter"), "review::SubmitReview");
    add(w, format!("{m}-alt-p"), "review::ShowPullRequests");
    add(d, format!("{m}-alt-m"), "diff::ToggleMovedCode");
    add(d, format!("{m}-alt-t"), "diff::ToggleStructural");
    add(d, format!("{m}-alt-d"), "diff::ToggleLayout");
    // v0.2: staging from the diff view. No bare-letter bindings: a stray key
    // (e.g. typing after Ctrl+F, which does not open a find bar in diffs) must
    // never change the index.
    add(d, format!("{m}-alt-s"), "git::StageHunk");
    add(d, format!("{m}-alt-u"), "git::UnstageHunk");

    // v0.2: changes panel and commit message editor.
    let g = "ChangesPanel";
    add(g, "space".into(), "git::Stage");
    add(g, format!("{m}-enter"), "git::Commit");
    let cm = "CommitEditor";
    add(cm, format!("{m}-enter"), "git::Commit");
    add(w, format!("{m}-k {m}-c"), "git::FocusCommitMessage");
    add(w, format!("{m}-shift-r"), "git::Refresh");
    // v0.7: sync with the remote, branches, stash (all rebindable). A `{m}-k` chord with a
    // plain second key, like `{m}-k f`; none of them is a plain key on its own.
    add(w, format!("{m}-k y"), "git::Sync");
    add(w, format!("{m}-k t"), "git::Fetch");
    add(w, format!("{m}-k l"), "git::Pull");
    add(w, format!("{m}-k p"), "git::Push");
    add(w, format!("{m}-k n"), "git::NewBranch");
    add(w, format!("{m}-k b"), "git::SwitchBranch");
    add(w, format!("{m}-k z"), "git::StashChanges");

    // v0.3: conflict resolution.
    let mg = "MergeEditor";
    add(mg, "alt-1".into(), "git::AcceptOurs");
    add(mg, "alt-2".into(), "git::AcceptTheirs");
    add(mg, "alt-3".into(), "git::AcceptBoth");
    add(mg, "alt-n".into(), "git::NextConflict");
    add(mg, "alt-p".into(), "git::PrevConflict");

    let p = "Picker";
    add(p, "enter".into(), "picker::Confirm");
    add(p, "escape".into(), "picker::Cancel");
    add(p, "down".into(), "picker::SelectNext");
    add(p, "up".into(), "picker::SelectPrev");
    if mac {
        add(p, "ctrl-n".into(), "picker::SelectNext");
        add(p, "ctrl-p".into(), "picker::SelectPrev");
    }
    out
}

/// Apply user bindings over `defaults`. A user binding replaces a default with
/// the same keys and context; an empty / `"none"` action only removes it.
pub fn merge_keymap(defaults: Vec<KeyBinding>, user: Vec<KeyBinding>) -> Vec<KeyBinding> {
    let mut out = defaults;
    for u in user {
        out.retain(|d| !(d.keys == u.keys && d.context == u.context));
        let a = u.action.trim();
        if !(a.is_empty() || a.eq_ignore_ascii_case("none")) {
            out.push(u);
        }
    }
    out
}

#[derive(Deserialize, Default)]
struct KeymapFile {
    #[serde(default, alias = "binding", alias = "keymap")]
    bindings: Vec<KeyBinding>,
}

pub(crate) fn load_keymap_from(file: &Path, platform: Platform) -> (Vec<KeyBinding>, Vec<String>) {
    let defaults = default_keymap(platform);
    let text = match std::fs::read_to_string(file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (defaults, vec![]),
        Err(e) => return (defaults, vec![format!("{}: {e}", file.display())]),
    };
    match toml::from_str::<KeymapFile>(&text) {
        Ok(f) => (merge_keymap(defaults, f.bindings), vec![]),
        Err(e) => (defaults, vec![format!("{}: {e}", file.display())]),
    }
}

/// Defaults for the current platform merged with the user's `keymap.toml`
/// (`[[bindings]]` entries with `keys`, `action`, optional `context`).
pub fn load_keymap() -> (Vec<KeyBinding>, Vec<String>) {
    load_keymap_from(&paths::keymap_file(), Platform::current())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(km: &'a [KeyBinding], action: &str) -> Vec<&'a str> {
        km.iter()
            .filter(|b| b.action == action)
            .map(|b| b.keys.as_str())
            .collect()
    }

    #[test]
    fn platform_defaults() {
        let mac = default_keymap(Platform::MacOS);
        let lin = default_keymap(Platform::Linux);
        assert_eq!(find(&mac, "workspace::ToggleFileFinder"), ["cmd-p"]);
        assert_eq!(find(&lin, "workspace::ToggleFileFinder"), ["ctrl-p"]);
        assert_eq!(find(&lin, "workspace::GoBack"), ["alt-left"]);
        assert_eq!(find(&mac, "editor::Hover"), ["cmd-k cmd-i"]);
        assert_eq!(find(&lin, "editor::FindNext"), ["f3"]);
        assert_eq!(
            find(&mac, "editor::FindReplace"),
            ["cmd-h", "cmd-h"],
            "code view and workspace"
        );
        assert_eq!(find(&lin, "editor::ReplaceNext"), ["ctrl-shift-1"]);
        assert_eq!(find(&lin, "editor::ReplaceAll"), ["ctrl-alt-enter"]);
        assert_eq!(find(&lin, "editor::GoToDefinition"), ["f12"]);
        assert_eq!(find(&lin, "editor::TriggerSuggest"), ["ctrl-space"]);
        assert_eq!(find(&mac, "completion::AcceptOnEnter"), ["enter"]);
    }

    /// X11/Wayland report Ctrl+Shift+<symbol or digit> with the shifted character and no shift
    /// modifier, so each such default has an alias on that character, listed before the
    /// canonical spelling (tooltips show the last binding of an action).
    #[test]
    fn shifted_symbol_defaults_have_x11_aliases() {
        let lin = default_keymap(Platform::Linux);
        assert_eq!(
            find(&lin, "terminal::SplitTerminal"),
            ["ctrl-%", "ctrl-shift-5"]
        );
        assert_eq!(
            find(&lin, "terminal::NewTerminal"),
            ["ctrl-~", "ctrl-shift-`"]
        );
        // No new default uses Ctrl+Shift with a digit or symbol without such an alias.
        let terminal_or_workspace = |b: &&KeyBinding| {
            matches!(b.context.as_deref(), Some("Terminal") | Some("Workspace"))
                && b.action.starts_with("terminal::")
        };
        for b in lin.iter().filter(terminal_or_workspace) {
            for stroke in b.keys.split_whitespace() {
                let Some(rest) = stroke.strip_prefix("ctrl-shift-") else {
                    continue;
                };
                let single = rest.chars().count() == 1;
                let c = rest.chars().next().unwrap_or('a');
                if single && !c.is_ascii_alphabetic() {
                    let others = find(&lin, &b.action);
                    assert!(
                        others.len() >= 2,
                        "{stroke} ({}) needs the shifted-character alias",
                        b.action
                    );
                }
            }
        }
    }

    /// The completion keys only bind while the popup is open, and after the plain CodeView
    /// bindings of the same keys (GPUI prefers the last binding at equal depth).
    #[test]
    fn completion_keys_are_scoped_and_come_last() {
        for p in [Platform::MacOS, Platform::Linux, Platform::Windows] {
            let km = default_keymap(p);
            for (i, b) in km.iter().enumerate() {
                if b.action.starts_with("completion::") || b.action.starts_with("signature::") {
                    let ctx = b.context.as_deref().unwrap_or_default();
                    assert!(
                        ctx.contains("completion") || ctx.contains("signature_"),
                        "{b:?}"
                    );
                    let plain = km
                        .iter()
                        .position(|o| o.keys == b.keys && o.context.as_deref() == Some("CodeView"));
                    if let Some(plain) = plain {
                        assert!(plain < i, "{p:?}: {b:?} must follow the plain binding");
                    }
                }
            }
        }
    }

    /// Regression: writes to the repository are never bound to a bare letter
    /// (typing into a diff that has no text field must not stage anything).
    #[test]
    fn git_writes_need_a_modifier() {
        for p in [Platform::MacOS, Platform::Linux, Platform::Windows] {
            for b in default_keymap(p) {
                let writes = b.action.starts_with("git::")
                    && ["Stage", "Unstage", "Discard", "Commit", "Accept", "Resolve"]
                        .iter()
                        .any(|w| b.action[5..].starts_with(w));
                if writes {
                    let first = b.keys.split(' ').next().unwrap_or_default();
                    assert!(
                        first.contains('-') && first.len() > 1 || first == "space",
                        "{p:?}: {} bound to bare `{}`",
                        b.action,
                        b.keys
                    );
                }
            }
        }
    }

    /// Plain-letter and bracket keys only bind in read-only review contexts and never where text
    /// is typed.
    #[test]
    fn plain_keys_are_review_only_and_not_in_text_inputs() {
        for p in [Platform::MacOS, Platform::Linux, Platform::Windows] {
            for b in default_keymap(p) {
                let first = b.keys.split(' ').next().unwrap_or_default();
                let plain =
                    first.len() == 1 && first != " " && !first.chars().all(char::is_numeric);
                if plain && b.action.starts_with("review::")
                    || b.action.starts_with("diff::") && plain
                {
                    let ctx = b.context.as_deref().unwrap_or_default();
                    assert!(
                        ctx.starts_with("DiffView") && ctx.contains("!TextInput"),
                        "{p:?}: {} on plain `{}` needs a read-only context, got `{ctx}`",
                        b.action,
                        b.keys
                    );
                }
            }
            let km = default_keymap(p);
            assert_eq!(find(&km, "review::NextUnreviewed"), ["]", "alt-shift-j"]);
            assert_eq!(find(&km, "review::ToggleHunkReviewed"), ["r"]);
        }
    }

    #[test]
    fn no_duplicate_key_in_context() {
        for p in [Platform::MacOS, Platform::Linux, Platform::Windows] {
            let km = default_keymap(p);
            for (i, a) in km.iter().enumerate() {
                for b in &km[i + 1..] {
                    assert!(
                        !(a.keys == b.keys && a.context == b.context),
                        "{p:?}: {a:?} vs {b:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn user_merge() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("keymap.toml");
        std::fs::write(
            &f,
            r#"
[[bindings]]
keys = "ctrl-p"
action = "workspace::ToggleCommandPalette"
context = "Workspace"
[[bindings]]
keys = "f12"
action = "none"
context = "CodeView"
"#,
        )
        .unwrap();
        let (km, w) = load_keymap_from(&f, Platform::Linux);
        assert!(w.is_empty());
        assert_eq!(
            find(&km, "workspace::ToggleCommandPalette"),
            ["ctrl-shift-p", "ctrl-p"]
        );
        assert!(find(&km, "workspace::ToggleFileFinder").is_empty());
        assert!(find(&km, "editor::GoToDefinition").is_empty());
        std::fs::write(&f, "[[bindings]\n").unwrap();
        let (km, w) = load_keymap_from(&f, Platform::Linux);
        assert_eq!(km, default_keymap(Platform::Linux));
        assert_eq!(w.len(), 1);
    }
}
