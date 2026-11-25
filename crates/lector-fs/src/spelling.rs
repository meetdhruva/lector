//! The on-disk spelling of a path: on case-insensitive or normalising file systems (macOS,
//! Windows, Linux casefold directories) `README.md`, `readme.md` and the NFC and NFD forms of
//! `café` can all name one file. Anything that keys state by path (an agent turn's backups,
//! ADR 0011) must key it by the name the file system has for the file, not by the spelling a
//! caller sent, or one file gets two identities.

use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Looks up the name a directory entry has on disk.
pub trait EntryNames {
    /// The name, as stored in `dir`, of the entry that `name` reaches; `None` when `name` reaches
    /// nothing there.
    fn on_disk(&self, dir: &Path, name: &OsStr) -> Option<OsString>;
}

/// [`EntryNames`] of the real file system. On Unix the entry is found by identity: the requested
/// spelling when the directory lists it, else the listed entry with the same inode (so a
/// case-folded or normalised spelling maps to the stored one). Elsewhere (Windows) the canonical
/// path's file name, which the system returns as stored (and with 8.3 short names expanded).
#[derive(Clone, Copy, Debug, Default)]
pub struct FileSystemNames;

impl EntryNames for FileSystemNames {
    fn on_disk(&self, dir: &Path, name: &OsStr) -> Option<OsString> {
        let path = dir.join(name);
        let meta = std::fs::symlink_metadata(&path).ok()?;
        Some(stored_name(dir, name, &path, &meta).unwrap_or_else(|| name.to_os_string()))
    }
}

#[cfg(unix)]
fn stored_name(
    dir: &Path,
    name: &OsStr,
    _path: &Path,
    meta: &std::fs::Metadata,
) -> Option<OsString> {
    use std::os::unix::fs::{DirEntryExt, MetadataExt};
    let mut same_inode = None;
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let entry_name = entry.file_name();
        if entry_name == name {
            return Some(entry_name);
        }
        if same_inode.is_none() && entry.ino() == meta.ino() {
            same_inode = Some(entry_name);
        }
    }
    same_inode
}

#[cfg(not(unix))]
fn stored_name(
    _dir: &Path,
    _name: &OsStr,
    path: &Path,
    _meta: &std::fs::Metadata,
) -> Option<OsString> {
    dunce::canonicalize(path)
        .ok()?
        .file_name()
        .map(OsStr::to_os_string)
}

/// The on-disk spelling of the `/`-separated relative path `rel` below `root`: every component
/// that exists is replaced by the name its entry has on disk ([`EntryNames::on_disk`]); from the
/// first component that does not exist on, the rest is kept as given (a file about to be
/// created has no stored name yet: take the key again after creating it). Looks only at the
/// directories on the path, never below them.
pub fn on_disk_relative(root: &Path, rel: &str, names: &dyn EntryNames) -> String {
    let mut dir = root.to_path_buf();
    let mut out: Vec<String> = Vec::new();
    let mut missing = false;
    for part in rel.split('/').filter(|p| !p.is_empty() && *p != ".") {
        if !missing {
            if let Some(stored) = names.on_disk(&dir, OsStr::new(part)) {
                let stored = stored.to_string_lossy().into_owned();
                dir.push(&stored);
                out.push(stored);
                continue;
            }
            missing = true;
        }
        out.push(part.to_owned());
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// A fake case-insensitive, normalisation-insensitive file system: stored names per directory.
    struct Folding(BTreeMap<PathBuf, Vec<&'static str>>);

    fn fold(s: &str) -> String {
        // NFD `e` + combining acute to NFC `é`, then lower case: enough for the tests.
        s.replace("e\u{301}", "\u{e9}").to_lowercase()
    }

    impl EntryNames for Folding {
        fn on_disk(&self, dir: &Path, name: &OsStr) -> Option<OsString> {
            let wanted = fold(&name.to_string_lossy());
            self.0
                .get(dir)?
                .iter()
                .find(|n| fold(n) == wanted)
                .map(OsString::from)
        }
    }

    fn fs() -> Folding {
        let root = PathBuf::from("/r");
        Folding(BTreeMap::from([
            (root.clone(), vec!["Docs", "Readme.md", "caf\u{e9}.txt"]),
            (root.join("Docs"), vec!["Notes.TXT"]),
        ]))
    }

    #[test]
    fn existing_components_take_their_stored_spelling() {
        let r = Path::new("/r");
        assert_eq!(on_disk_relative(r, "README.md", &fs()), "Readme.md");
        assert_eq!(on_disk_relative(r, "readme.md", &fs()), "Readme.md");
        assert_eq!(
            on_disk_relative(r, "docs/notes.txt", &fs()),
            "Docs/Notes.TXT"
        );
        // NFD request, NFC stored.
        assert_eq!(
            on_disk_relative(r, "cafe\u{301}.txt", &fs()),
            "caf\u{e9}.txt"
        );
    }

    #[test]
    fn missing_components_keep_the_requested_spelling() {
        let r = Path::new("/r");
        assert_eq!(on_disk_relative(r, "New.txt", &fs()), "New.txt");
        assert_eq!(
            on_disk_relative(r, "DOCS/New/Deep.txt", &fs()),
            "Docs/New/Deep.txt"
        );
        assert_eq!(
            on_disk_relative(r, "./docs//NOTES.txt", &fs()),
            "Docs/Notes.TXT"
        );
    }

    #[test]
    fn the_real_file_system_keeps_or_finds_the_stored_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::create_dir(root.join("Docs")).unwrap();
        std::fs::write(root.join("Docs/Readme.md"), "x").unwrap();
        let names = FileSystemNames;
        assert_eq!(
            on_disk_relative(&root, "Docs/Readme.md", &names),
            "Docs/Readme.md"
        );
        assert_eq!(
            on_disk_relative(&root, "Docs/new.md", &names),
            "Docs/new.md"
        );
        let insensitive = root.join("DOCS/README.MD").exists();
        println!("case-insensitive file system: {insensitive}");
        let upper = on_disk_relative(&root, "DOCS/README.MD", &names);
        if insensitive {
            assert_eq!(upper, "Docs/Readme.md");
        } else {
            assert_eq!(upper, "DOCS/README.MD", "a different, missing path");
        }
    }
}
