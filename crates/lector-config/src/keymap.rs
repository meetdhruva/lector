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

    let c = "CodeView";
    add(c, "ctrl-g".into(), "editor::GoToLine");
    add(c, format!("{m}-f"), "editor::Find");
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

    let d = "DiffView";
    add(d, "alt-n".into(), "diff::NextHunk");
    add(d, "alt-p".into(), "diff::PrevHunk");
    add(d, "n".into(), "diff::NextHunk");
    add(d, "p".into(), "diff::PrevHunk");
    add(d, "alt-j".into(), "diff::NextFile");
    add(d, "alt-k".into(), "diff::PrevFile");
    add(d, "v".into(), "diff::ToggleViewed");
    add(d, format!("{m}-alt-d"), "diff::ToggleLayout");

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
        assert_eq!(find(&lin, "editor::GoToDefinition"), ["f12"]);
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
