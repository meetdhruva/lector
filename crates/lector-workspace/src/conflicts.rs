//! Merge conflicts in working-file text (UI-free): conflict-marker parsing
//! (`<<<<<<<`, optional diff3 / zdiff3 `|||||||` base section, `=======`,
//! `>>>>>>>`), resolution edits (accept ours / theirs / both / base) as
//! [`lector_buffer::TextEdit`]s, per-side projections for a three-pane merge
//! editor, and the repository's conflict state (re-exported from `lector-git`).
//!
//! Parsing never fails: nested, unterminated or stray markers are reported as
//! [`MarkerProblem`]s and the affected text is left alone. Line endings may be
//! `\n` or `\r\n` (byte ranges include them). Rows are 0-based.

use std::ops::Range;

use lector_buffer::TextEdit;
pub use lector_git::{ConflictEntry, ConflictKind, ConflictStage, OperationKind, RepoOperation};

/// The repository's conflict state: the operation in progress and the
/// conflicted paths (read-only; one call for a background refresh).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictState {
    /// Merge / rebase / cherry-pick / revert / am in progress.
    pub operation: Option<RepoOperation>,
    /// Conflicted index entries, sorted by path.
    pub entries: Vec<ConflictEntry>,
}

impl ConflictState {
    /// Whether nothing is in progress and nothing is conflicted.
    pub fn is_clean(&self) -> bool {
        self.operation.is_none() && self.entries.is_empty()
    }

    /// The entry of `path`.
    pub fn entry(&self, path: &str) -> Option<&ConflictEntry> {
        self.entries.iter().find(|e| e.path == path)
    }
}

/// Reads [`ConflictState`] (`.git` state files + index stages; never writes).
pub fn conflict_state(repo: &lector_git::Repo) -> crate::Result<ConflictState> {
    Ok(ConflictState {
        operation: repo.operation()?,
        entries: repo.conflicts()?,
    })
}

/// One stage of a conflicted path as text (lossy UTF-8; `None` when absent or binary).
pub fn stage_text(
    repo: &lector_git::Repo,
    path: &str,
    stage: ConflictStage,
) -> crate::Result<Option<String>> {
    Ok(repo
        .conflict_stage(path, stage)?
        .filter(|b| !lector_git::is_binary(b))
        .map(|b| String::from_utf8_lossy(&b).into_owned()))
}

/// Length of git's conflict markers (`merge.conflictMarkerSize` default).
pub const MARKER_LEN: usize = 7;

/// One conflict: `<<<<<<< ours` … [`||||||| base` …] `=======` … `>>>>>>> theirs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    /// The whole conflict, from the start of the `<<<<<<<` line to the end of
    /// the `>>>>>>>` line including its line terminator (if any).
    pub range: Range<usize>,
    /// "Current" content (between the `<<<<<<<` line and the next marker line).
    pub ours: Range<usize>,
    /// Common-ancestor content (diff3 / zdiff3 style), if present.
    pub base: Option<Range<usize>>,
    /// "Incoming" content (between `=======` and `>>>>>>>`).
    pub theirs: Range<usize>,
    /// Text after `<<<<<<< ` ("HEAD").
    pub ours_label: String,
    /// Text after `||||||| ` ("merged common ancestors", a sha).
    pub base_label: Option<String>,
    /// Text after `>>>>>>> ` ("feature", "abc1234 (Fix parser)").
    pub theirs_label: String,
    /// Row of the `<<<<<<<` line.
    pub start_row: u32,
    /// Row of the `|||||||` line, if any.
    pub base_row: Option<u32>,
    /// Row of the `=======` line.
    pub separator_row: u32,
    /// Row of the `>>>>>>>` line.
    pub end_row: u32,
}

impl ConflictRegion {
    /// Rows of the ours content (`start_row + 1 .. base_row | separator_row`).
    pub fn ours_rows(&self) -> Range<u32> {
        self.start_row + 1..self.base_row.unwrap_or(self.separator_row)
    }

    /// Rows of the base content, if present.
    pub fn base_rows(&self) -> Option<Range<u32>> {
        self.base_row.map(|b| b + 1..self.separator_row)
    }

