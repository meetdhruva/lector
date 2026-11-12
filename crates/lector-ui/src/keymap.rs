//! Default key bindings.
//!
//! This hardcoded table mirrors `docs/ARCHITECTURE.md`; the integration wave
//! replaces it with `lector_config::default_keymap` + user overrides through
//! [`bind_keymap`], which takes the same `(keys, action name, context)` shape.

use std::rc::Rc;

use gpui::{App, DummyKeyboardMapper, KeyBinding, KeyBindingContextPredicate};

/// One binding: keystrokes in GPUI syntax, a registered action name and an
/// optional context predicate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeySpec {
    /// Keystrokes, e.g. `"ctrl-shift-p"` or `"cmd-k cmd-i"`.
    pub keys: String,
    /// Action name, e.g. `"workspace::ToggleCommandPalette"`.
    pub action: String,
    /// Context predicate, e.g. `"CodeView"`.
    pub context: Option<String>,
}

/// Key contexts used by Lector views.
pub mod contexts {
    /// The workspace root.
    pub const WORKSPACE: &str = "Workspace";
    /// A code view.
    pub const CODE_VIEW: &str = "CodeView";
    /// A diff view.
    pub const DIFF_VIEW: &str = "DiffView";
    /// A picker modal.
    pub const PICKER: &str = "Picker";
    /// A single-line text input.
    pub const TEXT_INPUT: &str = "TextInput";
    /// A context menu.
    pub const CONTEXT_MENU: &str = "ContextMenu";
    /// A tree view.
    pub const TREE_VIEW: &str = "TreeView";
    /// A multi-line text area (also carries `TextInput`).
    pub const TEXT_AREA: &str = "TextArea";
    /// The Changes panel file list.
    pub const CHANGES_PANEL: &str = "ChangesPanel";
    /// A keyboard-navigable list panel or results view (history, branches, references...):
    /// Up / Down move the selection, Enter opens it.
    pub const LIST_PANEL: &str = "ListPanel";
    /// The commit message editor.
    pub const COMMIT_EDITOR: &str = "CommitEditor";
    /// A focused terminal (v0.7, `lector-ui-terminal`): keys bound in `Workspace` are not seen
    /// here (they reach the shell) except a reserved few.
    pub const TERMINAL: &str = "Terminal";
    /// The terminal find bar.
    pub const TERMINAL_FIND: &str = "TerminalFind";
}

const fn mac() -> bool {
    cfg!(target_os = "macos")
}

