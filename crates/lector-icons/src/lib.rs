//! lector-icons: file icons and UI icons for Lector as embedded SVG bytes.
//!
//! * [`FileIcons`] maps file and folder names to colored
//!   [Material Icon Theme](https://github.com/material-extensions/vscode-material-icon-theme)
//!   icons (MIT).
//! * [`UiIcon`] / [`ui_icon`] provide monochrome [Lucide](https://lucide.dev)
//!   icons (ISC) that the UI tints with the current text color.
//!
//! See `THIRD_PARTY.md` for licenses.
#![forbid(unsafe_code)]

#[rustfmt::skip]
mod generated;
mod ui;

pub use ui::{ui_icon, UiIcon};

use generated as g;

/// A resolved icon: its Material name and SVG bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct IconRef {
    /// Icon name (Material file stem), stable and suitable as a cache key.
    pub name: &'static str,
    /// SVG document bytes.
    pub svg: &'static [u8],
}

/// Material Icon Theme mapping for files and folders.
#[derive(Debug)]
pub struct FileIcons {
    _private: (),
}

static FILE_ICONS: FileIcons = FileIcons { _private: () };

fn lookup(table: &[(&str, u16)], key: &str) -> Option<u16> {
    table
        .binary_search_by(|(k, _)| (*k).cmp(key))
        .ok()
        .map(|ix| table[ix].1)
}

fn icon(ix: u16) -> IconRef {
    let (name, svg) = g::ICONS[ix as usize];
    IconRef { name, svg }
}

/// Extension candidates of a file name, longest first: `"a.test.tsx"` →
/// `["test.tsx", "tsx"]`. A leading dot (dotfile) is not an extension
/// separator.
fn extension_candidates(name: &str) -> impl Iterator<Item = &str> {
    let start = usize::from(name.starts_with('.'));
    let body = &name[start..];
    body.match_indices('.')
        .map(move |(ix, _)| &body[ix + 1..])
        .filter(|ext| !ext.is_empty())
}

