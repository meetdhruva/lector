//! Partial staging round trips (`review::apply_hunk_action`) on fixture repositories:
//! stage / unstage / discard one hunk or a line selection, checked with git itself.
#![allow(clippy::single_range_in_vec_init)]

use std::path::Path;
use std::process::Command;
use std::sync::Once;

use lector_diff::{Algorithm, DiffOptions};
use lector_git::{Changeset, FileChange, Repo};
use lector_syntax::LanguageRegistry;
use lector_workspace::review::{self, FileDiff, HunkAction, LineSelection};

fn hermetic() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
        std::env::set_var("GIT_CONFIG_COUNT", "3");
        std::env::set_var("GIT_CONFIG_KEY_0", "gc.auto");
        std::env::set_var("GIT_CONFIG_VALUE_0", "0");
        std::env::set_var("GIT_CONFIG_KEY_1", "maintenance.auto");
        std::env::set_var("GIT_CONFIG_VALUE_1", "false");
        std::env::set_var("GIT_CONFIG_KEY_2", "commit.gpgsign");
        std::env::set_var("GIT_CONFIG_VALUE_2", "false");
    });
}

struct Fx {
    dir: tempfile::TempDir,
    languages: LanguageRegistry,
}

impl Fx {
    fn new() -> Fx {
        hermetic();
        let fx = Fx {
            dir: tempfile::tempdir().expect("tempdir"),
            languages: LanguageRegistry::new(),
        };
        fx.git(&["init", "-q", "-b", "main"]);
        fx.git(&["config", "user.name", "T"]);
        fx.git(&["config", "user.email", "t@example.com"]);
        fx.git(&["config", "core.autocrlf", "false"]);
        fx
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn repo(&self) -> Repo {
        Repo::discover(self.path()).expect("discover")
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(self.path())
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn write(&self, rel: &str, content: &[u8]) {
        std::fs::write(self.path().join(rel), content).unwrap();
    }

    fn read(&self, rel: &str) -> Vec<u8> {
        std::fs::read(self.path().join(rel)).unwrap()
    }

    fn index(&self, rel: &str) -> Vec<u8> {
        let out = Command::new("git")
            .args(["show", &format!(":{rel}")])
            .current_dir(self.path())
            .output()
            .unwrap();
        assert!(out.status.success());
        out.stdout
    }

    fn commit(&self, rel: &str, content: &[u8]) {
        self.write(rel, content);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "c"]);
    }

    fn change(&self, cs: &Changeset, rel: &str) -> FileChange {
        self.repo()
            .changes(cs)
            .unwrap()
            .into_iter()
            .find(|c| c.path() == rel)
            .unwrap_or_else(|| panic!("{rel} not in {cs:?}"))
    }

    fn diff(&self, cs: &Changeset, rel: &str) -> FileDiff {
        let change = self.change(cs, rel);
        review::load_file_diff_with(
            &self.repo(),
            &self.languages,
            &change,
            &DiffOptions::default(),
        )
        .unwrap()
    }

    /// Applies `action` to hunk `ix` of `rel` in `cs`.
    fn hunk(&self, cs: &Changeset, rel: &str, action: HunkAction, ix: usize) {
        let d = self.diff(cs, rel);
        let sel = d.hunk_selection(ix).expect("hunk exists");
        self.select(cs, rel, action, &sel);
    }

    fn select(&self, cs: &Changeset, rel: &str, action: HunkAction, sel: &LineSelection) {
        let change = self.change(cs, rel);
        let applied =
            review::apply_hunk_action(&self.repo(), cs, &change, action, sel, Algorithm::Histogram)
                .unwrap();
        assert!(applied, "nothing applied");
    }
}

const BASE: &str = "l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\n";
/// Two separate hunks: l2 changed, l11 changed.
const TWO_HUNKS: &str = "l1\nL2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nL11\nl12\n";

#[test]
fn stage_one_hunk_then_unstage_it() {
    let fx = Fx::new();
    fx.commit("f.txt", BASE.as_bytes());
    fx.write("f.txt", TWO_HUNKS.as_bytes());
    assert_eq!(fx.diff(&Changeset::Unstaged, "f.txt").diff.hunks.len(), 2);

    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, 0);
    let cached = fx.git(&["diff", "--cached", "-U0"]);
    assert!(cached.contains("-l2\n+L2\n"), "{cached}");
    assert!(!cached.contains("L11"), "{cached}");
    let unstaged = fx.git(&["diff", "-U0"]);
    assert!(
        unstaged.contains("-l11\n+L11\n") && !unstaged.contains("L2"),
        "{unstaged}"
    );

    // Unstage it again from the Staged changeset.
    fx.hunk(&Changeset::Staged, "f.txt", HunkAction::Unstage, 0);
    assert_eq!(fx.git(&["diff", "--cached"]), "");
    assert_eq!(fx.read("f.txt"), TWO_HUNKS.as_bytes());
}