    /// Rows of the theirs content.
    pub fn theirs_rows(&self) -> Range<u32> {
        self.separator_row + 1..self.end_row
    }

    /// All rows of the conflict, markers included.
    pub fn rows(&self) -> Range<u32> {
        self.start_row..self.end_row + 1
    }

    /// The text of one side (`Base` is empty when the region has no base section).
    pub fn side<'a>(&self, text: &'a str, side: Side) -> &'a str {
        let range = match side {
            Side::Ours => Some(&self.ours),
            Side::Theirs => Some(&self.theirs),
            Side::Base => self.base.as_ref(),
        };
        range.and_then(|r| text.get(r.clone())).unwrap_or("")
    }

    /// The three contents, for comparing regions across edits.
    pub fn contents(&self, text: &str) -> ConflictContents {
        ConflictContents {
            ours: self.side(text, Side::Ours).to_string(),
            base: self
                .base
                .as_ref()
                .map(|_| self.side(text, Side::Base).to_string()),
            theirs: self.side(text, Side::Theirs).to_string(),
        }
    }
}

/// A side of a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Side {
    /// Current (`HEAD`, stage 2).
    Ours,
    /// Incoming (stage 3).
    Theirs,
    /// Common ancestor (stage 1; diff3 sections).
    Base,
}

/// The contents of a conflict's sides (owned), see [`match_regions`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ConflictContents {
    /// Ours.
    pub ours: String,
    /// Base, if the region has a base section.
    pub base: Option<String>,
    /// Theirs.
    pub theirs: String,
}

/// What is wrong with a marker line that is not part of a well-formed conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProblemKind {
    /// `<<<<<<<` without a matching `=======` / `>>>>>>>` before the end of the text.
    Unterminated,
    /// `<<<<<<<` inside an open conflict (the open one is abandoned; parsing
    /// restarts at this line).
    Nested,
    /// `>>>>>>>` before any `=======` in an open conflict (it is abandoned).
    MissingSeparator,
    /// `>>>>>>>` or `|||||||` outside any conflict.
    Stray,
}

/// A marker problem at `row` (reported, never fatal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkerProblem {
    /// Row of the offending marker line (for `Unterminated`: the `<<<<<<<` line).
    pub row: u32,
    /// What is wrong.
    pub kind: ProblemKind,
}

/// The result of [`parse_conflicts`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ConflictParse {
    /// Well-formed conflicts in text order.
    pub regions: Vec<ConflictRegion>,
    /// Marker lines that did not form a conflict.
    pub problems: Vec<MarkerProblem>,
}

