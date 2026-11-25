//! The on-disk spelling of a path: on case-insensitive or normalising file systems (macOS,
//! Windows, Linux casefold directories) `README.md`, `readme.md` and the NFC and NFD forms of
//! `café` can all name one file. Anything that keys state by path (an agent turn's backups,
//! ADR 0011) must key it by the name the file system has for the file, not by the spelling a
//! caller sent, or one file gets two identities. Hard links are other names of a file, not
//! other spellings of a name: a request never resolves to another link's name.

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
    let entries = std::fs::read_dir(dir)
        .ok()?
        .flatten()
        .map(|e| (e.file_name(), e.ino()));
    choose_stored(name, entries, meta.ino())
}

/// The stored name of the entry that `requested` reached, given the directory's entries and the
/// inode it reached. The requested name itself when the directory lists it. Otherwise the entries
/// with that inode are names of the reached file, but with hard links several are, and only one
/// is the entry the file system resolved (an atomic write replaces that entry, not the other
/// links): with one such entry it is that one; with several, the one whose name is equivalent to
/// the request under a conservative folding (Unicode lower case after NFD, recomposed to NFC);
/// when none or several are, `None`: the requested spelling is kept (it reaches the right file;
/// the worst case is a spelling that differs from the stored one), never another link's name.
#[cfg(any(unix, test))]
fn choose_stored(
    requested: &OsStr,
    entries: impl IntoIterator<Item = (OsString, u64)>,
    ino: u64,
) -> Option<OsString> {
    let mut same_inode: Vec<OsString> = Vec::new();
    for (entry_name, entry_ino) in entries {
        if entry_name == requested {
            return Some(entry_name);
        }
        if entry_ino == ino {
            same_inode.push(entry_name);
        }
    }
    if same_inode.len() <= 1 {
        return same_inode.pop();
    }
    let wanted = fold(&requested.to_string_lossy());
    let mut equivalent = same_inode
        .into_iter()
        .filter(|n| fold(&n.to_string_lossy()) == wanted);
    match (equivalent.next(), equivalent.next()) {
        (Some(one), None) => Some(one),
        _ => None,
    }
}

/// Conservative name folding for [`choose_stored`]: canonical decomposition, lower case,
/// canonical composition.
#[cfg(any(unix, test))]
fn fold(name: &str) -> String {
    use unicode_normalization::UnicodeNormalization;
    name.nfd()
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .nfc()
        .collect()
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

    /// On CI for macOS and Windows the temp directory must be case-insensitive (and on macOS,
    /// APFS, insensitive to Unicode normalisation on lookup): if a runner's is not, this fails
    /// loudly instead of silently running the case-sensitive branch.
    fn require_ci_folding(dir: &Path, case: bool, normalising: Option<bool>) {
        if std::env::var_os("CI").is_none() {
            return;
        }
        let os = std::env::consts::OS;
        if cfg!(any(target_os = "macos", target_os = "windows")) {
            assert!(
                case,
                "CI on {os}: {} is case-sensitive, so the case-insensitive branch would not run",
                dir.display()
            );
        }
        if cfg!(target_os = "macos") {
            if let Some(normalising) = normalising {
                assert!(
                    normalising,
                    "CI on macOS: {} does not fold Unicode normalisation (APFS does)",
                    dir.display()
                );
            }
        }
    }

    fn listing(entries: &[(&str, u64)]) -> Vec<(OsString, u64)> {
        entries
            .iter()
            .map(|(n, i)| (OsString::from(n), *i))
            .collect()
    }

    /// Codex finding: hard links share an inode, so "the first entry with the requested inode"
    /// can be another name of the file (`backup.txt`), which the write then does not replace.
    #[test]
    fn among_hard_links_the_name_the_request_folds_to_is_chosen() {
        let pick = |req: &str, entries: &[(&str, u64)], ino| {
            choose_stored(OsStr::new(req), listing(entries), ino)
        };
        // The wrong link enumerates first.
        assert_eq!(
            pick(
                "README.txt",
                &[("backup.txt", 7), ("Readme.txt", 7), ("x", 8)],
                7
            ),
            Some(OsString::from("Readme.txt"))
        );
        // A link whose other name is a case variant of a third file (another inode).
        assert_eq!(
            pick(
                "data.txt",
                &[("notes.txt", 5), ("DATA.TXT", 6), ("Data.txt", 5)],
                5
            ),
            Some(OsString::from("Data.txt"))
        );
        // NFD request, NFC stored, next to another link.
        assert_eq!(
            pick("CAFE\u{301}", &[("copy", 9), ("caf\u{e9}", 9)], 9),
            Some(OsString::from("caf\u{e9}"))
        );
        // The exact name wins.
        assert_eq!(
            pick("Readme.txt", &[("README.TXT", 1), ("Readme.txt", 1)], 1),
            Some(OsString::from("Readme.txt"))
        );
        // One entry with the inode: it is the entry the request reached, whatever the folding.
        assert_eq!(
            pick("stra\u{df}e", &[("STRASSE", 4), ("y", 2)], 4),
            Some(OsString::from("STRASSE"))
        );
        // Several links and none folds to the request: no guess (keep the requested spelling).
        assert_eq!(pick("stra\u{df}e", &[("STRASSE", 3), ("b", 3)], 3), None);
    }

    /// On the real file system: hard links keep their own names, and a case variant of one of
    /// them never resolves to the other link.
    #[test]
    fn hard_links_on_the_real_file_system_keep_their_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(tmp.path()).unwrap();
        std::fs::write(root.join("backup.txt"), "x").unwrap();
        std::fs::hard_link(root.join("backup.txt"), root.join("Readme.txt")).unwrap();
        let names = FileSystemNames;
        assert_eq!(on_disk_relative(&root, "Readme.txt", &names), "Readme.txt");
        assert_eq!(on_disk_relative(&root, "backup.txt", &names), "backup.txt");
        let insensitive = root.join("README.TXT").exists();
        println!("case-insensitive file system: {insensitive}");
        require_ci_folding(&root, insensitive, None);
        let upper = on_disk_relative(&root, "README.TXT", &names);
        if insensitive {
            assert_eq!(upper, "Readme.txt", "never the other link");
        } else {
            assert_eq!(upper, "README.TXT", "a different, missing path");
        }
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
        require_ci_folding(&root, insensitive, None);
        let upper = on_disk_relative(&root, "DOCS/README.MD", &names);
        if insensitive {
            assert_eq!(upper, "Docs/Readme.md");
        } else {
            assert_eq!(upper, "DOCS/README.MD", "a different, missing path");
        }
    }
}