#[test]
fn stage_last_hunk_and_hunks_at_file_edges() {
    let fx = Fx::new();
    fx.commit("f.txt", BASE.as_bytes());
    // Changes on the first and the last line (no context before / after).
    let edited = BASE
        .replace("l1\n", "FIRST\n")
        .replace("l12\n", "LAST\nextra\n");
    fx.write("f.txt", edited.as_bytes());
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, 1);
    assert_eq!(
        String::from_utf8(fx.index("f.txt")).unwrap(),
        BASE.replace("l12\n", "LAST\nextra\n")
    );
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, 0);
    assert_eq!(fx.index("f.txt"), edited.as_bytes());
    assert_eq!(fx.git(&["diff"]), "");
    // Unstage the first-line hunk only.
    fx.hunk(&Changeset::Staged, "f.txt", HunkAction::Unstage, 0);
    assert_eq!(
        String::from_utf8(fx.index("f.txt")).unwrap(),
        BASE.replace("l12\n", "LAST\nextra\n")
    );
}

#[test]
fn discard_one_hunk() {
    let fx = Fx::new();
    fx.commit("f.txt", BASE.as_bytes());
    fx.write("f.txt", TWO_HUNKS.as_bytes());
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Discard, 1);
    assert_eq!(
        String::from_utf8(fx.read("f.txt")).unwrap(),
        BASE.replace("l2\n", "L2\n")
    );
    assert_eq!(
        fx.git(&["diff", "--cached"]),
        "",
        "discard never touches the index"
    );
}

#[test]
fn stage_selected_lines() {
    let fx = Fx::new();
    fx.commit("f.txt", b"a\nb\nc\nd\n");
    // Hunk: b, c -> B1, B2, B3
    fx.write("f.txt", b"a\nB1\nB2\nB3\nd\n");
    let d = fx.diff(&Changeset::Unstaged, "f.txt");
    assert_eq!(d.diff.hunks.len(), 1);
    // Stage removal of "b" (old row 1) and addition of "B2" (new row 2) only.
    let sel = LineSelection {
        old: vec![1..2],
        new: vec![2..3],
    };
    fx.select(&Changeset::Unstaged, "f.txt", HunkAction::Stage, &sel);
    assert_eq!(fx.index("f.txt"), b"a\nc\nB2\nd\n");
    // Unstage just "B2" again from the staged diff (new row 2 there).
    let staged = fx.diff(&Changeset::Staged, "f.txt");
    let row = (0..staged.new.line_count())
        .find(|r| staged.new.line(*r) == "B2")
        .unwrap();
    let sel = LineSelection {
        old: vec![],
        new: vec![row..row + 1],
    };
    fx.select(&Changeset::Staged, "f.txt", HunkAction::Unstage, &sel);
    assert_eq!(fx.index("f.txt"), b"a\nc\nd\n");
    // Discard only the added "B3" from the working tree (new row 3 in the unstaged diff).
    let d = fx.diff(&Changeset::Unstaged, "f.txt");
    let row = (0..d.new.line_count())
        .find(|r| d.new.line(*r) == "B3")
        .unwrap();
    let sel = LineSelection {
        old: vec![],
        new: vec![row..row + 1],
    };
    fx.select(&Changeset::Unstaged, "f.txt", HunkAction::Discard, &sel);
    assert_eq!(fx.read("f.txt"), b"a\nB1\nB2\nd\n");
}

