//! Shadow repositories of folders that are not git repositories (ADR 0011): created lazily and
//! privately under the data directory, keyed by the folder's canonical path, path-scoped only,
//! byte-exact, and never running git in (or above) the folder in a way that reads its settings.

use std::fs;
use std::path::PathBuf;
use std::sync::Once;
use std::time::Duration;

use lector_git::shadow::{Oversize, SnapshotEntry, SnapshotKind, FOLDER_MAX_FILE};
use lector_git::ShadowRepo;

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    });
}

struct Fx {
    _tmp: tempfile::TempDir,
    folder: PathBuf,
    data: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        Fx::inside(None)
    }

    /// A folder at `<tmp>/<sub>/folder` (so `<tmp>/<sub>` can be made a repository).
    fn inside(sub: Option<&str>) -> Fx {
        hermetic();
        let tmp = tempfile::tempdir().unwrap();
        let base = dunce::canonicalize(tmp.path()).unwrap();
        let folder = match sub {
            Some(s) => base.join(s).join("folder"),
            None => base.join("folder"),
        };
        fs::create_dir_all(&folder).unwrap();
        Fx {
            data: base.join("data"),
            folder,
            _tmp: tmp,
        }
    }

    fn write(&self, rel: &str, content: impl AsRef<[u8]>) {
        let p = self.folder.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(p, content).unwrap();
    }

    fn read(&self, rel: &str) -> Option<Vec<u8>> {
        fs::read(self.folder.join(rel)).ok()
    }

    fn shadow(&self) -> ShadowRepo {
        ShadowRepo::open_folder(&self.data, &self.folder).unwrap()
    }
}

#[cfg(unix)]
fn mode(path: &std::path::Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_folder_shadow_appears_at_first_use_private_and_outside_the_folder() {
    let fx = Fx::new();
    fx.write("a.txt", "one\n");
    assert!(ShadowRepo::existing_folder(&fx.data, &fx.folder).is_none());
    assert!(!fx.data.exists(), "looking for a shadow creates nothing");

    let shadow = fx.shadow();
    assert!(shadow.is_folder());
    assert_eq!(shadow.workdir(), fx.folder);
    assert_eq!(
        shadow.git_dir(),
        ShadowRepo::path_for(&fx.data, &fx.folder),
        "keyed by the folder's canonical path, like a repository's shadow"
    );
    assert!(shadow.git_dir().starts_with(fx.data.join("shadow")));
    assert!(!fx.folder.join(".git").exists(), "the folder gains no .git");
    assert_eq!(
        fs::read_dir(&fx.folder).unwrap().count(),
        1,
        "nothing new in the folder"
    );
    assert!(
        !shadow.git_dir().join("objects/info/alternates").exists(),
        "no alternates: there is no user repository"
    );
    let config = fs::read_to_string(shadow.git_dir().join("config")).unwrap();
    assert!(
        config.contains("sharedRepository = 0600") || config.contains("sharedrepository = 0600")
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(shadow.git_dir()), 0o700);
        assert_eq!(mode(&fx.data.join("shadow")), 0o700);
    }
    let found = ShadowRepo::existing_folder(&fx.data, &fx.folder).expect("exists now");
    assert_eq!(found.git_dir(), shadow.git_dir());
    assert!(found.is_folder());
}

