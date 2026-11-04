//! `lector_workspace::conflicts` on repositories with real conflicts made by
//! the git CLI (merge with conflicting edits in merge / diff3 / zdiff3 style,
//! add/add, modify/delete, CRLF). Hermetic git config. Reading conflict state
//! must not write anything.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use lector_git::Repo;
use lector_workspace::conflicts::{
    self, apply_edits, conflict_state, parse_conflicts, resolve_all, side_view, stage_text,
    ConflictKind, ConflictStage, OperationKind, Resolution, Side,
};

fn git_out(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_COUNT", "3")
        .env("GIT_CONFIG_KEY_0", "gc.auto")
        .env("GIT_CONFIG_VALUE_0", "0")
        .env("GIT_CONFIG_KEY_1", "maintenance.auto")
        .env("GIT_CONFIG_VALUE_1", "false")
        .env("GIT_CONFIG_KEY_2", "gc.autoDetach")
        .env("GIT_CONFIG_VALUE_2", "false")
        .env("GIT_AUTHOR_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_COMMITTER_DATE", "2024-01-01T00:00:00Z")
        .env("GIT_EDITOR", "true")
        .output()
        .expect("git")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = git_out(dir, args);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, content).unwrap();
}

fn commit_all(dir: &Path, msg: &str) {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", msg]);
}

/// Every file under `root` (incl. `.git`) with content and mtime.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (Vec<u8>, Option<std::time::SystemTime>)> {
    let mut out = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                stack.push(path);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    (fs::read(&path).unwrap_or_default(), meta.modified().ok()),
                );
            }
        }
    }
    out
}

/// A repository where merging `feature` into `main` conflicts in three
/// ways: edits to the same lines of `src/lib.rs` (two hunks), add/add of
/// `notes.md`, and modify/delete of `old.txt`.
fn conflicted_merge(style: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["config", "user.email", "t@t"]);
    git(d, &["config", "user.name", "t"]);
    git(d, &["config", "core.autocrlf", "false"]);
    git(d, &["config", "merge.conflictStyle", style]);
    write(
        d,
        "src/lib.rs",
        "fn a() {\n    1\n}\n\nfn keep() {}\n\nfn b() {\n    2\n}\n",
    );
    write(d, "old.txt", "old\n");
    commit_all(d, "base");
    git(d, &["checkout", "-q", "-b", "feature"]);
    write(
        d,
        "src/lib.rs",
        "fn a() {\n    10\n}\n\nfn keep() {}\n\nfn b() {\n    20\n}\n",
    );
    write(d, "notes.md", "# Notes\nfeature\n");
    fs::remove_file(d.join("old.txt")).unwrap();
    commit_all(d, "feature");
    git(d, &["checkout", "-q", "main"]);
    write(
        d,
        "src/lib.rs",
        "fn a() {\n    100\n}\n\nfn keep() {}\n\nfn b() {\n    200\n}\n",
    );
    write(d, "notes.md", "# Notes\nmain\n");
    write(d, "old.txt", "old, edited on main\n");
    commit_all(d, "main");
    assert!(!git_out(d, &["merge", "feature"]).status.success());
    dir
}

