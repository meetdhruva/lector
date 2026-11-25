//! The before x after state table of an agent turn (see `ShadowRepo::changes_between`), proven
//! for both kinds of turn snapshot: the path-scoped first-touch snapshots of a folder without git
//! (limit `FOLDER_MAX_FILE`) and the whole work-tree checkpoint of a repository (limit
//! `WORKTREE_MAX_FILE`). For every cell: what the comparison reports, what restoring the
//! before-state (the turn's undo) does, and what restoring the state backed up just before that
//! undo (the redo) does. A restore restores every requested path or fails naming the paths it
//! cannot restore; it never succeeds without writing the recorded state.

use std::fs;
use std::path::Path;
use std::process::Command;

use lector_git::shadow::{Oversize, FOLDER_MAX_FILE, WORKTREE_MAX_FILE};
use lector_git::ChangeKind::{self, Added, Deleted, Modified};
use lector_git::{Repo, ShadowRepo, ShadowSnapshot};

/// One path's state.
#[derive(Clone, Copy, Debug, PartialEq)]
enum St {
    Absent,
    File,
    /// A file over the kind's limit (recorded as skipped).
    Large,
    /// Not a regular file: in a folder a directory with a file in it (recorded as skipped); in a
    /// repository a FIFO (git records nothing for it, so the checkpoint sees it as absent; a
    /// directory there would be its files, which are other paths).
    Special,
}

const STATES: [St; 4] = [St::Absent, St::File, St::Large, St::Special];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Kind {
    /// A folder without git: first-touch `snapshot_files` (directories and large files skipped).
    Folder,
    /// A repository: `snapshot_worktree` checkpoints (a directory's files are other paths).
    Worktree,
}

impl Kind {
    fn limit(self) -> u64 {
        match self {
            Kind::Folder => FOLDER_MAX_FILE,
            Kind::Worktree => WORKTREE_MAX_FILE,
        }
    }

    /// How the snapshot sees the path itself: a work-tree snapshot records no FIFO, so the path
    /// is absent there.
    fn seen(self, st: St) -> St {
        match (self, st) {
            (Kind::Worktree, St::Special) => St::Absent,
            _ => st,
        }
    }
}

fn sparse(path: &Path, len: u64) {
    fs::File::create(path).unwrap().set_len(len).unwrap();
}

fn put(kind: Kind, root: &Path, rel: &str, st: St, text: &str, limit: u64) {
    let p = root.join(rel);
    if p.is_dir() {
        fs::remove_dir_all(&p).unwrap();
    } else if p.exists() {
        fs::remove_file(&p).unwrap();
    }
    match st {
        St::Absent => {}
        St::File => fs::write(&p, text).unwrap(),
        St::Large => sparse(&p, limit + 1),
        St::Special if kind == Kind::Folder => {
            fs::create_dir_all(&p).unwrap();
            fs::write(p.join("inside.txt"), text).unwrap();
        }
        St::Special => {
            let ok = Command::new("mkfifo")
                .arg(&p)
                .status()
                .is_ok_and(|s| s.success());
            assert!(ok, "mkfifo");
        }
    }
}

fn state_of(root: &Path, rel: &str, limit: u64) -> (St, Option<Vec<u8>>) {
    let p = root.join(rel);
    match fs::symlink_metadata(&p) {
        Err(_) => (St::Absent, None),
        Ok(m) if !m.is_file() => (St::Special, None),
        Ok(m) if m.len() > limit => (St::Large, None),
        Ok(_) => (St::File, fs::read(&p).ok()),
    }
}

/// Cells this platform cannot set up: a repository's "not a regular file" is a FIFO, and Windows
/// has none (a directory there is its files, other paths). They are skipped and listed.
fn unavailable(kind: Kind, b: St, a: St) -> bool {
    kind == Kind::Worktree && !cfg!(unix) && (b == St::Special || a == St::Special)
}

fn name(b: St, a: St) -> String {
    format!("{b:?}_{a:?}").to_lowercase()
}