/// Default bindings for the current platform. `cmd` is written for the
/// primary modifier and becomes `ctrl` outside macOS.
pub fn default_keymap() -> Vec<KeySpec> {
    use contexts::*;
    let primary = |keys: &str| {
        if mac() {
            keys.to_string()
        } else {
            keys.replace("cmd-", "ctrl-")
        }
    };
    let mut out = Vec::new();
    let mut add = |keys: String, action: &str, context: Option<&str>| {
        out.push(KeySpec {
            keys,
            action: action.to_string(),
            context: context.map(str::to_string),
        });
    };

    // Workspace (global within the window).
    let ws = Some(WORKSPACE);
    for (keys, action) in [
        ("cmd-p", "workspace::ToggleFileFinder"),
        ("cmd-shift-p", "workspace::ToggleCommandPalette"),
        ("cmd-t", "workspace::ToggleSymbolFinder"),
        ("cmd-shift-o", "workspace::ToggleOutline"),
        ("cmd-shift-f", "workspace::ToggleProjectSearch"),
        ("cmd-b", "workspace::ToggleSidebar"),
        ("cmd-shift-e", "workspace::ShowFiles"),
        ("cmd-w", "workspace::CloseTab"),
        ("cmd-shift-t", "workspace::ReopenClosedTab"),
        ("cmd-=", "workspace::ZoomIn"),
        ("cmd-+", "workspace::ZoomIn"),
        ("cmd--", "workspace::ZoomOut"),
        ("cmd-0", "workspace::ZoomReset"),
        ("cmd-,", "workspace::OpenSettings"),
        ("cmd-o", "workspace::OpenFolder"),
        ("cmd-\\", "workspace::SplitRight"),
    ] {
        add(primary(keys), action, ws);
    }
    add("ctrl-shift-g".into(), "workspace::ShowChanges", ws);
    add("ctrl-shift-h".into(), "workspace::ShowHistory", ws);
    add("ctrl-tab".into(), "workspace::NextTab", ws);
    add("ctrl-shift-tab".into(), "workspace::PrevTab", ws);
    if mac() {
        add("ctrl--".into(), "workspace::GoBack", ws);
        add("ctrl-shift--".into(), "workspace::GoForward", ws);
        add("cmd-q".into(), "workspace::Quit", None);
        add("cmd-shift-w".into(), "workspace::CloseWindow", ws);
    } else {
        add("alt-left".into(), "workspace::GoBack", ws);
        add("alt-right".into(), "workspace::GoForward", ws);
        add("ctrl-q".into(), "workspace::Quit", None);
        add("ctrl-shift-w".into(), "workspace::CloseWindow", ws);
    }

    // Code view.
    let cv = Some(CODE_VIEW);
    add("ctrl-g".into(), "editor::GoToLine", ws);
    for (keys, action) in [
        ("cmd-f", "editor::Find"),
        ("f12", "editor::GoToDefinition"),
        ("shift-f12", "editor::FindReferences"),
        ("cmd-k cmd-i", "editor::Hover"),
        ("cmd-alt-[", "editor::ToggleFold"),
        ("cmd-alt-]", "editor::ToggleFold"),
        ("cmd-alt-b", "editor::ToggleBlame"),
        ("cmd-shift-v", "editor::ToggleMarkdownPreview"),
        ("cmd-c", "editor::Copy"),
        ("cmd-a", "editor::SelectAll"),
        ("cmd-up", "editor::ScrollToTop"),
        ("cmd-down", "editor::ScrollToBottom"),
        ("cmd-shift-up", "editor::SelectToTop"),
        ("cmd-shift-down", "editor::SelectToBottom"),
    ] {
        add(primary(keys), action, cv);
    }
    if mac() {
        add("cmd-g".into(), "editor::FindNext", cv);
        add("cmd-shift-g".into(), "editor::FindPrev", cv);
        add("cmd-left".into(), "editor::MoveToLineStart", cv);
        add("cmd-right".into(), "editor::MoveToLineEnd", cv);
        add("cmd-shift-left".into(), "editor::SelectToLineStart", cv);
        add("cmd-shift-right".into(), "editor::SelectToLineEnd", cv);
        add("alt-left".into(), "editor::MoveWordLeft", cv);
        add("alt-right".into(), "editor::MoveWordRight", cv);
        add("alt-shift-left".into(), "editor::SelectWordLeft", cv);
        add("alt-shift-right".into(), "editor::SelectWordRight", cv);
    } else {
        add("f3".into(), "editor::FindNext", cv);
        add("shift-f3".into(), "editor::FindPrev", cv);
        add("ctrl-left".into(), "editor::MoveWordLeft", cv);
        add("ctrl-right".into(), "editor::MoveWordRight", cv);
        add("ctrl-shift-left".into(), "editor::SelectWordLeft", cv);
        add("ctrl-shift-right".into(), "editor::SelectWordRight", cv);
        add("ctrl-home".into(), "editor::ScrollToTop", cv);
        add("ctrl-end".into(), "editor::ScrollToBottom", cv);
        add("ctrl-shift-home".into(), "editor::SelectToTop", cv);
        add("ctrl-shift-end".into(), "editor::SelectToBottom", cv);
    }
    for (keys, action) in [
        ("alt-z", "editor::ToggleSoftWrap"),
        ("up", "editor::MoveUp"),
        ("down", "editor::MoveDown"),
        ("left", "editor::MoveLeft"),
        ("right", "editor::MoveRight"),
        ("shift-up", "editor::SelectUp"),
        ("shift-down", "editor::SelectDown"),
        ("shift-left", "editor::SelectLeft"),
        ("shift-right", "editor::SelectRight"),
        ("home", "editor::MoveToLineStart"),
        ("end", "editor::MoveToLineEnd"),
        ("shift-home", "editor::SelectToLineStart"),
        ("shift-end", "editor::SelectToLineEnd"),
        ("pageup", "editor::PageUp"),
        ("pagedown", "editor::PageDown"),
        ("shift-pageup", "editor::SelectPageUp"),
        ("shift-pagedown", "editor::SelectPageDown"),
        ("escape", "editor::ClearSelection"),
        ("alt-]", "editor::NextChange"),
        ("alt-[", "editor::PrevChange"),
    ] {
        add(keys.into(), action, cv);
    }

    // Diff view.
    let dv = Some(DIFF_VIEW);
    for (keys, action) in [
        ("alt-n", "diff::NextHunk"),
        ("alt-p", "diff::PrevHunk"),
        ("n", "diff::NextHunk"),
        ("p", "diff::PrevHunk"),
        ("alt-j", "diff::NextFile"),
        ("alt-k", "diff::PrevFile"),
        ("v", "diff::ToggleViewed"),
    ] {
        add(keys.into(), action, dv);
    }
    add(primary("cmd-alt-d"), "diff::ToggleLayout", dv);

    // Pickers, menus and trees.
    for ctx in [PICKER, CONTEXT_MENU] {
        add("up".into(), "picker::SelectPrev", Some(ctx));
        add("down".into(), "picker::SelectNext", Some(ctx));
        add("ctrl-p".into(), "picker::SelectPrev", Some(ctx));
        add("ctrl-n".into(), "picker::SelectNext", Some(ctx));
        add("enter".into(), "picker::Confirm", Some(ctx));
        add("escape".into(), "picker::Cancel", Some(ctx));
    }
    // Secondary confirm (e.g. open a recent project in a new window); the
    // delegate reads the held modifier.
    add(primary("cmd-enter"), "picker::Confirm", Some(PICKER));
    let tv = Some(TREE_VIEW);
    add("up".into(), "picker::SelectPrev", tv);
    add("down".into(), "picker::SelectNext", tv);
    add("enter".into(), "picker::Confirm", tv);
    add("space".into(), "picker::Confirm", tv);
    add("right".into(), "tree::ExpandSelected", tv);
    add("left".into(), "tree::CollapseSelected", tv);

    // Text inputs.
    let ti = Some(TEXT_INPUT);
    for (keys, action) in [
        ("backspace", "text_input::Backspace"),
        ("delete", "text_input::Delete"),
        ("left", "text_input::Left"),
        ("right", "text_input::Right"),
        ("shift-left", "text_input::SelectLeft"),
        ("shift-right", "text_input::SelectRight"),
        ("home", "text_input::Home"),
        ("end", "text_input::End"),
    ] {
        add(keys.into(), action, ti);
    }
    for (keys, action) in [
        ("cmd-a", "text_input::SelectAll"),
        ("cmd-v", "text_input::Paste"),
        ("cmd-c", "text_input::Copy"),
        ("cmd-x", "text_input::Cut"),
    ] {
        add(primary(keys), action, ti);
    }
    if mac() {
        add(
            "ctrl-cmd-space".into(),
            "text_input::ShowCharacterPalette",
            ti,
        );
    }

    // Multi-line text areas (commit message); `text_input::*` above applies too.
    let ta = Some(TEXT_AREA);
    for (keys, action) in [
        ("enter", "text_area::Newline"),
        ("shift-enter", "text_area::Newline"),
        ("up", "text_area::MoveUp"),
        ("down", "text_area::MoveDown"),
        ("shift-up", "text_area::SelectUp"),
        ("shift-down", "text_area::SelectDown"),
        ("pageup", "text_area::PageUp"),
        ("pagedown", "text_area::PageDown"),
        ("shift-home", "text_area::SelectToLineStart"),
        ("shift-end", "text_area::SelectToLineEnd"),
        ("cmd-z", "text_area::Undo"),
        ("cmd-shift-z", "text_area::Redo"),
    ] {
        add(primary(keys), action, ta);
    }
    let word = if mac() { "alt" } else { "ctrl" };
    add(format!("{word}-left"), "text_area::WordLeft", ta);
    add(format!("{word}-right"), "text_area::WordRight", ta);
    add(
        format!("{word}-shift-left"),
        "text_area::SelectWordLeft",
        ta,
    );
    add(
        format!("{word}-shift-right"),
        "text_area::SelectWordRight",
        ta,
    );
    add(format!("{word}-backspace"), "text_area::DeleteWordLeft", ta);
    if mac() {
        add("cmd-up".into(), "text_area::MoveToStart", ta);
        add("cmd-down".into(), "text_area::MoveToEnd", ta);
        add("cmd-shift-up".into(), "text_area::SelectToStart", ta);
        add("cmd-shift-down".into(), "text_area::SelectToEnd", ta);
        add("cmd-left".into(), "text_input::Home", ta);
        add("cmd-right".into(), "text_input::End", ta);
    } else {
        add("ctrl-home".into(), "text_area::MoveToStart", ta);
        add("ctrl-end".into(), "text_area::MoveToEnd", ta);
        add("ctrl-shift-home".into(), "text_area::SelectToStart", ta);
        add("ctrl-shift-end".into(), "text_area::SelectToEnd", ta);
        add("ctrl-y".into(), "text_area::Redo", ta);
    }

    // Generic list panels.
    let lp = Some(LIST_PANEL);
    add("up".into(), "picker::SelectPrev", lp);
    add("down".into(), "picker::SelectNext", lp);
    add("enter".into(), "picker::Confirm", lp);

    // Changes panel list (`space` = git::Stage comes from lector-config's keymap).
    let cp = Some(CHANGES_PANEL);
    add("up".into(), "picker::SelectPrev", cp);
    add("down".into(), "picker::SelectNext", cp);
    add("enter".into(), "picker::Confirm", cp);
    out
}