#[test]
fn merge_conflicts_parse_and_resolve_to_the_stages() {
    for style in ["merge", "diff3", "zdiff3"] {
        let dir = conflicted_merge(style);
        let d = dir.path();
        let repo = Repo::discover(d).unwrap();
        let before = snapshot(d);
        let state = conflict_state(&repo).unwrap();
        let op = state.operation.clone().expect("merge");
        assert_eq!(op.kind, OperationKind::Merge);
        assert_eq!(op.describe(), "Merging feature into main");
        let kinds: Vec<_> = state
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.kind))
            .collect();
        assert_eq!(
            kinds,
            [
                ("notes.md", ConflictKind::BothAdded),
                ("old.txt", ConflictKind::DeletedByThem),
                ("src/lib.rs", ConflictKind::BothModified),
            ]
        );
        assert!(state
            .entry("old.txt")
            .is_some_and(|e| !e.kind.has_markers()));

        let text = fs::read_to_string(d.join("src/lib.rs")).unwrap();
        let parse = parse_conflicts(&text);
        assert!(parse.problems.is_empty(), "{style}: {:?}", parse.problems);
        assert_eq!(parse.regions.len(), 2, "{style}:\n{text}");
        let r0 = &parse.regions[0];
        assert_eq!(r0.ours_label, "HEAD");
        assert_eq!(r0.theirs_label, "feature");
        assert_eq!(r0.side(&text, Side::Ours).trim(), "100");
        assert_eq!(r0.side(&text, Side::Theirs).trim(), "10");
        if style == "merge" {
            assert!(r0.base.is_none());
        } else {
            assert_eq!(r0.side(&text, Side::Base).trim(), "1", "{style}");
        }
        // Accepting one side everywhere reproduces that stage exactly.
        let ours = stage_text(&repo, "src/lib.rs", ConflictStage::Ours)
            .unwrap()
            .unwrap();
        let theirs = stage_text(&repo, "src/lib.rs", ConflictStage::Theirs)
            .unwrap()
            .unwrap();
        let base = stage_text(&repo, "src/lib.rs", ConflictStage::Base)
            .unwrap()
            .unwrap();
        assert_eq!(
            apply_edits(&text, &resolve_all(&text, &parse.regions, Resolution::Ours)),
            ours
        );
        assert_eq!(
            apply_edits(
                &text,
                &resolve_all(&text, &parse.regions, Resolution::Theirs)
            ),
            theirs
        );
        assert_eq!(side_view(&text, &parse, Side::Ours).text, ours);
        assert_eq!(side_view(&text, &parse, Side::Theirs).text, theirs);
        if style != "merge" {
            assert_eq!(
                apply_edits(&text, &resolve_all(&text, &parse.regions, Resolution::Base)),
                base
            );
            assert!(side_view(&text, &parse, Side::Base).complete);
        }
        let both = apply_edits(
            &text,
            &resolve_all(&text, &parse.regions, Resolution::OursThenTheirs),
        );
        assert!(both.contains("    100\n    10\n"), "{both}");
        assert!(!conflicts::has_conflict_markers(&both));

        // add/add.
        let notes = fs::read_to_string(d.join("notes.md")).unwrap();
        let p = parse_conflicts(&notes);
        assert_eq!(p.regions.len(), 1);
        let merged = apply_edits(
            &notes,
            &resolve_all(&notes, &p.regions, Resolution::TheirsThenOurs),
        );
        // merge/zdiff3 share the common "# Notes" line; diff3 conflicts the whole file.
        let expected = if style != "diff3" {
            "# Notes\nfeature\nmain\n"
        } else {
            "# Notes\nfeature\n# Notes\nmain\n"
        };
        assert_eq!(merged, expected, "{style}");
        assert_eq!(
            stage_text(&repo, "notes.md", ConflictStage::Base).unwrap(),
            None
        );
        // modify/delete: no markers in the working file.
        let old = fs::read_to_string(d.join("old.txt")).unwrap();
        assert!(!conflicts::has_conflict_markers(&old));

        assert_eq!(
            snapshot(d),
            before,
            "{style}: reading conflict state wrote something"
        );
    }
}

#[test]
fn crlf_working_file() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["config", "user.email", "t@t"]);
    git(d, &["config", "user.name", "t"]);
    git(d, &["config", "core.autocrlf", "false"]);
    write(d, "w.txt", "a\r\nb\r\nc\r\n");
    commit_all(d, "base");
    git(d, &["checkout", "-q", "-b", "x"]);
    write(d, "w.txt", "a\r\nB-x\r\nc\r\n");
    commit_all(d, "x");
    git(d, &["checkout", "-q", "main"]);
    write(d, "w.txt", "a\r\nB-main\r\nc\r\n");
    commit_all(d, "main");
    assert!(!git_out(d, &["merge", "x"]).status.success());
    let text = fs::read_to_string(d.join("w.txt")).unwrap();
    let p = parse_conflicts(&text);
    assert_eq!(p.regions.len(), 1, "{text:?}");
    let r = &p.regions[0];
    assert_eq!(r.side(&text, Side::Ours), "B-main\r\n");
    assert_eq!(
        apply_edits(&text, &resolve_all(&text, &p.regions, Resolution::Theirs)),
        "a\r\nB-x\r\nc\r\n"
    );
}

#[test]
fn clean_repository_has_clean_state() {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    git(d, &["init", "-q", "-b", "main"]);
    git(d, &["config", "user.email", "t@t"]);
    git(d, &["config", "user.name", "t"]);
    write(d, "a.txt", "a\n");
    commit_all(d, "init");
    let repo = Repo::discover(d).unwrap();
    assert!(conflict_state(&repo).unwrap().is_clean());
}