fn run_table(kind: Kind) {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    let data = base.join("data");
    fs::create_dir_all(&root).unwrap();
    let limit = kind.limit();
    let all: Vec<String> = STATES
        .iter()
        .flat_map(|b| STATES.iter().map(move |a| name(*b, *a)))
        .collect();
    let refs: Vec<&str> = all.iter().map(String::as_str).collect();
    for b in STATES {
        for a in STATES {
            if !unavailable(kind, b, a) {
                put(kind, &root, &name(b, a), b, "before\n", limit);
            }
        }
    }
    let shadow = match kind {
        Kind::Folder => ShadowRepo::open_folder(&data, &root).unwrap(),
        Kind::Worktree => {
            let ok = Command::new("git")
                .args(["init", "-q"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success();
            assert!(ok);
            ShadowRepo::open(&data, &Repo::discover(&root).unwrap()).unwrap()
        }
    };
    let take = |note: &str| -> ShadowSnapshot {
        match kind {
            Kind::Folder => shadow
                .snapshot_files(&refs, limit, Oversize::SkipAll, note)
                .unwrap(),
            Kind::Worktree => shadow.snapshot_worktree(note).unwrap(),
        }
    };
    let before = take("before");
    for b in STATES {
        for a in STATES {
            if !unavailable(kind, b, a) {
                put(kind, &root, &name(b, a), a, "after\n", limit);
            }
        }
    }
    let after = take("after");
    let changes = shadow.changes_between(&before, &after).unwrap();
    let reported =
        |p: &str| -> Option<ChangeKind> { changes.iter().find(|c| c.path() == p).map(|c| c.kind) };

    let mut skipped: Vec<String> = Vec::new();
    for b in STATES {
        for a in STATES {
            let p = name(b, a);
            if unavailable(kind, b, a) {
                skipped.push(p);
                continue;
            }
            let (sb, sa) = (kind.seen(b), kind.seen(a));
            let expected = match (sb, sa) {
                (St::Absent, St::Absent) => None,
                (St::Absent, _) => Some(Added),
                (_, St::Absent) => Some(Deleted),
                (St::File, _) | (_, St::File) => Some(Modified),
                _ => None,
            };
            assert_eq!(reported(&p), expected, "{kind:?}: compare {p}");

            // Undo: the journal backs up the present state first (the redo), then restores.
            let backup = shadow.snapshot_paths(&[p.as_str()], "before undo");
            let undone = shadow.restore_paths(&before, &[p.as_str()]);
            if a == St::Special {
                // Not a regular file now: never removed or overwritten by a restore.
                assert!(
                    undone.is_err(),
                    "{kind:?} {p}: restored over a special file"
                );
                assert_eq!(state_of(&root, &p, limit).0, St::Special, "{kind:?} {p}");
                continue;
            }
            let backup = backup.unwrap_or_else(|e| panic!("{kind:?} {p}: backup {e}"));
            match sb {
                St::Large | St::Special => {
                    let err = undone.expect_err(&format!("{kind:?} {p}: a no-op undo"));
                    assert!(err.to_string().contains(&p), "{kind:?} {p}: {err}");
                    assert_eq!(state_of(&root, &p, limit).0, a, "{kind:?} {p}: untouched");
                    continue;
                }
                St::Absent => {
                    undone.unwrap_or_else(|e| panic!("{kind:?} {p}: {e}"));
                    assert_eq!(state_of(&root, &p, limit).0, St::Absent, "{kind:?} {p}");
                }
                St::File => {
                    undone.unwrap_or_else(|e| panic!("{kind:?} {p}: {e}"));
                    assert_eq!(
                        state_of(&root, &p, limit).1.as_deref(),
                        Some(&b"before\n"[..]),
                        "{kind:?} {p}"
                    );
                }
            }
            // Redo: restore what the undo backed up.
            let redone = shadow.restore_paths(&backup, &[p.as_str()]);
            match (a, kind) {
                // The folder shadow did not store it; a repository's path backup did.
                (St::Large, Kind::Folder) => {
                    let err = redone.unwrap_err().to_string();
                    assert!(err.contains("not backed up"), "{p}: {err}");
                }
                (St::Large, Kind::Worktree) => {
                    redone.unwrap();
                    assert_eq!(state_of(&root, &p, limit).0, St::Large, "{p}");
                }
                (St::Absent, _) => {
                    redone.unwrap();
                    assert_eq!(state_of(&root, &p, limit).0, St::Absent, "{kind:?} {p}");
                }
                (St::File, _) => {
                    redone.unwrap();
                    assert_eq!(
                        state_of(&root, &p, limit).1.as_deref(),
                        Some(&b"after\n"[..]),
                        "{kind:?} {p}"
                    );
                }
                (St::Special, _) => unreachable!(),
            }
        }
    }
    println!(
        "{kind:?}: {} of 16 cells proven; skipped (no FIFO on this platform): {skipped:?}",
        16 - skipped.len()
    );
}

#[test]
fn the_state_table_holds_for_folder_first_touch_snapshots() {
    run_table(Kind::Folder);
}

#[test]
fn the_state_table_holds_for_a_repository_worktree_checkpoint() {
    run_table(Kind::Worktree);
}

/// The restore invariant beyond the table: a merged snapshot's skipped entry fails the restore
/// like any other kind's, a parent that became a file blocks the restore, and either failure
/// names the path and writes nothing (the other requested file stays as it is).
#[test]
fn a_restore_fails_as_a_whole_for_skipped_entries_and_blocked_parents() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(root.join("d/x.txt"), "x\n").unwrap();
    sparse(&root.join("big.bin"), FOLDER_MAX_FILE + 1);
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let first = shadow
        .snapshot_files(
            &["a.txt", "d/x.txt"],
            FOLDER_MAX_FILE,
            Oversize::Refuse,
            "a",
        )
        .unwrap();
    let big = shadow
        .snapshot_files(&["big.bin"], FOLDER_MAX_FILE, Oversize::SkipAll, "b")
        .unwrap();
    let merged = shadow.merge_snapshots(&[&first, &big], "merged").unwrap();
    fs::write(root.join("a.txt"), "changed\n").unwrap();
    let err = shadow
        .restore_paths(&merged, &["a.txt", "big.bin"])
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("big.bin") && err.contains("nothing was restored"),
        "{err}"
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "changed\n");

    fs::remove_dir_all(root.join("d")).unwrap();
    fs::write(root.join("d"), "a file now").unwrap();
    let err = shadow
        .restore_paths(&first, &["a.txt", "d/x.txt"])
        .unwrap_err()
        .to_string();
    assert!(err.contains("d is now a file"), "{err}");
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "changed\n");
    assert_eq!(fs::read_to_string(root.join("d")).unwrap(), "a file now");
}