#[test]
fn folder_snapshots_restore_bytes_modes_links_and_absence() {
    let fx = Fx::new();
    fx.write("a.txt", "one\r\ntwo\n");
    fx.write("bin.dat", [0u8, 1, 2, 255, 13, 10]);
    fx.write("run.sh", "#!/bin/sh\necho hi\n");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fx.folder.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        // A link to something outside the folder: recorded as a link, never followed.
        std::os::unix::fs::symlink("/etc/hostname", fx.folder.join("link")).unwrap();
    }
    let shadow = fx.shadow();
    let paths = ["a.txt", "bin.dat", "run.sh", "link", "new/dir/x.txt"];
    let snap = shadow.snapshot_paths(&paths, "Before").unwrap();
    assert_eq!(shadow.kind(&snap).unwrap(), SnapshotKind::Paths);

    fx.write("a.txt", "changed");
    fx.write("bin.dat", "text now");
    fx.write("new/dir/x.txt", "created by the turn");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(fx.folder.join("run.sh"), fs::Permissions::from_mode(0o644)).unwrap();
        fs::remove_file(fx.folder.join("link")).unwrap();
        fx.write("link", "a regular file now");
    }
    let before = shadow.fingerprints(&paths).unwrap();
    shadow.restore_paths(&snap, &paths).unwrap();

    assert_eq!(fx.read("a.txt").unwrap(), b"one\r\ntwo\n");
    assert_eq!(fx.read("bin.dat").unwrap(), [0u8, 1, 2, 255, 13, 10]);
    assert!(fx.read("new/dir/x.txt").is_none(), "absent -> removed");
    assert!(
        !fx.folder.join("new").exists(),
        "emptied directories go too"
    );
    #[cfg(unix)]
    {
        assert_eq!(mode(&fx.folder.join("run.sh")) & 0o111, 0o111);
        assert_eq!(
            fs::read_link(fx.folder.join("link")).unwrap(),
            std::path::Path::new("/etc/hostname")
        );
    }
    assert_ne!(shadow.fingerprints(&paths).unwrap(), before);
    assert!(!fx.folder.join(".git").exists());
}

#[test]
fn folder_shadows_refuse_whole_tree_and_index_operations() {
    let fx = Fx::new();
    fx.write("a.txt", "a");
    let shadow = fx.shadow();
    assert!(shadow.snapshot_worktree("all").is_err());
    assert!(shadow.snapshot_index(&["a.txt"], "index").is_err());
    assert!(shadow.index_fingerprints(&["a.txt"]).is_err());
    assert!(
        !shadow.is_ignored("a.txt"),
        "nothing is ignored without git"
    );
    // Nothing was recorded by the refused calls.
    assert!(shadow.snapshots().unwrap().is_empty());
}

/// The folder lies inside another repository whose configuration is broken: any git command that
/// discovered that repository (instead of using the shadow explicitly) would fail on it.
#[test]
fn folder_shadow_commands_never_discover_a_repository_above_the_folder() {
    let fx = Fx::inside(Some("outer"));
    let outer = fx.folder.parent().unwrap().to_path_buf();
    fs::create_dir_all(outer.join(".git/objects")).unwrap();
    fs::create_dir_all(outer.join(".git/refs")).unwrap();
    fs::write(outer.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    fs::write(
        outer.join(".git/config"),
        "[core\n this is not a config file\n",
    )
    .unwrap();
    fx.write("a.txt", "mine");
    let shadow = fx.shadow();
    let snap = shadow.snapshot_paths(&["a.txt", "b.txt"], "s").unwrap();
    fx.write("a.txt", "theirs");
    fx.write("b.txt", "new");
    let prints = shadow.fingerprints(&["a.txt", "b.txt"]).unwrap();
    assert_eq!(prints.len(), 2);
    let after = shadow.snapshot_paths(&["a.txt", "b.txt"], "after").unwrap();
    assert_eq!(shadow.changes_between(&snap, &after).unwrap().len(), 2);
    shadow.restore_paths(&snap, &["a.txt", "b.txt"]).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), b"mine");
    assert!(fx.read("b.txt").is_none());
    shadow.prune(Duration::from_secs(3600)).unwrap();
}

/// `.gitattributes` in the folder cannot run programs or change bytes: the shadow is bare (no work
/// tree, so the folder's attributes are never read) and hashes without filters. The drivers are
/// defined in the shadow's own configuration, the only configuration its commands read, so they
/// would run if the attributes were honoured.
#[test]
fn folder_attributes_cannot_run_filters_or_convert_line_endings() {
    let fx = Fx::new();
    let marker = fx.folder.parent().unwrap().join("ran");
    fx.write(".gitattributes", "* filter=evil diff=evil text eol=crlf\n");
    fx.write("a.txt", "lf\nonly\n");
    let shadow = fx.shadow();
    let cmd = format!("sh -c 'touch {}; cat'", marker.display());
    for (key, value) in [
        ("filter.evil.clean", cmd.as_str()),
        ("filter.evil.smudge", cmd.as_str()),
        ("diff.evil.textconv", cmd.as_str()),
    ] {
        let out = std::process::Command::new("git")
            .arg("--git-dir")
            .arg(shadow.git_dir())
            .args(["config", key, value])
            .output()
            .unwrap();
        assert!(out.status.success());
    }
    let snap = shadow.snapshot_paths(&["a.txt"], "s").unwrap();
    fx.write("a.txt", "x\n");
    shadow.fingerprints(&["a.txt"]).unwrap();
    let after = shadow.snapshot_paths(&["a.txt"], "after").unwrap();
    shadow.changes_between(&snap, &after).unwrap();
    shadow.restore_paths(&snap, &["a.txt"]).unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), b"lf\nonly\n");
    assert_eq!(
        shadow.file_at(&snap, "a.txt").unwrap().unwrap(),
        b"lf\nonly\n"
    );
    assert!(!marker.exists(), "a filter or textconv driver ran");
}