impl ConflictParse {
    /// Index of the conflict containing `row` (markers included).
    pub fn region_at_row(&self, row: u32) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| r.start_row <= row && row <= r.end_row)
    }

    /// Index of the first conflict starting after `row` (wrapping when `wrap`).
    pub fn next_after(&self, row: u32, wrap: bool) -> Option<usize> {
        self.regions
            .iter()
            .position(|r| r.start_row > row)
            .or_else(|| (wrap && !self.regions.is_empty()).then_some(0))
    }

    /// Index of the last conflict starting before `row` (wrapping when `wrap`).
    pub fn prev_before(&self, row: u32, wrap: bool) -> Option<usize> {
        self.regions
            .iter()
            .rposition(|r| r.start_row < row)
            .or_else(|| (wrap && !self.regions.is_empty()).then(|| self.regions.len() - 1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Marker {
    Start,
    Base,
    Separator,
    End,
}

/// Classifies a line (without terminator): exactly [`MARKER_LEN`] marker
/// characters, then end of line or a space and a label (`=======` takes no
/// label but tolerates trailing whitespace). Returns the marker and its label.
fn marker(line: &str) -> Option<(Marker, &str)> {
    let first = *line.as_bytes().first()?;
    let kind = match first {
        b'<' => Marker::Start,
        b'|' => Marker::Base,
        b'=' => Marker::Separator,
        b'>' => Marker::End,
        _ => return None,
    };
    let bytes = line.as_bytes();
    if bytes.len() < MARKER_LEN || bytes[..MARKER_LEN].iter().any(|b| *b != first) {
        return None;
    }
    let rest = &line[MARKER_LEN..];
    if kind == Marker::Separator {
        return rest.trim().is_empty().then_some((kind, ""));
    }
    if rest.is_empty() {
        return Some((kind, ""));
    }
    let label = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'))?;
    Some((kind, label.trim_end()))
}

/// Quick check: does any line start with a `<<<<<<<` marker?
pub fn has_conflict_markers(text: &str) -> bool {
    let mut at = 0;
    while let Some(ix) = text[at..].find("<<<<<<<") {
        let pos = at + ix;
        let line_start = pos == 0 || text.as_bytes()[pos - 1] == b'\n';
        if line_start {
            let end = text[pos..].find('\n').map_or(text.len(), |e| pos + e);
            let line = text[pos..end].strip_suffix('\r').unwrap_or(&text[pos..end]);
            if marker(line).is_some_and(|(m, _)| m == Marker::Start) {
                return true;
            }
        }
        at = pos + 7;
    }
    false
}

struct Open {
    start: usize,
    start_row: u32,
    ours_label: String,
    ours_start: usize,
    base: Option<(usize, u32, String, usize)>, // (marker start, row, label, content start)
    separator: Option<(usize, u32, usize)>,    // (marker start, row, content start)
}

/// Parses every conflict in `text`. Never fails; see [`ConflictParse::problems`].
pub fn parse_conflicts(text: &str) -> ConflictParse {
    let mut out = ConflictParse::default();
    let mut open: Option<Open> = None;
    let mut pos = 0usize;
    let mut row = 0u32;
    while pos < text.len() {
        let (line_end, next) = match text[pos..].find('\n') {
            Some(i) => (pos + i, pos + i + 1),
            None => (text.len(), text.len()),
        };
        let raw = &text[pos..line_end];
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if let Some((m, label)) = marker(line) {
            match (m, open.as_mut()) {
                (Marker::Start, current) => {
                    if let Some(o) = current {
                        out.problems.push(MarkerProblem {
                            row: o.start_row,
                            kind: ProblemKind::Unterminated,
                        });
                        out.problems.push(MarkerProblem {
                            row,
                            kind: ProblemKind::Nested,
                        });
                    }
                    open = Some(Open {
                        start: pos,
                        start_row: row,
                        ours_label: label.to_string(),
                        ours_start: next,
                        base: None,
                        separator: None,
                    });
                }
                (Marker::Base, Some(o)) if o.base.is_none() && o.separator.is_none() => {
                    o.base = Some((pos, row, label.to_string(), next));
                }
                (Marker::Base, Some(_)) => {} // content (e.g. inside theirs)
                (Marker::Separator, Some(o)) if o.separator.is_none() => {
                    o.separator = Some((pos, row, next));
                }
                (Marker::Separator, _) => {} // `=======` is common in prose; content
                (Marker::End, Some(o)) => match o.separator {
                    Some((sep_start, sep_row, theirs_start)) => {
                        let ours_end = o.base.as_ref().map_or(sep_start, |b| b.0);
                        out.regions.push(ConflictRegion {
                            range: o.start..next,
                            ours: o.ours_start..ours_end,
                            base: o.base.as_ref().map(|b| b.3..sep_start),
                            theirs: theirs_start..pos,
                            ours_label: std::mem::take(&mut o.ours_label),
                            base_label: o.base.as_ref().map(|b| b.2.clone()),
                            theirs_label: label.to_string(),
                            start_row: o.start_row,
                            base_row: o.base.as_ref().map(|b| b.1),
                            separator_row: sep_row,
                            end_row: row,
                        });
                        open = None;
                    }
                    None => {
                        out.problems.push(MarkerProblem {
                            row,
                            kind: ProblemKind::MissingSeparator,
                        });
                        open = None;
                    }
                },
                (Marker::Base | Marker::End, None) => out.problems.push(MarkerProblem {
                    row,
                    kind: ProblemKind::Stray,
                }),
            }
        }
        if next == pos {
            break;
        }
        pos = next;
        row += 1;
    }
    if let Some(o) = open {
        out.problems.push(MarkerProblem {
            row: o.start_row,
            kind: ProblemKind::Unterminated,
        });
    }
    out
}

/// How to resolve a conflict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Resolution {
    /// Keep the current side ("Accept Current").
    Ours,
    /// Keep the incoming side ("Accept Incoming").
    Theirs,
    /// Ours, then theirs ("Accept Both").
    OursThenTheirs,
    /// Theirs, then ours.
    TheirsThenOurs,
    /// The common ancestor (empty when the region has no base section).
    Base,
}

/// The replacement text for `region`. When the `>>>>>>>` line was the last
/// line without a terminator, the replacement's final line break is dropped
/// so the file keeps ending the way it did.
pub fn resolved_text(text: &str, region: &ConflictRegion, resolution: Resolution) -> String {
    let ours = region.side(text, Side::Ours);
    let theirs = region.side(text, Side::Theirs);
    let mut out = match resolution {
        Resolution::Ours => ours.to_string(),
        Resolution::Theirs => theirs.to_string(),
        Resolution::Base => region.side(text, Side::Base).to_string(),
        Resolution::OursThenTheirs => join_sides(ours, theirs),
        Resolution::TheirsThenOurs => join_sides(theirs, ours),
    };
    let ends_with_break = text[..region.range.end].ends_with('\n');
    if !ends_with_break {
        if out.ends_with("\r\n") {
            out.truncate(out.len() - 2);
        } else if out.ends_with('\n') {
            out.truncate(out.len() - 1);
        }
    }
    out
}

/// Concatenates two sides; inserts a line break between them if the first
/// lacks one (can only happen with hand-edited markers).
fn join_sides(a: &str, b: &str) -> String {
    let mut s = String::with_capacity(a.len() + b.len() + 1);
    s.push_str(a);
    if !a.is_empty() && !a.ends_with('\n') && !b.is_empty() {
        s.push('\n');
    }
    s.push_str(b);
    s
}

/// The edit resolving `region` (offsets into `text`, the text it was parsed from).
pub fn resolve(text: &str, region: &ConflictRegion, resolution: Resolution) -> TextEdit {
    TextEdit {
        range: region.range.clone(),
        new_text: resolved_text(text, region, resolution),
    }
}

/// Edits resolving every region the same way (non-overlapping, ascending).
pub fn resolve_all(
    text: &str,
    regions: &[ConflictRegion],
    resolution: Resolution,
) -> Vec<TextEdit> {
    regions
        .iter()
        .map(|r| resolve(text, r, resolution))
        .collect()
}

/// Applies non-overlapping edits (ascending or not) to `text` (tests, previews).
pub fn apply_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut sorted: Vec<&TextEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.range.start);
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for e in sorted {
        if e.range.start < at || e.range.end > text.len() {
            continue;
        }
        out.push_str(&text[at..e.range.start]);
        out.push_str(&e.new_text);
        at = e.range.end;
    }
    out.push_str(&text[at..]);
    out
}