/// Self-audit d: a touched file whose parent became a symlink during the turn does not make the
/// end-of-turn record fail (which would drop the whole turn's undo): with `SkipAll` it is recorded
/// as skipped, unread, and a later restore of it refuses naming it.
#[cfg(unix)]
#[test]
fn a_parent_replaced_by_a_link_is_skipped_at_turn_end_not_fatal() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    let outside = base.join("outside");
    fs::create_dir_all(root.join("d")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(root.join("d/x.txt"), "x\n").unwrap();
    fs::write(root.join("a.txt"), "a\n").unwrap();
    fs::write(outside.join("x.txt"), "elsewhere\n").unwrap();
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let paths = ["a.txt", "d/x.txt"];
    let before = shadow
        .snapshot_files(&paths, FOLDER_MAX_FILE, Oversize::Refuse, "before")
        .unwrap();
    fs::remove_dir_all(root.join("d")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("d")).unwrap();
    fs::write(root.join("a.txt"), "agent\n").unwrap();
    let after = shadow
        .snapshot_files(&paths, FOLDER_MAX_FILE, Oversize::SkipAll, "after")
        .expect("the turn's end is recorded");
    let changed: Vec<String> = shadow
        .changes_between(&before, &after)
        .unwrap()
        .iter()
        .map(|c| c.path().to_owned())
        .collect();
    assert_eq!(changed, ["a.txt", "d/x.txt"]);
    let err = shadow
        .restore_paths(&before, &paths)
        .unwrap_err()
        .to_string();
    assert!(err.contains("d/x.txt") && err.contains("link"), "{err}");
    assert_eq!(
        fs::read_to_string(outside.join("x.txt")).unwrap(),
        "elsewhere\n"
    );
    assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), "agent\n");
}

/// Self-audit e: trimming for size (`prune`, which every backup path runs) never removes
/// snapshots from the last day, so another session's running turn keeps its first-touch backups
/// even when the shadow is over its size cap; age-based pruning is unchanged.
#[test]
fn size_trimming_keeps_the_last_day_of_snapshots() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let mut snaps = Vec::new();
    for i in 0..4u32 {
        // ~1 MB of incompressible data each.
        let mut data = Vec::new();
        let mut x = 0x9e37_79b9u32.wrapping_mul(i + 1);
        for _ in 0..250_000 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            data.extend_from_slice(&x.to_le_bytes());
        }
        fs::write(root.join("f.bin"), &data).unwrap();
        snaps.push(shadow.snapshot_paths(&["f.bin"], &format!("s{i}")).unwrap());
    }
    shadow
        .prune_capped(std::time::Duration::from_secs(3600), 1)
        .unwrap();
    assert!(
        snaps.iter().all(|s| shadow.contains(s)),
        "recent snapshots survive size trimming"
    );
    shadow.prune(std::time::Duration::ZERO).unwrap();
    assert!(
        shadow.snapshots().unwrap().is_empty(),
        "age pruning still works"
    );
}