#[test]
fn merged_snapshots_keep_the_first_record_of_each_path() {
    let fx = Fx::new();
    fx.write("a.txt", "a1");
    let shadow = fx.shadow();
    let first = shadow.snapshot_paths(&["a.txt"], "a").unwrap();
    fx.write("a.txt", "a2");
    fx.write("b.txt", "b1");
    let second = shadow
        .snapshot_paths(&["a.txt", "b.txt", "c.txt"], "b")
        .unwrap();
    let merged = shadow
        .merge_snapshots(&[&first, &second], "Before the turn")
        .unwrap();
    assert_eq!(merged.note, "Before the turn");
    assert_eq!(shadow.kind(&merged).unwrap(), SnapshotKind::Paths);
    assert_eq!(shadow.file_at(&merged, "a.txt").unwrap().unwrap(), b"a1");
    assert_eq!(shadow.file_at(&merged, "b.txt").unwrap().unwrap(), b"b1");
    fx.write("c.txt", "appeared");
    shadow
        .restore_paths(&merged, &["a.txt", "b.txt", "c.txt"])
        .unwrap();
    assert_eq!(fx.read("a.txt").unwrap(), b"a1");
    assert!(fx.read("c.txt").is_none(), "absent records carry over");
    assert!(shadow.merge_snapshots(&[], "nothing").is_err());
}

#[test]
fn orphaned_shadows_are_pruned_by_age_and_live_ones_are_kept() {
    let fx = Fx::new();
    fx.write("a.txt", "a");
    let shadow = fx.shadow();
    shadow.snapshot_paths(&["a.txt"], "s").unwrap();
    let live = Fx::new();
    live.write("b.txt", "b");
    let live_shadow = ShadowRepo::open_folder(&fx.data, &live.folder).unwrap();
    live_shadow.snapshot_paths(&["b.txt"], "s").unwrap();

    // The folder still exists: nothing is removed, whatever the age.
    assert_eq!(
        ShadowRepo::prune_orphans(&fx.data, Duration::ZERO).unwrap(),
        0
    );
    fs::remove_dir_all(&fx.folder).unwrap();
    // Deleted (or renamed) folder: kept while its newest backup is younger than the policy...
    assert_eq!(
        ShadowRepo::prune_orphans(&fx.data, Duration::from_secs(3600)).unwrap(),
        0
    );
    assert!(shadow.git_dir().exists());
    // ...and removed once it is older.
    assert_eq!(
        ShadowRepo::prune_orphans(&fx.data, Duration::ZERO).unwrap(),
        1
    );
    assert!(!shadow.git_dir().exists());
    assert!(live_shadow.git_dir().exists());
    assert!(ShadowRepo::prune_orphans(&fx.data.join("missing"), Duration::ZERO).is_ok());
}

/// A sparse file of `len` bytes (no data blocks: creating it reads and writes nothing).
fn sparse(path: &std::path::Path, len: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::File::create(path).unwrap().set_len(len).unwrap();
}

/// Size of the shadow's object store: what a snapshot copied.
fn stored(shadow: &ShadowRepo) -> u64 {
    fn walk(dir: &std::path::Path) -> u64 {
        fs::read_dir(dir)
            .map(|d| {
                d.filter_map(|e| e.ok())
                    .map(|e| match e.metadata() {
                        Ok(m) if m.is_dir() => walk(&e.path()),
                        Ok(m) => m.len(),
                        Err(_) => 0,
                    })
                    .sum()
            })
            .unwrap_or(0)
    }
    walk(&shadow.git_dir().join("objects"))
}

