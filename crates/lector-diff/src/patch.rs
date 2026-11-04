//! Patch generation for partial staging: a `git apply`-compatible unified diff
//! that applies only the selected changes of `old → new` to `old`.
//!
//! The semantics follow `git add -p`'s edit mode: a selected removed line is
//! removed, an unselected removed line stays (becomes context), a selected added
//! line is added and an unselected added line is dropped. Context lines always come
//! from `old`, so the patch applies to `old` even when neighbouring changes were not
//! selected. To unstage or discard, build the patch over the *reversed* pair
//! (`index → HEAD`, `worktree → index`) with the selection's sides swapped
//! ([`LineSelection::swapped`]) and apply it forwards.
//!
//! Selections and hunks use the display row model of [`crate::diff_lines`] (split on
//! `'\n'`; a text ending in `'\n'` has a trailing empty row, whose addition or removal
//! is the "newline at end of file" change). The patch itself is written in byte lines
//! *including* their terminator, so CRLF files and a missing final newline
//! (`\ No newline at end of file`) round-trip exactly.

use std::borrow::Cow;
use std::ops::Range;

use crate::{run, Algorithm};

/// Number of context lines around each change, like `git diff`.
pub const CONTEXT_LINES: usize = 3;

/// Which lines of a diff to include, in the line model of [`crate::diff_lines`]
/// (0-based rows of the `old` and `new` text).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LineSelection {
    /// Selected rows of the old text (removed lines to remove).
    pub old: Vec<Range<u32>>,
    /// Selected rows of the new text (added lines to add).
    pub new: Vec<Range<u32>>,
}

impl LineSelection {
    /// Selects one hunk (`old` and `new` ranges of [`crate::Hunk`]).
    pub fn hunk(hunk: &crate::Hunk) -> Self {
        LineSelection {
            old: vec![hunk.old.clone()],
            new: vec![hunk.new.clone()],
        }
    }

    /// Selects every line of both texts.
    #[allow(clippy::single_range_in_vec_init)]
    pub fn all() -> Self {
        LineSelection {
            old: vec![0..u32::MAX],
            new: vec![0..u32::MAX],
        }
    }

    /// The same selection with `old` and `new` exchanged (for reversed patches).
    pub fn swapped(&self) -> Self {
        LineSelection {
            old: self.new.clone(),
            new: self.old.clone(),
        }
    }

    /// True when nothing is selected.
    pub fn is_empty(&self) -> bool {
        self.old.iter().chain(&self.new).all(|r| r.is_empty())
    }

    fn has_old(&self, row: u32) -> bool {
        self.old.iter().any(|r| r.contains(&row))
    }

    fn has_new(&self, row: u32) -> bool {
        self.new.iter().any(|r| r.contains(&row))
    }
}

/// The file a patch modifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PatchFile<'a> {
    /// Repository-relative path (`/`-separated) of the file in the target.
    pub path: &'a str,
    /// Git file mode for new/deleted file headers (`0o100644`, `0o100755`).
    pub mode: u32,
}

/// Builds a patch that applies the `selection` of the changes `old → new` to `old`.
///
/// `old == None` means the file does not exist in the target (the patch creates it);
/// `new == None` means it does not exist on the other side, so selecting every line
/// produces a deletion. Returns `None` when the selection contains no change, or
/// when either side looks binary (a NUL byte; such files can only be staged whole).
pub fn build_patch(
    file: &PatchFile<'_>,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    selection: &LineSelection,
) -> Option<Vec<u8>> {
    build_patch_with(file, old, new, selection, Algorithm::Histogram)
}