/// A file whose case changes between a turn's two checkpoints (`a.txt` renamed to `A.txt`): the
/// comparison reports the old spelling deleted and the new one added, and restoring both leaves
/// exactly the original file with its bytes, on a case-sensitive file system (two entries) and
/// on a case-insensitive one (one file: the restore must not delete what it just restored).
#[test]
fn a_case_rename_between_checkpoints_restores_the_original_file() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("probe"), "p").unwrap();
    let insensitive = root.join("PROBE").exists();
    fs::remove_file(root.join("probe")).unwrap();
    println!("case-insensitive file system: {insensitive}");
    require_ci_folding(&root, insensitive, None);
    let ok = Command::new("git")
        .args(["init", "-q"])
        .current_dir(&root)
        .status()
        .unwrap()
        .success();
    assert!(ok);
    fs::write(root.join("a.txt"), "original\n").unwrap();
    let shadow = ShadowRepo::open(&base.join("data"), &Repo::discover(&root).unwrap()).unwrap();
    let before = shadow.snapshot_worktree("before").unwrap();
    fs::rename(root.join("a.txt"), root.join("A.txt")).unwrap();
    fs::write(root.join("A.txt"), "renamed\n").unwrap();
    let after = shadow.snapshot_worktree("after").unwrap();
    let mut changes: Vec<(String, ChangeKind)> = shadow
        .changes_between(&before, &after)
        .unwrap()
        .iter()
        .map(|c| (c.path().to_owned(), c.kind))
        .collect();
    changes.sort();
    assert_eq!(
        changes,
        [("A.txt".to_owned(), Added), ("a.txt".to_owned(), Deleted)]
    );
    shadow.restore_paths(&before, &["A.txt", "a.txt"]).unwrap();
    let names: Vec<String> = fs::read_dir(&root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".git")
        .collect();
    // Expected everywhere: exactly `a.txt` (the restore renames its temp file to the recorded
    // spelling, and the same-entry guard keeps the delete of `A.txt` from removing it).
    assert_eq!(
        names,
        ["a.txt"],
        "on {} the restore left {names:?} (case-insensitive: {insensitive})",
        std::env::consts::OS
    );
    assert_eq!(
        fs::read_to_string(root.join("a.txt")).unwrap(),
        "original\n"
    );
}

/// Hard links are other files' names, not spellings of one name: a restore that writes `b.txt`
/// and deletes `a.txt` (recorded absent) deletes it even when `a.txt` is a hard link of `b.txt`.
/// The restored `b.txt` is a new file put in place by a rename, so the same-entry guard (which
/// protects a file restored under one spelling from deleting it under another) never sees the
/// old link as the restored file.
#[test]
fn a_hard_link_recorded_absent_is_deleted_by_a_restore() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("b.txt"), "before\n").unwrap();
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let before = shadow
        .snapshot_files(
            &["b.txt", "a.txt"],
            FOLDER_MAX_FILE,
            Oversize::Refuse,
            "before",
        )
        .unwrap();
    fs::write(root.join("b.txt"), "agent\n").unwrap();
    fs::hard_link(root.join("b.txt"), root.join("a.txt")).unwrap();
    shadow.restore_paths(&before, &["a.txt", "b.txt"]).unwrap();
    assert_eq!(fs::read_to_string(root.join("b.txt")).unwrap(), "before\n");
    assert!(
        !root.join("a.txt").exists(),
        "the link recorded absent is removed"
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

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".git")
        .collect();
    names.sort();
    names
}