/// One side of a conflicted file with every conflict replaced by that side's
/// content (the "Current" / "Incoming" / "Base" panes of a merge editor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideView {
    /// The text.
    pub text: String,
    /// Per conflict (same order as the parse): the rows its content occupies
    /// here (empty range = the side is empty; `start` is where it would be).
    pub rows: Vec<Range<u32>>,
    /// For `Side::Base`: false when some conflict had no base section.
    pub complete: bool,
}

fn count_rows(s: &str) -> u32 {
    s.bytes().filter(|b| *b == b'\n').count() as u32
}

/// Builds the [`SideView`] of `side`. Text outside conflicts is kept as is.
pub fn side_view(text: &str, parse: &ConflictParse, side: Side) -> SideView {
    let mut out = String::with_capacity(text.len());
    let mut rows = Vec::with_capacity(parse.regions.len());
    let mut complete = true;
    let mut at = 0;
    let mut row = 0u32;
    for region in &parse.regions {
        let before = &text[at..region.range.start];
        out.push_str(before);
        row += count_rows(before);
        if side == Side::Base && region.base.is_none() {
            complete = false;
        }
        let content = region.side(text, side);
        let n = count_rows(content) + u32::from(!content.is_empty() && !content.ends_with('\n'));
        rows.push(row..row + n);
        out.push_str(content);
        if !content.is_empty() && !content.ends_with('\n') {
            out.push('\n');
        }
        row += n;
        at = region.range.end;
    }
    out.push_str(&text[at..]);
    SideView {
        text: out,
        rows,
        complete,
    }
}