/// [`build_patch`] with an explicit diff algorithm (use the one the view used, so
/// the selected rows line up with the hunks the user saw).
pub fn build_patch_with(
    file: &PatchFile<'_>,
    old: Option<&[u8]>,
    new: Option<&[u8]>,
    selection: &LineSelection,
    algorithm: Algorithm,
) -> Option<Vec<u8>> {
    let old_bytes = old.unwrap_or_default();
    let new_bytes = new.unwrap_or_default();
    if crate::is_binary(old_bytes) || crate::is_binary(new_bytes) {
        return None;
    }
    // Diff in the display row model (rows joined by '\n' give back the text exactly),
    // so the hunks are the ones the view shows.
    let old_rows: Vec<&[u8]> = old_bytes.split(|b| *b == b'\n').collect();
    let new_rows: Vec<&[u8]> = new_bytes.split(|b| *b == b'\n').collect();
    let hunks = run(
        old_rows.iter().copied(),
        new_rows.iter().copied(),
        algorithm,
    );

    // Edit script over display rows: the result is old with the selection applied.
    let mut rows: Vec<RowOp<'_>> = Vec::with_capacity(old_rows.len() + 8);
    let mut pos = 0usize;
    for h in &hunks {
        let (os, oe) = (h.old.start as usize, h.old.end as usize);
        rows.extend(old_rows[pos..os].iter().map(|r| RowOp::Keep(r)));
        for (i, row) in old_rows.iter().enumerate().take(oe).skip(os) {
            rows.push(if selection.has_old(i as u32) {
                RowOp::Remove(row)
            } else {
                RowOp::Keep(row)
            });
        }
        let (ns, ne) = (h.new.start as usize, h.new.end as usize);
        for (j, row) in new_rows.iter().enumerate().take(ne).skip(ns) {
            if selection.has_new(j as u32) {
                rows.push(RowOp::Insert(row));
            }
        }
        pos = oe;
    }
    rows.extend(old_rows[pos..].iter().map(|r| RowOp::Keep(r)));

    let ops = to_line_ops(&rows);
    if !ops.iter().any(Op::is_change) {
        return None;
    }
    let creates = old.is_none();
    let deletes = new.is_none() && ops.iter().all(|op| matches!(op, Op::Del(_)));
    let mut out = header(file, creates, deletes);
    write_hunks(&mut out, &ops);
    Some(out)
}

/// One display row of the old text, kept or removed, or a row inserted from new.
enum RowOp<'a> {
    Keep(&'a [u8]),
    Remove(&'a [u8]),
    Insert(&'a [u8]),
}

/// Converts row operations into byte-line operations. A row is followed by `'\n'`
/// unless it is the last row of its text (old for kept/removed rows, the result for
/// kept/inserted rows); a last row that is empty does not exist as a line. A kept
/// row whose terminator differs between old and result becomes a removal plus an
/// addition (adding or dropping the final newline).
fn to_line_ops<'a>(rows: &[RowOp<'a>]) -> Vec<Op<'a>> {
    let last_old = rows.iter().rposition(|r| !matches!(r, RowOp::Insert(_)));
    let last_new = rows.iter().rposition(|r| !matches!(r, RowOp::Remove(_)));
    let line = |row: &'a [u8], last: bool| -> Option<Cow<'a, [u8]>> {
        if last {
            (!row.is_empty()).then_some(Cow::Borrowed(row))
        } else {
            let mut l = row.to_vec();
            l.push(b'\n');
            Some(Cow::Owned(l))
        }
    };
    let mut ops = Vec::with_capacity(rows.len());
    for (ix, row) in rows.iter().enumerate() {
        match row {
            RowOp::Keep(r) => {
                let in_old = line(r, Some(ix) == last_old);
                let in_new = line(r, Some(ix) == last_new);
                if in_old == in_new {
                    ops.extend(in_old.map(Op::Ctx));
                } else {
                    ops.extend(in_old.map(Op::Del));
                    ops.extend(in_new.map(Op::Add));
                }
            }
            RowOp::Remove(r) => ops.extend(line(r, Some(ix) == last_old).map(Op::Del)),
            RowOp::Insert(r) => ops.extend(line(r, Some(ix) == last_new).map(Op::Add)),
        }
    }
    // Keep removals before additions inside each run of changes (git's convention).
    let mut i = 0;
    while i < ops.len() {
        if !ops[i].is_change() {
            i += 1;
            continue;
        }
        let start = i;
        while i < ops.len() && ops[i].is_change() {
            i += 1;
        }
        ops[start..i].sort_by_key(|op| matches!(op, Op::Add(_)));
    }
    ops
}