/// The Unicode-normalisation variant: `café.txt` recorded in NFC, its entry renamed to the NFD
/// form. On a normalising file system (APFS keeps the form as written) the restore gives the
/// entry the recorded NFC name back; where the file system cannot store it (HFS+ always stores
/// NFD) the entry keeps resolving to the restored file and that is accepted; where names are
/// not normalised (Linux, NTFS) the two forms are two files and the NFD one is left alone.
#[test]
fn a_normalisation_rename_is_restored_with_the_recorded_form() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(&root).unwrap();
    let (nfc, nfd) = ("caf\u{e9}.txt", "cafe\u{301}.txt");
    fs::write(root.join(nfc), "p").unwrap();
    let normalising = root.join(nfd).exists();
    println!("normalising file system: {normalising}");
    if std::env::var_os("CI").is_some() && cfg!(target_os = "macos") {
        assert!(
            normalising,
            "CI on macOS: {} does not fold normalisation",
            root.display()
        );
    }
    fs::write(root.join(nfc), "original\n").unwrap();
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let before = shadow
        .snapshot_files(&[nfc], FOLDER_MAX_FILE, Oversize::Refuse, "before")
        .unwrap();
    fs::rename(root.join(nfc), root.join(nfd)).unwrap();
    fs::write(root.join(nfd), "agent\n").unwrap();
    let renamed = listing(&root);
    let report = shadow.restore_paths_reporting(&before, &[nfc]).unwrap();
    let after = listing(&root);
    let msg = format!(
        "on {}: after the rename {renamed:?}, after the restore {after:?}, report {report:?}",
        std::env::consts::OS
    );
    assert_eq!(
        fs::read_to_string(root.join(nfc)).unwrap(),
        "original\n",
        "{msg}"
    );
    assert!(report.unrenamed.is_empty(), "{msg}");
    if normalising {
        // APFS: the recorded form; HFS+ cannot store NFC, so NFD there is accepted.
        assert!(after == [nfc] || after == [nfd], "{msg}");
        if cfg!(target_os = "macos") && std::env::var_os("CI").is_some() {
            assert_eq!(after, [nfc], "APFS keeps the form written: {msg}");
        }
    } else {
        let mut both = vec![nfc.to_owned(), nfd.to_owned()];
        both.sort();
        assert_eq!(after, both, "two files: {msg}");
        assert_eq!(
            fs::read_to_string(root.join(nfd)).unwrap(),
            "agent\n",
            "{msg}"
        );
    }
}

/// A parent directory whose case changed (`Src/a.txt` recorded, `src/` on disk): the restore
/// puts the bytes back through the directory as it is now and does not re-spell directories
/// (only the final component gets the recorded name). On a case-sensitive file system `Src` and
/// `src` are two directories and the restore recreates `Src/a.txt`.
#[test]
fn a_restore_does_not_re_spell_parent_directories() {
    std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
    let tmp = tempfile::tempdir().unwrap();
    let base = dunce::canonicalize(tmp.path()).unwrap();
    let root = base.join("work");
    fs::create_dir_all(root.join("Src")).unwrap();
    fs::write(root.join("Src/a.txt"), "original\n").unwrap();
    let insensitive = root.join("SRC/A.TXT").exists();
    println!("case-insensitive file system: {insensitive}");
    require_ci_folding(&root, insensitive, None);
    let shadow = ShadowRepo::open_folder(&base.join("data"), &root).unwrap();
    let before = shadow
        .snapshot_files(&["Src/a.txt"], FOLDER_MAX_FILE, Oversize::Refuse, "before")
        .unwrap();
    fs::rename(root.join("Src"), root.join("src")).unwrap();
    fs::rename(root.join("src/a.txt"), root.join("src/A.txt")).unwrap();
    fs::write(root.join("src/A.txt"), "agent\n").unwrap();
    let renamed = format!("{:?} / {:?}", listing(&root), listing(&root.join("src")));
    let report = shadow
        .restore_paths_reporting(&before, &["Src/a.txt"])
        .unwrap();
    let msg = format!(
        "on {}: after the rename {renamed}, after the restore {:?} / src {:?}, report {report:?}",
        std::env::consts::OS,
        listing(&root),
        listing(&root.join("src"))
    );
    assert!(report.unrenamed.is_empty(), "{msg}");
    if insensitive {
        assert_eq!(
            listing(&root),
            ["src"],
            "the directory keeps its spelling: {msg}"
        );
        assert_eq!(
            listing(&root.join("src")),
            ["a.txt"],
            "the file gets its own: {msg}"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/a.txt")).unwrap(),
            "original\n"
        );
    } else {
        assert_eq!(listing(&root), ["Src", "src"], "{msg}");
        assert_eq!(
            fs::read_to_string(root.join("Src/a.txt")).unwrap(),
            "original\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("src/A.txt")).unwrap(),
            "agent\n"
        );
    }
}