impl FileIcons {
    /// The shared instance.
    pub fn get() -> &'static FileIcons {
        &FILE_ICONS
    }

    /// Icon for a file name (not a path). Precedence: exact file name
    /// (case-insensitive) → longest compound extension (`d.ts`, `test.tsx`)
    /// → extension → default file icon. `light` selects light-theme variants
    /// where Material provides them.
    pub fn for_file(&self, file_name: &str, light: bool) -> IconRef {
        let name = file_name.to_ascii_lowercase();
        let find = |dark: &[(&str, u16)], light_table: &[(&str, u16)], key: &str| {
            if light {
                if let Some(ix) = lookup(light_table, key) {
                    return Some(ix);
                }
            }
            lookup(dark, key)
        };
        if let Some(ix) = find(g::FILE_NAMES, g::LIGHT_FILE_NAMES, &name) {
            return icon(ix);
        }
        for ext in extension_candidates(&name) {
            if let Some(ix) = find(g::FILE_EXTENSIONS, g::LIGHT_FILE_EXTENSIONS, ext) {
                return icon(ix);
            }
        }
        icon(g::DEFAULT_FILE)
    }

    /// Icon for a folder name. `root` selects the project-root icon.
    pub fn for_folder(
        &self,
        folder_name: &str,
        expanded: bool,
        root: bool,
        light: bool,
    ) -> IconRef {
        if root {
            return icon(if expanded {
                g::ROOT_FOLDER_EXPANDED
            } else {
                g::ROOT_FOLDER
            });
        }
        let name = folder_name.to_ascii_lowercase();
        let (dark_table, light_table, default) = if expanded {
            (
                g::FOLDER_NAMES_EXPANDED,
                g::LIGHT_FOLDER_NAMES_EXPANDED,
                g::DEFAULT_FOLDER_EXPANDED,
            )
        } else {
            (g::FOLDER_NAMES, g::LIGHT_FOLDER_NAMES, g::DEFAULT_FOLDER)
        };
        if light {
            if let Some(ix) = lookup(light_table, &name) {
                return icon(ix);
            }
        }
        icon(lookup(dark_table, &name).unwrap_or(default))
    }

    /// Icon by Material name (e.g. `"rust"`), for callers that cached a name.
    pub fn by_name(&self, name: &str) -> Option<IconRef> {
        g::ICONS
            .binary_search_by(|(n, _)| (*n).cmp(name))
            .ok()
            .map(|ix| icon(ix as u16))
    }

    /// The default file icon.
    pub fn default_file(&self) -> IconRef {
        icon(g::DEFAULT_FILE)
    }

    /// Number of embedded file/folder icons.
    pub fn len(&self) -> usize {
        g::ICONS.len()
    }

    /// Always false; present for API symmetry with `len`.
    pub fn is_empty(&self) -> bool {
        g::ICONS.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str) -> &'static str {
        FileIcons::get().for_file(name, false).name
    }

    #[test]
    fn tables_are_sorted() {
        for table in [
            g::FILE_NAMES,
            g::FILE_EXTENSIONS,
            g::FOLDER_NAMES,
            g::FOLDER_NAMES_EXPANDED,
            g::LIGHT_FILE_NAMES,
            g::LIGHT_FILE_EXTENSIONS,
            g::LIGHT_FOLDER_NAMES,
            g::LIGHT_FOLDER_NAMES_EXPANDED,
        ] {
            assert!(table.windows(2).all(|w| w[0].0 < w[1].0));
            assert!(table.iter().all(|(_, ix)| (*ix as usize) < g::ICONS.len()));
        }
        assert!(g::ICONS.windows(2).all(|w| w[0].0 < w[1].0));
    }

    #[test]
    fn extension_mapping() {
        assert_eq!(file("main.rs"), "rust");
        assert_eq!(file("index.ts"), "typescript");
        assert_eq!(file("App.tsx"), "react_ts");
        assert_eq!(file("README.MD"), "readme");
        assert_eq!(file("notes.md"), "markdown");
        assert_eq!(file("x.unknownext"), "file");
        assert_eq!(file("noextension"), "file");
        assert_eq!(file("trailingdot."), "file");
    }

    #[test]
    fn compound_extensions_beat_simple_ones() {
        assert_eq!(file("types.d.ts"), "typescript-def");
        assert_eq!(file("button.spec.ts"), "test-ts");
        assert_eq!(file("button.test.ts"), "test-ts");
        assert_ne!(file("button.test.tsx"), "react_ts");
    }

    #[test]
    fn exact_name_beats_extension_case_insensitive() {
        assert_eq!(file("Dockerfile"), "docker");
        assert_eq!(file(".gitignore"), "git");
        assert_eq!(file("LICENSE"), "license");
        assert_eq!(file("license"), "license");
        assert_eq!(
            file("Cargo.lock"),
            FileIcons::get().for_file("cargo.lock", false).name
        );
        // `package.json` is an exact name, more specific than `.json`.
        assert_ne!(file("package.json"), file("other.json"));
    }

    #[test]
    fn dotfiles_use_their_name_as_extension_only_after_first_dot() {
        assert_eq!(
            extension_candidates(".eslintrc.json").collect::<Vec<_>>(),
            ["json"]
        );
        assert_eq!(
            extension_candidates("a.b.c").collect::<Vec<_>>(),
            ["b.c", "c"]
        );
        assert!(extension_candidates(".bashrc").next().is_none());
    }

    #[test]
    fn light_variants() {
        let icons = FileIcons::get();
        assert_eq!(icons.for_file("Cargo.toml", true).name, "toml_light");
        assert_eq!(icons.for_file("Cargo.toml", false).name, "toml");
        // Falls back to the dark table when there is no light override.
        assert_eq!(icons.for_file("main.rs", true).name, "rust");
    }

    #[test]
    fn folders() {
        let icons = FileIcons::get();
        assert_eq!(
            icons.for_folder("src", false, false, false).name,
            "folder-src"
        );
        assert_eq!(
            icons.for_folder("SRC", true, false, false).name,
            "folder-src-open"
        );
        assert_eq!(icons.for_folder("zzzz", false, false, false).name, "folder");
        assert_eq!(
            icons.for_folder("zzzz", true, false, false).name,
            "folder-open"
        );
        assert_eq!(
            icons.for_folder("anything", false, true, false).name,
            "folder-root"
        );
        assert_eq!(
            icons.for_folder("anything", true, true, false).name,
            "folder-root-open"
        );
    }

    #[test]
    fn svgs_are_embedded() {
        let icons = FileIcons::get();
        assert!(icons.len() > 1000);
        let rust = icons.for_file("lib.rs", false);
        assert!(rust.svg.starts_with(b"<svg"));
        assert_eq!(icons.by_name("rust"), Some(rust));
        for icon in UiIcon::ALL {
            let svg = std::str::from_utf8(icon.svg()).unwrap();
            assert!(
                svg.starts_with("<svg") && svg.contains("currentColor"),
                "{icon:?}"
            );
            assert_eq!(UiIcon::from_name(icon.name()), Some(*icon));
            assert_eq!(ui_icon(*icon), icon.svg());
        }
    }
}