/// One line of the edit script over `old`.
#[derive(Debug)]
enum Op<'a> {
    Ctx(Cow<'a, [u8]>),
    Del(Cow<'a, [u8]>),
    Add(Cow<'a, [u8]>),
}

impl Op<'_> {
    fn is_change(&self) -> bool {
        !matches!(self, Op::Ctx(_))
    }
    fn in_old(&self) -> bool {
        !matches!(self, Op::Add(_))
    }
    fn in_new(&self) -> bool {
        !matches!(self, Op::Del(_))
    }
}

fn header(file: &PatchFile<'_>, creates: bool, deletes: bool) -> Vec<u8> {
    let a = quote_path("a/", file.path);
    let b = quote_path("b/", file.path);
    let mut out = format!("diff --git {a} {b}\n");
    if creates {
        out.push_str(&format!("new file mode {:o}\n", file.mode));
        out.push_str(&format!("--- /dev/null\n+++ {b}\n"));
    } else if deletes {
        out.push_str(&format!("deleted file mode {:o}\n", file.mode));
        out.push_str(&format!("--- {a}\n+++ /dev/null\n"));
    } else {
        out.push_str(&format!("--- {a}\n+++ {b}\n"));
    }
    out.into_bytes()
}

/// `prefix + path`, C-quoted like git when the path contains `"`, `\` or control chars.
fn quote_path(prefix: &str, path: &str) -> String {
    let needs = path
        .chars()
        .any(|c| c == '"' || c == '\\' || c.is_control());
    if !needs {
        return format!("{prefix}{path}");
    }
    let mut out = String::from("\"");
    out.push_str(prefix);
    for c in path.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if c.is_control() => {
                let mut buf = [0u8; 4];
                for b in c.encode_utf8(&mut buf).bytes() {
                    out.push_str(&format!("\\{b:03o}"));
                }
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_hunks(out: &mut Vec<u8>, ops: &[Op<'_>]) {
    let changes: Vec<usize> = (0..ops.len()).filter(|&i| ops[i].is_change()).collect();
    // Group changes whose context windows touch.
    let mut groups: Vec<Range<usize>> = Vec::new();
    for &c in &changes {
        let start = c.saturating_sub(CONTEXT_LINES);
        let end = (c + 1 + CONTEXT_LINES).min(ops.len());
        match groups.last_mut() {
            Some(g) if start <= g.end => g.end = g.end.max(end),
            _ => groups.push(start..end),
        }
    }
    // Line numbers before each op (0-based) in old and in the result.
    let mut old_at = Vec::with_capacity(ops.len());
    let mut new_at = Vec::with_capacity(ops.len());
    let (mut o, mut n) = (0usize, 0usize);
    for op in ops {
        old_at.push(o);
        new_at.push(n);
        o += op.in_old() as usize;
        n += op.in_new() as usize;
    }
    for g in groups {
        let slice = &ops[g.clone()];
        let old_count = slice.iter().filter(|op| op.in_old()).count();
        let new_count = slice.iter().filter(|op| op.in_new()).count();
        // An empty side names the line *before* the hunk (0 at the start of the file).
        let old_start = old_at[g.start] + usize::from(old_count > 0);
        let new_start = new_at[g.start] + usize::from(new_count > 0);
        out.extend_from_slice(
            format!("@@ -{old_start},{old_count} +{new_start},{new_count} @@\n").as_bytes(),
        );
        for op in slice {
            let (prefix, line) = match op {
                Op::Ctx(l) => (b' ', l),
                Op::Del(l) => (b'-', l),
                Op::Add(l) => (b'+', l),
            };
            out.push(prefix);
            out.extend_from_slice(line);
            if !line.ends_with(b"\n") {
                out.extend_from_slice(b"\n\\ No newline at end of file\n");
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const FILE: PatchFile<'static> = PatchFile {
        path: "src/a.txt",
        mode: 0o100644,
    };

    /// Minimal `git apply` for our own output: checks context and counts strictly.
    fn apply(old: &[u8], patch: &[u8]) -> Vec<u8> {
        let old_lines: Vec<&[u8]> = old.split_inclusive(|b| *b == b'\n').collect();
        let lines: Vec<&[u8]> = patch.split_inclusive(|b| *b == b'\n').collect();
        let mut out: Vec<u8> = Vec::new();
        let mut pos = 0usize;
        let mut i = 0;
        while i < lines.len() && !lines[i].starts_with(b"@@") {
            i += 1;
        }
        while i < lines.len() {
            let h = std::str::from_utf8(lines[i]).unwrap();
            let nums: Vec<usize> = h
                .split(|c: char| !c.is_ascii_digit())
                .filter(|s| !s.is_empty())
                .map(|s| s.parse().unwrap())
                .collect();
            let (os, oc, _ns, nc) = (nums[0], nums[1], nums[2], nums[3]);
            let start = if oc == 0 { os } else { os - 1 };
            for l in &old_lines[pos..start] {
                out.extend_from_slice(l);
            }
            pos = start;
            i += 1;
            let (mut seen_old, mut seen_new) = (0, 0);
            let mut last: Option<(u8, usize)> = None; // (kind, out len before line)
            while i < lines.len() && !lines[i].starts_with(b"@@") {
                let l = lines[i];
                if l.starts_with(b"\\") {
                    // Strip the newline we wrote for the previous line.
                    let (kind, _) = last.unwrap();
                    if kind != b'-' {
                        assert_eq!(out.pop(), Some(b'\n'));
                    }
                    i += 1;
                    continue;
                }
                let body = &l[1..];
                match l[0] {
                    b' ' | b'-' => {
                        let want = old_lines[pos];
                        let got_trimmed = body;
                        let ok = want == got_trimmed
                            || (!want.ends_with(b"\n")
                                && got_trimmed.strip_suffix(b"\n") == Some(want));
                        assert!(ok, "context mismatch {want:?} vs {body:?}");
                        if l[0] == b' ' {
                            out.extend_from_slice(body);
                            seen_new += 1;
                        }
                        pos += 1;
                        seen_old += 1;
                    }
                    b'+' => {
                        out.extend_from_slice(body);
                        seen_new += 1;
                    }
                    _ => panic!("bad line {l:?}"),
                }
                last = Some((l[0], out.len()));
                i += 1;
            }
            assert_eq!((seen_old, seen_new), (oc, nc), "counts in {h}");
        }
        for l in &old_lines[pos..] {
            out.extend_from_slice(l);
        }
        out
    }

    fn patch_text(old: &str, new: &str, sel: &LineSelection) -> Option<String> {
        build_patch(&FILE, Some(old.as_bytes()), Some(new.as_bytes()), sel)
            .map(|p| String::from_utf8(p).unwrap())
    }

    #[test]
    fn whole_hunk_patch() {
        let old = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n";
        let new = "a\nB\nc\nd\ne\nf\ng\nh\ni\nJ\n";
        let diff = crate::diff_lines(old, new, &Default::default());
        assert_eq!(diff.hunks.len(), 2);
        let p = patch_text(old, new, &LineSelection::hunk(&diff.hunks[0])).unwrap();
        assert_eq!(
            p,
            "diff --git a/src/a.txt b/src/a.txt\n--- a/src/a.txt\n+++ b/src/a.txt\n\
             @@ -1,5 +1,5 @@\n a\n-b\n+B\n c\n d\n e\n"
        );
        let p2 = patch_text(old, new, &LineSelection::hunk(&diff.hunks[1])).unwrap();
        assert!(p2.contains("@@ -7,4 +7,4 @@\n g\n h\n i\n-j\n+J\n"), "{p2}");
        assert_eq!(
            apply(old.as_bytes(), p2.as_bytes()),
            b"a\nb\nc\nd\ne\nf\ng\nh\ni\nJ\n"
        );
    }

    #[test]
    fn line_selection_keeps_unselected_removals_and_drops_additions() {
        let old = "1\n2\n3\n";
        let new = "1\nx\ny\n3\n";
        // Select only the added "y" (new row 2): "2" stays, "x" is dropped.
        let sel = LineSelection {
            old: vec![],
            new: vec![2..3],
        };
        let p = patch_text(old, new, &sel).unwrap();
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), b"1\n2\ny\n3\n");
        // Select only the removal of "2".
        let sel = LineSelection {
            old: vec![1..2],
            new: vec![],
        };
        let p = patch_text(old, new, &sel).unwrap();
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), b"1\n3\n");
        assert!(patch_text(old, new, &LineSelection::default()).is_none());
    }

    #[test]
    fn new_and_deleted_files() {
        let p = build_patch(&FILE, None, Some(b"a\nb"), &LineSelection::all()).unwrap();
        assert_eq!(
            String::from_utf8(p).unwrap(),
            "diff --git a/src/a.txt b/src/a.txt\nnew file mode 100644\n--- /dev/null\n+++ b/src/a.txt\n\
             @@ -0,0 +1,2 @@\n+a\n+b\n\\ No newline at end of file\n"
        );
        let p = build_patch(&FILE, Some(b"a\n"), None, &LineSelection::all()).unwrap();
        assert_eq!(
            String::from_utf8(p).unwrap(),
            "diff --git a/src/a.txt b/src/a.txt\ndeleted file mode 100644\n--- a/src/a.txt\n+++ /dev/null\n\
             @@ -1,1 +0,0 @@\n-a\n"
        );
        // Partial removal from a file that is deleted on the other side: a modification.
        let p = build_patch(
            &FILE,
            Some(b"a\nb\n"),
            None,
            &LineSelection {
                old: vec![0..1],
                new: vec![],
            },
        )
        .unwrap();
        assert!(String::from_utf8(p).unwrap().contains("+++ b/src/a.txt"));
    }

    #[test]
    fn missing_final_newline_fixups() {
        // Adding the final newline: display hunk adds the phantom row.
        let old = "a\nb";
        let new = "a\nb\n";
        let diff = crate::diff_lines(old, new, &Default::default());
        let p = patch_text(old, new, &LineSelection::hunk(&diff.hunks[0])).unwrap();
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), new.as_bytes());
        // Appending lines after a final line without newline: selecting only the "c"
        // row (not the trailing empty row) adds "c" without a final newline.
        let old = "a\nb";
        let new = "a\nb\nc\n";
        let sel = LineSelection {
            old: vec![],
            new: vec![2..3],
        };
        let p = patch_text(old, new, &sel).unwrap();
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), b"a\nb\nc");
        let sel = LineSelection {
            old: vec![],
            new: vec![2..4],
        };
        let p = patch_text(old, new, &sel).unwrap();
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), b"a\nb\nc\n");
        // Removing the final newline.
        let p = patch_text("a\nb\n", "a\nb", &LineSelection::all()).unwrap();
        assert!(p.ends_with("-b\n+b\n\\ No newline at end of file\n"), "{p}");
    }

    #[test]
    fn crlf_lines_round_trip() {
        let old = "a\r\nb\r\nc\r\n";
        let new = "a\r\nB\r\nc\r\n";
        let p = patch_text(old, new, &LineSelection::all()).unwrap();
        assert!(p.contains("-b\r\n+B\r\n"));
        assert_eq!(apply(old.as_bytes(), p.as_bytes()), new.as_bytes());
    }

    #[test]
    fn quotes_unusual_paths() {
        assert_eq!(quote_path("a/", "x y.txt"), "a/x y.txt");
        assert_eq!(quote_path("a/", "q\"t\\.txt"), "\"a/q\\\"t\\\\.txt\"");
        assert_eq!(quote_path("b/", "tab\tx"), "\"b/tab\\tx\"");
        assert_eq!(quote_path("a/", "é.txt"), "a/é.txt");
    }

    #[test]
    fn binary_is_refused() {
        assert!(build_patch(&FILE, Some(b"a\0"), Some(b"b"), &LineSelection::all()).is_none());
    }

    fn text_strategy() -> impl Strategy<Value = String> {
        proptest::collection::vec(prop_oneof!["a", "b", "c", "d", ""], 0..14).prop_flat_map(
            |lines| {
                let joined = lines.join("\n");
                prop_oneof![Just(joined.clone()), Just(format!("{joined}\n"))]
            },
        )
    }

    proptest! {
        #[test]
        fn selecting_everything_reproduces_new(old in text_strategy(), new in text_strategy()) {
            let p = build_patch(&FILE, Some(old.as_bytes()), Some(new.as_bytes()), &LineSelection::all());
            match p {
                None => prop_assert_eq!(&old, &new),
                Some(p) => prop_assert_eq!(apply(old.as_bytes(), &p), new.as_bytes().to_vec()),
            }
        }

        #[test]
        fn hunk_by_hunk_reproduces_new(old in text_strategy(), new in text_strategy()) {
            // Applying each display hunk in turn (re-diffing after every step) ends at `new`.
            let mut cur = old.clone();
            for _ in 0..64 {
                let diff = crate::diff_lines(&cur, &new, &Default::default());
                let Some(h) = diff.hunks.first() else { break };
                let p = build_patch(&FILE, Some(cur.as_bytes()), Some(new.as_bytes()), &LineSelection::hunk(h))
                    .expect("a display hunk always yields a patch");
                cur = String::from_utf8(apply(cur.as_bytes(), &p)).unwrap();
            }
            prop_assert_eq!(cur, new);
        }

        #[test]
        fn partial_selection_applies_cleanly(old in text_strategy(), new in text_strategy(),
                                              picks in proptest::collection::vec(any::<bool>(), 32)) {
            let sel = LineSelection {
                old: (0..16u32).filter(|i| picks[*i as usize]).map(|i| i..i + 1).collect(),
                new: (0..16u32).filter(|i| picks[16 + *i as usize]).map(|i| i..i + 1).collect(),
            };
            if let Some(p) = build_patch(&FILE, Some(old.as_bytes()), Some(new.as_bytes()), &sel) {
                let _ = apply(old.as_bytes(), &p); // context and counts must match
            }
        }
    }
}