#[test]
fn staged_and_unstaged_changes_in_one_file() {
    let fx = Fx::new();
    fx.commit("f.txt", BASE.as_bytes());
    fx.write("f.txt", TWO_HUNKS.as_bytes());
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, 0);
    // Further edits next to the staged hunk and elsewhere.
    let more = TWO_HUNKS.replace("l3\n", "l3 edited\n").replace("l7\n", "");
    fx.write("f.txt", more.as_bytes());
    let d = fx.diff(&Changeset::Unstaged, "f.txt");
    assert_eq!(d.diff.hunks.len(), 3, "{:?}", d.diff.hunks);
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, 1); // remove l7
    let index = String::from_utf8(fx.index("f.txt")).unwrap();
    assert_eq!(index, BASE.replace("l2\n", "L2\n").replace("l7\n", ""));
    // Unstage the first staged hunk while the working tree has more edits.
    fx.hunk(&Changeset::Staged, "f.txt", HunkAction::Unstage, 0);
    let index = String::from_utf8(fx.index("f.txt")).unwrap();
    assert_eq!(index, BASE.replace("l7\n", ""));
    assert_eq!(fx.read("f.txt"), more.as_bytes());
}

#[test]
fn missing_final_newline() {
    let fx = Fx::new();
    fx.commit("f.txt", b"a\nb\nc");
    fx.write("f.txt", b"a\nB\nc\nd\n");
    let d = fx.diff(&Changeset::Unstaged, "f.txt");
    for ix in (0..d.diff.hunks.len()).rev() {
        fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Stage, ix);
    }
    assert_eq!(fx.index("f.txt"), b"a\nB\nc\nd\n");
    // And back: discard everything staged... via unstage of every hunk.
    loop {
        let d = fx.diff(&Changeset::Staged, "f.txt");
        if d.diff.hunks.is_empty() {
            break;
        }
        fx.hunk(&Changeset::Staged, "f.txt", HunkAction::Unstage, 0);
        if fx.git(&["diff", "--cached"]).is_empty() {
            break;
        }
    }
    assert_eq!(fx.index("f.txt"), b"a\nb\nc");
    // Discard of the newline-at-eof change only.
    fx.write("f.txt", b"a\nb\nc\n");
    fx.hunk(&Changeset::Unstaged, "f.txt", HunkAction::Discard, 0);
    assert_eq!(fx.read("f.txt"), b"a\nb\nc");
}

#[test]
fn crlf_files() {
    let fx = Fx::new();
    fx.commit(
        "w.txt",
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\r\n",
    );
    fx.write(
        "w.txt",
        b"ONE\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\nEIGHT\r\n",
    );
    fx.hunk(&Changeset::Unstaged, "w.txt", HunkAction::Stage, 1);
    assert_eq!(
        fx.index("w.txt"),
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\nEIGHT\r\n"
    );
    fx.hunk(&Changeset::Unstaged, "w.txt", HunkAction::Discard, 0);
    assert_eq!(
        fx.read("w.txt"),
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\nEIGHT\r\n"
    );
}