/// Extra bindings derived from the effective keymap (e.g. the terminal's pass-through
/// `zed::NoAction` entries); see [`add_keymap_extension`].
pub type KeymapExtension = fn(&[KeySpec]) -> Vec<KeySpec>;

#[derive(Default)]
struct KeymapExtensions(Vec<KeymapExtension>);

impl gpui::Global for KeymapExtensions {}

/// Registers `f`: every time the keymap is (re)bound, `f` sees the effective bindings and
/// returns more to bind after them. Used by extension crates whose key contexts depend on
/// what the user bound elsewhere.
pub fn add_keymap_extension(cx: &mut App, f: KeymapExtension) {
    cx.default_global::<KeymapExtensions>().0.push(f);
}

/// The bindings the registered extensions add for `specs`.
pub fn extension_bindings(cx: &App, specs: &[KeySpec]) -> Vec<KeySpec> {
    cx.try_global::<KeymapExtensions>()
        .map(|e| e.0.iter().flat_map(|f| f(specs)).collect())
        .unwrap_or_default()
}

/// Binds `specs`, skipping (and returning) entries whose action is unknown
/// or whose keystrokes or context fail to parse.
pub fn bind_keymap(cx: &mut App, specs: &[KeySpec]) -> Vec<String> {
    let mut errors = Vec::new();
    let mut bindings = Vec::with_capacity(specs.len());
    for spec in specs {
        let action = match cx.build_action(&spec.action, None) {
            Ok(action) => action,
            Err(err) => {
                errors.push(format!("{}: {err}", spec.action));
                continue;
            }
        };
        let predicate = match spec
            .context
            .as_deref()
            .map(KeyBindingContextPredicate::parse)
        {
            None => None,
            Some(Ok(p)) => Some(Rc::new(p)),
            Some(Err(err)) => {
                errors.push(format!("{}: invalid context: {err}", spec.action));
                continue;
            }
        };
        match KeyBinding::load(
            &spec.keys,
            action,
            predicate,
            false,
            None,
            &DummyKeyboardMapper,
        ) {
            Ok(binding) => bindings.push(binding),
            Err(err) => errors.push(format!("{} ({}): {err}", spec.action, spec.keys)),
        }
    }
    cx.bind_keys(bindings);
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn primary_modifier_is_platform_specific() {
        let keymap = default_keymap();
        let palette = keymap
            .iter()
            .find(|k| k.action == "workspace::ToggleCommandPalette")
            .unwrap();
        if mac() {
            assert_eq!(palette.keys, "cmd-shift-p");
        } else {
            assert_eq!(palette.keys, "ctrl-shift-p");
        }
        assert!(keymap.iter().all(|k| k.action.contains("::")));
    }
}