/// Maps the conflicts of an earlier parse (`original`) to those of a later one
/// (`current`, after some were resolved by edits): for each original, the
/// index of the current conflict it became, `None` = resolved. Identical
/// contents are aligned first (longest common subsequence); a current conflict
/// that was edited but is still a conflict pairs with an unmatched original in
/// the same gap, by position.
pub fn match_regions(
    original: &[ConflictContents],
    current: &[ConflictContents],
) -> Vec<Option<usize>> {
    let (n, m) = (original.len(), current.len());
    // lcs[i][j] = LCS length of original[i..] and current[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if original[i] == current[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = vec![None; n];
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if original[i] == current[j] && lcs[i][j] == lcs[i + 1][j + 1] + 1 {
            out[i] = Some(j);
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    // Pair leftovers inside each gap between matched anchors, in order.
    let mut used = vec![false; m];
    for c in out.iter().flatten() {
        used[*c] = true;
    }
    let mut oi = 0;
    let mut cj = 0;
    while oi < n || cj < m {
        // Next anchor at or after (oi, cj).
        let anchor = (oi..n).find(|k| out[*k].is_some());
        let (o_end, c_end) = match anchor {
            Some(k) => (k, out[k].unwrap_or(m)),
            None => (n, m),
        };
        let free_o: Vec<usize> = (oi..o_end).filter(|k| out[*k].is_none()).collect();
        let free_c: Vec<usize> = (cj..c_end).filter(|k| !used[*k]).collect();
        for (o, c) in free_o.into_iter().zip(free_c) {
            out[o] = Some(c);
            used[c] = true;
        }
        if anchor.is_none() {
            break;
        }
        oi = o_end + 1;
        cj = c_end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE: &str = "a\n<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs\n>>>>>>> feature\nz\n";

    #[test]
    fn parses_a_simple_conflict() {
        let p = parse_conflicts(SIMPLE);
        assert!(p.problems.is_empty());
        assert_eq!(p.regions.len(), 1);
        let r = &p.regions[0];
        assert_eq!(
            &SIMPLE[r.range.clone()],
            "<<<<<<< HEAD\nours 1\nours 2\n=======\ntheirs\n>>>>>>> feature\n"
        );
        assert_eq!(r.side(SIMPLE, Side::Ours), "ours 1\nours 2\n");
        assert_eq!(r.side(SIMPLE, Side::Theirs), "theirs\n");
        assert_eq!(r.side(SIMPLE, Side::Base), "");
        assert_eq!(r.ours_label, "HEAD");
        assert_eq!(r.theirs_label, "feature");
        assert_eq!((r.start_row, r.separator_row, r.end_row), (1, 4, 6));
        assert_eq!(r.ours_rows(), 2..4);
        assert_eq!(r.theirs_rows(), 5..6);
        assert_eq!(r.rows(), 1..7);
        assert!(has_conflict_markers(SIMPLE));
        assert!(!has_conflict_markers("a <<<<<<< b\n"));
        assert!(!has_conflict_markers("<<<<<<<< eight\n"));
        assert!(has_conflict_markers("<<<<<<<\n"));
    }

    #[test]
    fn resolutions() {
        let p = parse_conflicts(SIMPLE);
        let r = &p.regions[0];
        let apply = |res| apply_edits(SIMPLE, &[resolve(SIMPLE, r, res)]);
        assert_eq!(apply(Resolution::Ours), "a\nours 1\nours 2\nz\n");
        assert_eq!(apply(Resolution::Theirs), "a\ntheirs\nz\n");
        assert_eq!(
            apply(Resolution::OursThenTheirs),
            "a\nours 1\nours 2\ntheirs\nz\n"
        );
        assert_eq!(
            apply(Resolution::TheirsThenOurs),
            "a\ntheirs\nours 1\nours 2\nz\n"
        );
        assert_eq!(apply(Resolution::Base), "a\nz\n");
    }

    #[test]
    fn diff3_and_zdiff3_base_sections() {
        let text = "x\n<<<<<<< HEAD\nmine\n||||||| merged common ancestors\norig\n=======\nyours\n>>>>>>> topic\n";
        let p = parse_conflicts(text);
        assert!(p.problems.is_empty());
        let r = &p.regions[0];
        assert_eq!(r.side(text, Side::Ours), "mine\n");
        assert_eq!(r.side(text, Side::Base), "orig\n");
        assert_eq!(r.side(text, Side::Theirs), "yours\n");
        assert_eq!(r.base_label.as_deref(), Some("merged common ancestors"));
        assert_eq!(r.base_rows(), Some(4..5));
        assert_eq!(r.ours_rows(), 2..3);
        assert_eq!(
            apply_edits(text, &[resolve(text, r, Resolution::Base)]),
            "x\norig\n"
        );
        // zdiff3 with an empty base and a bare `|||||||` marker.
        let z = "<<<<<<< ours\na\n|||||||\n=======\nb\n>>>>>>>\n";
        let p = parse_conflicts(z);
        let r = &p.regions[0];
        assert!(r.base.is_some());
        assert_eq!(r.side(z, Side::Base), "");
        assert_eq!(r.theirs_label, "");
        assert_eq!(r.base_label.as_deref(), Some(""));
    }

    #[test]
    fn crlf_is_kept() {
        let text = "a\r\n<<<<<<< HEAD\r\nours\r\n=======\r\ntheirs\r\n>>>>>>> b\r\nz\r\n";
        let p = parse_conflicts(text);
        assert!(p.problems.is_empty());
        let r = &p.regions[0];
        assert_eq!(r.ours_label, "HEAD");
        assert_eq!(r.theirs_label, "b");
        assert_eq!(r.side(text, Side::Ours), "ours\r\n");
        assert_eq!(
            apply_edits(text, &[resolve(text, r, Resolution::OursThenTheirs)]),
            "a\r\nours\r\ntheirs\r\nz\r\n"
        );
        assert!(has_conflict_markers(text));
    }

    #[test]
    fn end_marker_at_eof_without_newline() {
        let text = "<<<<<<< HEAD\nours\n=======\ntheirs\n>>>>>>> b";
        let p = parse_conflicts(text);
        let r = &p.regions[0];
        assert_eq!(r.range, 0..text.len());
        assert_eq!(
            apply_edits(text, &[resolve(text, r, Resolution::Ours)]),
            "ours"
        );
        assert_eq!(
            apply_edits(text, &[resolve(text, r, Resolution::OursThenTheirs)]),
            "ours\ntheirs"
        );
    }

    #[test]
    fn empty_sides() {
        let text = "<<<<<<< HEAD\n=======\nadded\n>>>>>>> b\n";
        let p = parse_conflicts(text);
        let r = &p.regions[0];
        assert_eq!(r.ours, r.ours.start..r.ours.start);
        assert_eq!(r.ours_rows(), 1..1);
        assert_eq!(apply_edits(text, &[resolve(text, r, Resolution::Ours)]), "");
        assert_eq!(
            apply_edits(text, &[resolve(text, r, Resolution::OursThenTheirs)]),
            "added\n"
        );
    }

    #[test]
    fn several_conflicts_resolve_all() {
        let text = format!("{SIMPLE}mid\n{SIMPLE}");
        let p = parse_conflicts(&text);
        assert_eq!(p.regions.len(), 2);
        assert_eq!(p.regions[1].start_row, 10);
        let all = resolve_all(&text, &p.regions, Resolution::Theirs);
        assert_eq!(
            apply_edits(&text, &all),
            "a\ntheirs\nz\nmid\na\ntheirs\nz\n"
        );
        assert_eq!(p.region_at_row(3), Some(0));
        assert_eq!(p.region_at_row(8), None);
        assert_eq!(p.next_after(1, false), Some(1));
        assert_eq!(p.next_after(10, false), None);
        assert_eq!(p.next_after(10, true), Some(0));
        assert_eq!(p.prev_before(9, false), Some(0));
        assert_eq!(p.prev_before(0, true), Some(1));
    }

    #[test]
    fn garbled_markers_are_reported_not_fatal() {
        // Nested start: the first conflict is abandoned, the inner one parses.
        let text = "<<<<<<< a\nx\n<<<<<<< b\ny\n=======\nz\n>>>>>>> c\n";
        let p = parse_conflicts(text);
        assert_eq!(p.regions.len(), 1);
        assert_eq!(p.regions[0].start_row, 2);
        assert_eq!(
            p.problems,
            [
                MarkerProblem {
                    row: 0,
                    kind: ProblemKind::Unterminated
                },
                MarkerProblem {
                    row: 2,
                    kind: ProblemKind::Nested
                },
            ]
        );
        // Unterminated.
        let p = parse_conflicts("<<<<<<< a\nx\n=======\ny\n");
        assert!(p.regions.is_empty());
        assert_eq!(
            p.problems,
            [MarkerProblem {
                row: 0,
                kind: ProblemKind::Unterminated
            }]
        );
        // End before separator.
        let p = parse_conflicts("<<<<<<< a\nx\n>>>>>>> b\n");
        assert!(p.regions.is_empty());
        assert_eq!(p.problems[0].kind, ProblemKind::MissingSeparator);
        // Stray markers; `=======` alone is content (Markdown/RST underlines).
        let p = parse_conflicts("Title\n=======\n>>>>>>> x\n||||||| y\n");
        assert!(p.regions.is_empty());
        assert_eq!(
            p.problems
                .iter()
                .map(|p| (p.row, p.kind))
                .collect::<Vec<_>>(),
            [(2, ProblemKind::Stray), (3, ProblemKind::Stray)]
        );
        // Lookalikes are not markers.
        let p = parse_conflicts("<<<<<<<< 8\n<<<<<<<x\n>>>>>>>>\n");
        assert_eq!(p, ConflictParse::default());
        // Odd bytes never panic.
        let p = parse_conflicts("<<<<<<< é\n\u{0}\n=======\n\r\n>>>>>>> ü");
        assert_eq!(p.regions.len(), 1);
    }

    #[test]
    fn side_views_and_rows() {
        let text = format!("{SIMPLE}mid\n<<<<<<< HEAD\n=======\nt1\nt2\n>>>>>>> f\n");
        let p = parse_conflicts(&text);
        let ours = side_view(&text, &p, Side::Ours);
        assert_eq!(ours.text, "a\nours 1\nours 2\nz\nmid\n");
        assert_eq!(ours.rows, [1..3, 5..5]);
        let theirs = side_view(&text, &p, Side::Theirs);
        assert_eq!(theirs.text, "a\ntheirs\nz\nmid\nt1\nt2\n");
        assert_eq!(theirs.rows, [1..2, 4..6]);
        assert!(theirs.complete);
        let base = side_view(&text, &p, Side::Base);
        assert!(!base.complete);
        assert_eq!(base.text, "a\nz\nmid\n");
    }

    #[test]
    fn matching_regions_across_edits() {
        let c = |o: &str, t: &str| ConflictContents {
            ours: o.into(),
            base: None,
            theirs: t.into(),
        };
        let orig = [c("a", "b"), c("c", "d"), c("a", "b")];
        // Middle one resolved.
        assert_eq!(
            match_regions(&orig, &[c("a", "b"), c("a", "b")]),
            [Some(0), None, Some(1)]
        );
        // First resolved: the identical third still maps to the last.
        assert_eq!(
            match_regions(&orig, &[c("c", "d"), c("a", "b")]),
            [None, Some(0), Some(1)]
        );
        // Middle edited (still a conflict): paired by position.
        assert_eq!(
            match_regions(&orig, &[c("a", "b"), c("c!", "d"), c("a", "b")]),
            [Some(0), Some(1), Some(2)]
        );
        assert_eq!(match_regions(&orig, &[]), [None, None, None]);
    }

    mod props {
        use super::super::*;
        use proptest::prelude::*;

        /// Plain lines that are never markers.
        fn line() -> impl Strategy<Value = String> {
            "[a-z =<>|]{0,12}".prop_filter("not a marker", |l| marker(l).is_none())
        }

        fn lines(max: usize) -> impl Strategy<Value = Vec<String>> {
            proptest::collection::vec(line(), 0..max)
        }

        #[derive(Debug, Clone)]
        enum Chunk {
            Plain(Vec<String>),
            Conflict(Vec<String>, Option<Vec<String>>, Vec<String>),
        }

        fn chunk() -> impl Strategy<Value = Chunk> {
            prop_oneof![
                lines(4).prop_map(Chunk::Plain),
                (lines(4), proptest::option::of(lines(3)), lines(4))
                    .prop_map(|(o, b, t)| Chunk::Conflict(o, b, t)),
            ]
        }

        fn join(ls: &[String], eol: &str) -> String {
            ls.iter().map(|l| format!("{l}{eol}")).collect()
        }

        proptest! {
            #[test]
            fn generated_conflicts_round_trip(chunks in proptest::collection::vec(chunk(), 0..6), crlf in any::<bool>()) {
                let eol = if crlf { "\r\n" } else { "\n" };
                let mut text = String::new();
                let (mut ours, mut theirs) = (String::new(), String::new());
                let mut count = 0;
                for c in &chunks {
                    match c {
                        Chunk::Plain(ls) => {
                            let s = join(ls, eol);
                            text.push_str(&s);
                            ours.push_str(&s);
                            theirs.push_str(&s);
                        }
                        Chunk::Conflict(o, b, t) => {
                            count += 1;
                            text.push_str(&format!("<<<<<<< HEAD{eol}"));
                            text.push_str(&join(o, eol));
                            if let Some(b) = b {
                                text.push_str(&format!("||||||| base{eol}"));
                                text.push_str(&join(b, eol));
                            }
                            text.push_str(&format!("======={eol}"));
                            text.push_str(&join(t, eol));
                            text.push_str(&format!(">>>>>>> theirs{eol}"));
                            ours.push_str(&join(o, eol));
                            theirs.push_str(&join(t, eol));
                        }
                    }
                }
                let p = parse_conflicts(&text);
                prop_assert!(p.problems.is_empty());
                prop_assert_eq!(p.regions.len(), count);
                prop_assert_eq!(apply_edits(&text, &resolve_all(&text, &p.regions, Resolution::Ours)), ours.clone());
                prop_assert_eq!(apply_edits(&text, &resolve_all(&text, &p.regions, Resolution::Theirs)), theirs.clone());
                prop_assert_eq!(side_view(&text, &p, Side::Ours).text, ours);
                prop_assert_eq!(side_view(&text, &p, Side::Theirs).text, theirs);
                prop_assert_eq!(has_conflict_markers(&text), count > 0);
                let contents: Vec<_> = p.regions.iter().map(|r| r.contents(&text)).collect();
                let identity: Vec<_> = (0..count).map(Some).collect();
                prop_assert_eq!(match_regions(&contents, &contents), identity);
            }

            #[test]
            fn never_panics(text in "(<<<<<<< ?|=======|>>>>>>> ?|\\|\\|\\|\\|\\|\\|\\| ?|[a-z]{0,3}|\r|\n|é){0,40}") {
                let p = parse_conflicts(&text);
                for r in &p.regions {
                    prop_assert!(r.range.end <= text.len());
                    prop_assert!(r.ours.start <= r.ours.end && r.theirs.start <= r.theirs.end);
                    prop_assert!(r.start_row < r.separator_row && r.separator_row < r.end_row);
                    let _ = resolved_text(&text, r, Resolution::OursThenTheirs);
                }
                let _ = side_view(&text, &p, Side::Base);
            }
        }
    }
}