/// Path-scoped means files: a folder shadow never walks a directory it is handed (it may be
/// `Documents` or a home directory), and never opens a FIFO, socket or device.
#[test]
fn folder_snapshots_refuse_directories_and_special_files() {
    let fx = Fx::new();
    sparse(&fx.folder.join("Documents/big.iso"), 65 * 1024 * 1024);
    fx.write("Documents/note.txt", "n");
    let shadow = fx.shadow();
    let before = stored(&shadow);
    let err = shadow
        .snapshot_paths(&["Documents"], "dir")
        .unwrap_err()
        .to_string();
    assert!(err.contains("not a regular file"), "{err}");
    assert!(shadow
        .snapshot_files(&["Documents"], u64::MAX, Oversize::Refuse, "dir")
        .is_err());
    #[cfg(unix)]
    {
        let fifo = fx.folder.join("pipe");
        let ok = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success());
        if ok {
            assert!(shadow.snapshot_paths(&["pipe"], "fifo").is_err());
        }
    }
    assert!(shadow.snapshots().unwrap().is_empty(), "nothing recorded");
    assert!(stored(&shadow) <= before, "nothing copied");
    // The same holds for any shadow through `snapshot_files` (the repository case uses it for
    // git-ignored files): a directory is refused, or with `SkipAll` recorded as skipped, unread.
    assert!(shadow
        .snapshot_files(&["Documents"], u64::MAX, Oversize::Skip, "dir")
        .is_err());
    let snap = shadow
        .snapshot_files(&["Documents", "a.txt"], u64::MAX, Oversize::SkipAll, "skip")
        .unwrap();
    let entries = shadow.entries(&snap).unwrap();
    assert!(entries.contains(&SnapshotEntry::Skipped {
        path: "Documents".into()
    }));
    assert!(
        stored(&shadow) < before + 1024 * 1024,
        "the directory was not copied"
    );
    // Fingerprints do not expand a directory of a folder shadow either.
    let prints = shadow.fingerprints(&["Documents"]).unwrap();
    assert_eq!(prints.len(), 1, "{prints:?}");
}

/// The 64 MiB rule inside the shadow: a folder shadow records a larger file as skipped without
/// storing it (`Refuse` fails instead), a restore of a skipped path refuses with the reason, and
/// a comparison reports the file as modified, not deleted.
#[test]
fn files_over_the_limit_are_never_stored_by_a_folder_shadow() {
    let fx = Fx::new();
    fx.write("log.txt", "small\n");
    let shadow = fx.shadow();
    let small = shadow.snapshot_paths(&["log.txt"], "before").unwrap();
    sparse(&fx.folder.join("log.txt"), FOLDER_MAX_FILE + 1);
    let before = stored(&shadow);
    let err = shadow
        .snapshot_files(&["log.txt"], FOLDER_MAX_FILE, Oversize::Refuse, "x")
        .unwrap_err()
        .to_string();
    assert!(err.contains("64 MiB"), "{err}");
    let big = shadow.snapshot_paths(&["log.txt"], "after").unwrap();
    assert!(
        stored(&shadow) < before + 1024 * 1024,
        "the large file was stored"
    );
    assert_eq!(
        shadow.entries(&big).unwrap(),
        vec![SnapshotEntry::Skipped {
            path: "log.txt".into()
        }]
    );
    let changes = shadow.changes_between(&small, &big).unwrap();
    assert_eq!(changes.len(), 1);
    assert_eq!(changes[0].kind, lector_git::ChangeKind::Modified);
    let err = shadow
        .restore_paths(&big, &["log.txt"])
        .unwrap_err()
        .to_string();
    assert!(err.contains("not backed up"), "{err}");
    assert_eq!(
        fs::metadata(fx.folder.join("log.txt")).unwrap().len(),
        FOLDER_MAX_FILE + 1,
        "nothing written"
    );
    shadow.restore_paths(&small, &["log.txt"]).unwrap();
    assert_eq!(fx.read("log.txt").unwrap(), b"small\n");
}