#[test]
fn autocrlf_working_tree() {
    let fx = Fx::new();
    fx.git(&["config", "core.autocrlf", "true"]);
    fx.commit("w.txt", b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n");
    assert_eq!(fx.index("w.txt"), b"a\nb\nc\nd\ne\nf\ng\nh\n");
    fx.write("w.txt", b"A\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nH\r\n");
    fx.hunk(&Changeset::Unstaged, "w.txt", HunkAction::Stage, 0);
    assert_eq!(fx.index("w.txt"), b"A\nb\nc\nd\ne\nf\ng\nh\n");
    fx.hunk(&Changeset::Unstaged, "w.txt", HunkAction::Discard, 0);
    assert_eq!(
        fx.read("w.txt"),
        b"A\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\n"
    );
}

#[test]
fn deleted_and_added_files() {
    let fx = Fx::new();
    fx.commit("gone.txt", b"x\ny\n");
    std::fs::remove_file(fx.path().join("gone.txt")).unwrap();
    // Staging the whole hunk of a deleted file deletes it from the index.
    fx.hunk(&Changeset::Unstaged, "gone.txt", HunkAction::Stage, 0);
    assert_eq!(
        fx.git(&["diff", "--cached", "--name-status"]),
        "D\tgone.txt\n"
    );
    // Unstaging it restores the index entry.
    fx.hunk(&Changeset::Staged, "gone.txt", HunkAction::Unstage, 0);
    assert_eq!(fx.git(&["diff", "--cached", "--name-status"]), "");
    assert_eq!(fx.index("gone.txt"), b"x\ny\n");
    // Discarding the deletion brings the file back.
    fx.hunk(&Changeset::Unstaged, "gone.txt", HunkAction::Discard, 0);
    assert_eq!(fx.read("gone.txt"), b"x\ny\n");

    // A staged new file: unstaging its only hunk removes it from the index.
    fx.write("new.txt", b"hello\nworld\n");
    fx.git(&["add", "new.txt"]);
    fx.hunk(&Changeset::Staged, "new.txt", HunkAction::Unstage, 0);
    assert_eq!(fx.git(&["status", "--porcelain"]), "?? new.txt\n");
}

#[test]
fn wrong_changeset_is_refused() {
    let fx = Fx::new();
    fx.commit("f.txt", b"a\n");
    fx.write("f.txt", b"b\n");
    let change = fx.change(&Changeset::Unstaged, "f.txt");
    let err = review::apply_hunk_action(
        &fx.repo(),
        &Changeset::WorkingTree,
        &change,
        HunkAction::Stage,
        &LineSelection::all(),
        Algorithm::Histogram,
    );
    assert!(err.is_err());
    assert_eq!(
        HunkAction::available_for(&Changeset::Staged),
        &[HunkAction::Unstage]
    );
}

/// Regression: rows selected in a diff that is no longer current (the file was edited
/// and saved after the diff was shown) must not be applied to the new content, where
/// they would stage / discard different lines.
#[test]
fn stale_diff_is_refused_and_writes_nothing() {
    let fx = Fx::new();
    fx.commit("f.txt", BASE.as_bytes());
    fx.write("f.txt", TWO_HUNKS.as_bytes());
    let shown = fx.diff(&Changeset::Unstaged, "f.txt");
    let sel = shown.hunk_selection(0).expect("hunk 0");
    let change = fx.change(&Changeset::Unstaged, "f.txt");
    // The file changes on disk: a new line is inserted at the top.
    fx.write("f.txt", format!("NEW\n{TWO_HUNKS}").as_bytes());
    for action in [HunkAction::Stage, HunkAction::Discard] {
        let err = review::apply_hunk_action_if_unchanged(
            &fx.repo(),
            &Changeset::Unstaged,
            &change,
            action,
            &sel,
            Algorithm::Histogram,
            shown.content_id(),
        );
        assert!(err.is_err(), "{action:?} on a stale diff must fail");
    }
    assert_eq!(fx.index("f.txt"), BASE.as_bytes());
    assert_eq!(fx.read("f.txt"), format!("NEW\n{TWO_HUNKS}").as_bytes());

    // With a fresh diff the same call goes through.
    let fresh = fx.diff(&Changeset::Unstaged, "f.txt");
    let sel = fresh.hunk_selection(1).expect("hunk 1");
    let applied = review::apply_hunk_action_if_unchanged(
        &fx.repo(),
        &Changeset::Unstaged,
        &change,
        HunkAction::Stage,
        &sel,
        Algorithm::Histogram,
        fresh.content_id(),
    )
    .unwrap();
    assert!(applied);
    assert_eq!(
        fx.index("f.txt"),
        "l1\nL2\nl3\nl4\nl5\nl6\nl7\nl8\nl9\nl10\nl11\nl12\n".as_bytes()
    );
}

/// The content id check also holds for staged diffs (unstage) and CRLF working files
/// under autocrlf, where the working copy is compared normalized.
#[test]
fn content_id_matches_for_unchanged_staged_and_crlf_files() {
    let fx = Fx::new();
    fx.git(&["config", "core.autocrlf", "true"]);
    fx.commit("f.txt", BASE.as_bytes());
    fx.write("f.txt", TWO_HUNKS.replace('\n', "\r\n").as_bytes());
    let d = fx.diff(&Changeset::Unstaged, "f.txt");
    let change = fx.change(&Changeset::Unstaged, "f.txt");
    assert!(review::apply_hunk_action_if_unchanged(
        &fx.repo(),
        &Changeset::Unstaged,
        &change,
        HunkAction::Stage,
        &d.hunk_selection(0).unwrap(),
        Algorithm::Histogram,
        d.content_id(),
    )
    .unwrap());
    let s = fx.diff(&Changeset::Staged, "f.txt");
    let change = fx.change(&Changeset::Staged, "f.txt");
    assert!(review::apply_hunk_action_if_unchanged(
        &fx.repo(),
        &Changeset::Staged,
        &change,
        HunkAction::Unstage,
        &s.hunk_selection(0).unwrap(),
        Algorithm::Histogram,
        s.content_id(),
    )
    .unwrap());
    assert_eq!(fx.index("f.txt"), BASE.as_bytes());
}
