//! `lector changes` (files with stats and risk) and `lector diff` (hunks, moved blocks).

use std::ops::Range;

use anyhow::{anyhow, bail, Context as _};
use lector_app::commands::read::file_diff_json;
use lector_diff::moves::{detect_moved, detect_moved_across, CrossFileMove, MovedBlock};
use lector_git::{ChangeKind, Changeset, Repo};
use lector_workspace::review::{self, ChangesetView, DiffKind, FileDiff, ReviewFile};
use lector_workspace::risk::{rank_files_with, RiskLevel, RiskReport};
use lector_workspace::Project;
use serde_json::{json, Value};

use crate::args::{ChangesArgs, DiffArgs};
use crate::out::{fit, print_json, truncate, Io, Style};
use crate::session::{open_core, open_repo_project, resolve_dir};

/// Above this many files `changes` skips the risk analysis (it parses both sides of each).
pub const MAX_RISK_FILES: usize = 500;

/// Parses a changeset given on the command line: a review key (`worktree`, `staged`,
/// `unstaged`, `untracked`, `commit:<sha>`, `range:<a>..<b>`, `branch:<base>...<branch>`),
/// `A...B` (merge base of A and B to B), `A..B`, or any single revision (that commit
/// against its first parent).
pub fn parse_changeset(repo: &Repo, spec: &str) -> anyhow::Result<Changeset> {
    let spec = spec.trim();
    if spec.is_empty() {
        bail!("empty changeset");
    }
    if let Some(cs) = review::parse_changeset_key(spec) {
        return Ok(cs);
    }
    if let Some(rev) = spec.strip_prefix("commit:") {
        return commit_changeset(repo, rev);
    }
    if let Some((a, b)) = spec.split_once("...") {
        return Ok(Changeset::BranchVsBase {
            base: or_head(a),
            branch: or_head(b),
        });
    }
    if let Some((a, b)) = spec.split_once("..") {
        return Ok(Changeset::Range {
            from: or_head(a),
            to: or_head(b),
        });
    }
    commit_changeset(repo, spec).with_context(|| {
        format!(
            "`{spec}` is not a changeset: use worktree, staged, unstaged, untracked, commit:<sha>, \
             range:<a>..<b>, branch:<base>...<branch>, a revision, or A..B / A...B"
        )
    })
}

fn or_head(rev: &str) -> String {
    if rev.is_empty() {
        "HEAD".into()
    } else {
        rev.into()
    }
}

fn commit_changeset(repo: &Repo, rev: &str) -> anyhow::Result<Changeset> {
    let oid = repo
        .resolve(rev)
        .map_err(|e| anyhow!("cannot resolve `{rev}`: {e}"))?;
    Ok(Changeset::Commit(oid))
}

/// `PublicApiChanged` → `public_api_changed`.
pub fn snake_case(name: &str) -> String {
    let mut out = String::new();
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `low` / `medium` / `high`.
pub fn level_name(level: RiskLevel) -> &'static str {
    match level {
        RiskLevel::Low => "low",
        RiskLevel::Medium => "medium",
        RiskLevel::High => "high",
    }
}

fn risk_json(report: &RiskReport) -> Value {
    let reasons: Vec<Value> = report
        .reasons
        .iter()
        .map(|r| {
            let mut v = json!({
                "kind": snake_case(&format!("{:?}", r.kind)),
                "weight": round3(r.weight),
                "message": r.message,
            });
            if r.kind.is_dampening() {
                v["dampening"] = json!(true);
            }
            if let Some(line) = r.line {
                v["line"] = json!(line);
            }
            v
        })
        .collect();
    json!({
        "score": round3(report.score),
        "level": level_name(report.level()),
        "reasons": reasons,
    })
}

/// `f32` scores as short JSON numbers (`0.45`, not `0.44999998807907104`).
fn round3(x: f32) -> f64 {
    (f64::from(x) * 1000.0).round() / 1000.0
}

fn kind_name(kind: ChangeKind) -> String {
    format!("{kind:?}").to_lowercase()
}

fn kind_letter(kind: ChangeKind) -> char {
    match kind {
        ChangeKind::Added => 'A',
        ChangeKind::Modified => 'M',
        ChangeKind::Deleted => 'D',
        ChangeKind::Renamed => 'R',
        ChangeKind::Copied => 'C',
        ChangeKind::TypeChange => 'T',
    }
}

/// Risk reports aligned with `view.files` (`None`: the diff could not be loaded).
/// `Err` when there are too many files.
pub fn assess_files(
    project: &Project,
    view: &ChangesetView,
) -> Result<Vec<Option<RiskReport>>, usize> {
    if view.files.len() > MAX_RISK_FILES {
        return Err(view.files.len());
    }
    let opts = review::diff_options(&project.settings().diff);
    let mut diffs: Vec<FileDiff> = Vec::new();
    let mut origin: Vec<usize> = Vec::new();
    for (i, file) in view.files.iter().enumerate() {
        match review::load_file_diff(project, &file.change, &opts) {
            Ok(diff) => {
                diffs.push(diff);
                origin.push(i);
            }
            Err(err) => tracing::debug!("no diff for {}: {err}", file.path()),
        }
    }
    let mut out: Vec<Option<RiskReport>> = vec![None; view.files.len()];
    for (loaded_ix, report) in rank_files_with(&diffs, project.languages()) {
        out[origin[loaded_ix]] = Some(report);
    }
    Ok(out)
}

/// One row of `changes`.
struct Row<'a> {
    file: &'a ReviewFile,
    risk: Option<RiskReport>,
}

fn file_json(row: &Row) -> Value {
    let f = row.file;
    let mut v = json!({
        "path": f.path(),
        "kind": kind_name(f.change.kind),
    });
    if let (Some(old), Some(new)) = (&f.change.old_path, &f.change.new_path) {
        if old != new {
            v["old_path"] = json!(old);
        }
    }
    if let Some(s) = f.stats {
        v["added"] = json!(s.added);
        v["removed"] = json!(s.removed);
    }
    if f.binary {
        v["binary"] = json!(true);
    }
    if f.viewed {
        v["viewed"] = json!(true);
    }
    if let Some(risk) = &row.risk {
        v["risk"] = risk_json(risk);
    }
    v
}

/// Picks the changeset of `lector changes` from its flags.
fn changes_changeset(args: &ChangesArgs, project: &Project) -> anyhow::Result<Changeset> {
    let repo = project
        .repo()
        .ok_or_else(|| anyhow!("not inside a git repository"))?;
    if args.staged {
        return Ok(Changeset::Staged);
    }
    if args.unstaged {
        return Ok(Changeset::Unstaged);
    }
    if let Some(base) = &args.branch {
        let head = repo.head().context("cannot read HEAD")?;
        return Ok(Changeset::BranchVsBase {
            branch: head.branch.unwrap_or_else(|| "HEAD".into()),
            base: base.clone(),
        });
    }
    if let Some(range) = &args.range {
        if !range.contains("..") {
            bail!("--range expects A..B or A...B, got `{range}`");
        }
        return parse_changeset(repo, range);
    }
    Ok(Changeset::WorkingTree)
}

/// Runs `lector changes`.
pub fn run_changes(args: &ChangesArgs, io: &mut Io) -> anyhow::Result<()> {
    let dir = resolve_dir(args.directory.as_deref())?;
    let project = open_repo_project(&dir)?;
    let cs = changes_changeset(args, &project)?;
    let core = open_core();
    let view = review::load_changeset(&project, &cs, &core.review_store())
        .with_context(|| "cannot load the changes")?;

    let mut risk_skipped = None;
    let risks = if args.no_risk {
        vec![None; view.files.len()]
    } else {
        match assess_files(&project, &view) {
            Ok(r) => r,
            Err(n) => {
                risk_skipped = Some(n);
                vec![None; view.files.len()]
            }
        }
    };
    let mut rows: Vec<Row> = view
        .files
        .iter()
        .zip(risks)
        .map(|(file, risk)| Row { file, risk })
        .collect();
    // Riskiest first; files without a score (or all, with --no-risk) by path.
    rows.sort_by(|a, b| {
        let sa = a.risk.as_ref().map_or(-1.0, |r| r.score);
        let sb = b.risk.as_ref().map_or(-1.0, |r| r.score);
        sb.total_cmp(&sa)
            .then_with(|| a.file.path().cmp(b.file.path()))
    });

    let totals = view.total_stats();
    let count_level = |l: RiskLevel| {
        rows.iter()
            .filter(|r| r.risk.as_ref().is_some_and(|x| x.level() == l))
            .count()
    };
    let (high, medium, low) = (
        count_level(RiskLevel::High),
        count_level(RiskLevel::Medium),
        count_level(RiskLevel::Low),
    );

    if args.json {
        let mut data = json!({
            "changeset": view.key,
            "title": view.title,
            "files": rows.iter().map(file_json).collect::<Vec<_>>(),
            "totals": {
                "files": view.files.len(),
                "added": totals.added,
                "removed": totals.removed,
                "high": high,
                "medium": medium,
                "low": low,
            },
        });
        if let Some(n) = risk_skipped {
            data["risk_skipped"] = json!(format!(
                "{n} files (more than {MAX_RISK_FILES}); use `lector diff` per file"
            ));
        }
        if args.no_risk {
            data["risk_skipped"] = json!("--no-risk");
        }
        return Ok(print_json(io.out, data)?);
    }

    let s = io.style;
    if rows.is_empty() {
        writeln!(
            io.out,
            "{}",
            s.dim(&format!("No changes in {}.", view.title))
        )?;
        return Ok(());
    }
    let mut head = format!(
        "{} {} {} file{}, {} {}",
        s.bold(&view.title),
        s.dim("·"),
        rows.len(),
        if rows.len() == 1 { "" } else { "s" },
        s.green(&format!("+{}", totals.added)),
        s.red(&format!("-{}", totals.removed)),
    );
    if high + medium > 0 {
        let mut parts = Vec::new();
        if high > 0 {
            parts.push(s.red(&format!("{high} high")));
        }
        if medium > 0 {
            parts.push(s.yellow(&format!("{medium} medium")));
        }
        head.push_str(&format!(" {} {}", s.dim("·"), parts.join(", ")));
    }
    writeln!(io.out, "{head}")?;
    if let Some(n) = risk_skipped {
        writeln!(
            io.out,
            "{}",
            s.dim(&format!(
                "(risk not computed: {n} files; use `lector diff` per file)"
            ))
        )?;
    }
    let width = rows
        .iter()
        .map(|r| r.file.display_path.chars().count())
        .max()
        .unwrap_or(0)
        .min(56);
    for row in &rows {
        writeln!(io.out, "{}", human_row(row, width, &s))?;
    }
    Ok(())
}

fn human_row(row: &Row, width: usize, s: &Style) -> String {
    let f = row.file;
    let (label, score) = match &row.risk {
        Some(r) => {
            let label = match r.level() {
                RiskLevel::High => s.red(&s.bold("HIGH")),
                RiskLevel::Medium => s.yellow("MED "),
                RiskLevel::Low => s.dim("low "),
            };
            (label, format!("{:.2}", r.score))
        }
        None => ("    ".to_string(), "    ".to_string()),
    };
    let letter = kind_letter(f.change.kind).to_string();
    let letter = match f.change.kind {
        ChangeKind::Added => s.green(&letter),
        ChangeKind::Deleted => s.red(&letter),
        _ => s.yellow(&letter),
    };
    let stats = if f.binary {
        s.dim("binary")
    } else {
        match f.stats {
            Some(st) => format!(
                "{} {}",
                s.green(&format!("+{}", st.added)),
                s.red(&format!("-{}", st.removed))
            ),
            None => String::new(),
        }
    };
    // `fit` counts characters, escape codes would break the padding: pad plain text first.
    let path = fit(&f.display_path, width);
    let reason = row
        .risk
        .as_ref()
        .and_then(|r| r.top_reason())
        .map(|r| truncate(&r.message, 72))
        .unwrap_or_default();
    let mut line = format!("{label} {score} {letter} {path}  {stats}");
    if !reason.is_empty() {
        line.push_str(&format!("  {}", s.dim(&reason)));
    }
    if f.viewed {
        line.push_str(&format!(" {}", s.dim("(viewed)")));
    }
    line.trim_end().to_string()
}

// ------------------------------------------------------------------------- diff

/// A moved block inside one file or between two files, as printed in JSON.
fn block_json(old: &Range<u32>, new: &Range<u32>, reindented: bool) -> Value {
    json!({ "old": [old.start, old.end], "new": [new.start, new.end], "reindented": reindented })
}

/// Moves that touch one file.
#[derive(Default)]
struct FileMoves {
    within: Vec<MovedBlock>,
    /// This file lost the block (it moved to `to`).
    moved_out: Vec<(Range<u32>, String, Range<u32>, bool)>,
    /// This file gained the block (it came from `from`).
    moved_in: Vec<(Range<u32>, String, Range<u32>, bool)>,
}

impl FileMoves {
    fn is_old_moved(&self, line: u32) -> bool {
        self.within.iter().any(|m| m.old.contains(&line))
            || self.moved_out.iter().any(|(r, ..)| r.contains(&line))
    }
    fn is_new_moved(&self, line: u32) -> bool {
        self.within.iter().any(|m| m.new.contains(&line))
            || self.moved_in.iter().any(|(r, ..)| r.contains(&line))
    }
}

fn side_text(side: &review::DiffSide) -> String {
    side.text
        .as_ref()
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// Moved blocks of every file of a changeset (within files and between files).
fn detect_moves(files: &[(String, FileDiff)]) -> (Vec<FileMoves>, Vec<CrossFileMove>) {
    let texts: Vec<Option<(String, String)>> = files
        .iter()
        .map(|(_, d)| (d.kind == DiffKind::Text).then(|| (side_text(&d.old), side_text(&d.new))))
        .collect();
    let mut moves: Vec<FileMoves> = files.iter().map(|_| FileMoves::default()).collect();
    for (i, (_, d)) in files.iter().enumerate() {
        if let Some((old, new)) = &texts[i] {
            moves[i].within = detect_moved(old, new, &d.diff, 3);
        }
    }
    let present: Vec<usize> = (0..files.len()).filter(|i| texts[*i].is_some()).collect();
    let inputs: Vec<(&str, &str, &lector_diff::LineDiff)> = present
        .iter()
        .filter_map(|&i| {
            texts[i]
                .as_ref()
                .map(|(o, n)| (o.as_str(), n.as_str(), &files[i].1.diff))
        })
        .collect();
    let mut cross = detect_moved_across(&inputs);
    // Indices refer to `inputs`; map them back to `files`.
    for m in &mut cross {
        m.old_file = present[m.old_file];
        m.new_file = present[m.new_file];
        let (from, to) = (files[m.old_file].0.clone(), files[m.new_file].0.clone());
        moves[m.old_file]
            .moved_out
            .push((m.old.clone(), to, m.new.clone(), m.reindented));
        moves[m.new_file]
            .moved_in
            .push((m.new.clone(), from, m.old.clone(), m.reindented));
    }
    (moves, cross)
}

/// One printed diff line.
struct DiffLine {
    sign: char,
    text: String,
    moved: bool,
}

/// Hunks whose context would touch or overlap are printed as one (like `git diff`).
fn group_hunks(hunks: &[lector_diff::Hunk], context: u32) -> Vec<&[lector_diff::Hunk]> {
    let mut groups = Vec::new();
    let mut start = 0;
    for i in 1..=hunks.len() {
        let split = i == hunks.len()
            || hunks[i].old.start.saturating_sub(hunks[i - 1].old.end) > context.saturating_mul(2);
        if split {
            groups.push(&hunks[start..i]);
            start = i;
        }
    }
    groups
}

/// Lines of a side without the empty row a trailing newline leaves at the end.
fn real_lines(side: &review::DiffSide) -> u32 {
    let n = side.line_count();
    if n > 0 && side.line(n - 1).is_empty() {
        n - 1
    } else {
        n
    }
}

/// `start,len` of a unified-diff range (`start` 0-based; git's conventions for empty and
/// single-line ranges).
fn range_str(start: u32, len: u32) -> String {
    match len {
        0 => format!("{start},0"),
        1 => format!("{}", start + 1),
        _ => format!("{},{len}", start + 1),
    }
}

/// The header and lines of a group of hunks with `context` lines around them.
fn group_lines(
    diff: &FileDiff,
    group: &[lector_diff::Hunk],
    context: u32,
    mv: &FileMoves,
) -> (String, Vec<DiffLine>) {
    let (Some(first), Some(last)) = (group.first(), group.last()) else {
        return (String::new(), Vec::new());
    };
    let old_n = real_lines(&diff.old);
    let new_n = real_lines(&diff.new);
    let before = context.min(first.old.start);
    let after = context.min(old_n.saturating_sub(last.old.end));
    let ctx = |r: u32, new_side: bool| DiffLine {
        sign: ' ',
        text: if new_side {
            diff.new.line(r)
        } else {
            diff.old.line(r)
        },
        moved: false,
    };
    let mut lines = Vec::new();
    for r in first.old.start - before..first.old.start {
        lines.push(ctx(r, false));
    }
    for (i, h) in group.iter().enumerate() {
        if i > 0 {
            // Unchanged lines between two hunks of the group (equal in both texts).
            for r in group[i - 1].new.end..h.new.start {
                lines.push(ctx(r, true));
            }
        }
        for r in h.old.clone() {
            lines.push(DiffLine {
                sign: '-',
                text: diff.old.line(r),
                moved: mv.is_old_moved(r),
            });
        }
        for r in h.new.clone() {
            lines.push(DiffLine {
                sign: '+',
                text: diff.new.line(r),
                moved: mv.is_new_moved(r),
            });
        }
    }
    for r in last.new.end..(last.new.end + after).min(new_n) {
        lines.push(ctx(r, true));
    }
    let os = first.old.start - before;
    let ol = (last.old.end + after).min(old_n).saturating_sub(os);
    let ns = first.new.start.saturating_sub(before);
    let nl = (last.new.end + after).min(new_n) - ns;
    (
        format!("@@ -{} +{} @@", range_str(os, ol), range_str(ns, nl)),
        lines,
    )
}

fn lines_label(r: &Range<u32>) -> String {
    if r.end - r.start <= 1 {
        format!("{}", r.start + 1)
    } else {
        format!("{}-{}", r.start + 1, r.end)
    }
}

/// Runs `lector diff`.
pub fn run_diff(args: &DiffArgs, io: &mut Io) -> anyhow::Result<()> {
    let dir = resolve_dir(args.directory.as_deref())?;
    let project = open_repo_project(&dir)?;
    let repo = project
        .repo()
        .ok_or_else(|| anyhow!("not inside a git repository"))?;
    let cs = parse_changeset(repo, &args.changeset)?;
    let core = open_core();
    let view = review::load_changeset(&project, &cs, &core.review_store())
        .with_context(|| "cannot load the changes")?;
    let only = args
        .file
        .as_deref()
        .map(|p| p.trim_start_matches("./").to_string());
    if let Some(p) = &only {
        if view.index_of(p).is_none() && view.index_of(&project.repo_path(p)).is_none() {
            bail!("`{p}` is not changed in {}", view.title);
        }
    }
    let selected = |f: &ReviewFile| {
        only.as_deref().is_none_or(|p| {
            f.path() == p
                || f.change.old_path.as_deref() == Some(p)
                || f.path() == project.repo_path(p)
        })
    };
    // Moves need the whole changeset (a block can move to a file that is not shown).
    let opts = review::diff_options(&project.settings().diff);
    let wanted: Vec<&ReviewFile> = view
        .files
        .iter()
        .filter(|f| args.moved || selected(f))
        .collect();
    let mut loaded: Vec<(String, FileDiff)> = Vec::new();
    for f in wanted {
        let diff = review::load_file_diff(&project, &f.change, &opts)
            .with_context(|| format!("cannot diff {}", f.path()))?;
        loaded.push((f.path().to_string(), diff));
    }
    let (moves, cross) = if args.moved {
        detect_moves(&loaded)
    } else {
        (
            loaded.iter().map(|_| FileMoves::default()).collect(),
            Vec::new(),
        )
    };
    let shown: Vec<usize> = (0..loaded.len())
        .filter(|&i| {
            view.files
                .iter()
                .find(|f| f.path() == loaded[i].0)
                .is_some_and(&selected)
        })
        .collect();

    if args.json {
        let files: Vec<Value> = shown
            .iter()
            .map(|&i| {
                let (path, diff) = &loaded[i];
                let mut v = file_diff_json(path, diff, args.context);
                if args.moved {
                    v["moved"] = json!(moves[i]
                        .within
                        .iter()
                        .map(|m| block_json(&m.old, &m.new, m.reindented))
                        .collect::<Vec<_>>());
                }
                v
            })
            .collect();
        let mut data = json!({
            "changeset": view.key,
            "title": view.title,
            "files": files,
        });
        if args.moved {
            data["moves"] = json!(cross
                .iter()
                .map(|m| json!({
                    "from": { "path": loaded[m.old_file].0, "lines": [m.old.start, m.old.end] },
                    "to": { "path": loaded[m.new_file].0, "lines": [m.new.start, m.new.end] },
                    "reindented": m.reindented,
                }))
                .collect::<Vec<_>>());
        }
        return Ok(print_json(io.out, data)?);
    }

    let s = io.style;
    if shown.is_empty() {
        writeln!(
            io.out,
            "{}",
            s.dim(&format!("No changes in {}.", view.title))
        )?;
        return Ok(());
    }
    writeln!(io.out, "{}", s.bold(&view.title))?;
    if args.moved {
        writeln!(
            io.out,
            "{}",
            s.dim("(lines starting with < or > were moved, not rewritten)")
        )?;
    }
    for &i in &shown {
        let (path, diff) = &loaded[i];
        let stats = diff.stats();
        let title = format!(
            "{} ({}{})",
            path,
            kind_name(diff.change.kind),
            if diff.kind == DiffKind::Text {
                format!(", +{} -{}", stats.added, stats.removed)
            } else {
                String::new()
            }
        );
        writeln!(io.out, "\n{}", s.bold(&title))?;
        match diff.kind {
            DiffKind::Text => {}
            DiffKind::Binary => {
                writeln!(io.out, "{}", s.dim("binary file"))?;
                continue;
            }
            DiffKind::Image(_) => {
                writeln!(io.out, "{}", s.dim("image file"))?;
                continue;
            }
            DiffKind::TooLarge => {
                writeln!(io.out, "{}", s.dim("too large to diff"))?;
                continue;
            }
        }
        let mv = &moves[i];
        for m in &mv.within {
            writeln!(
                io.out,
                "{}",
                s.magenta(&format!(
                    "moved within file: lines {} -> {}{}",
                    lines_label(&m.old),
                    lines_label(&m.new),
                    if m.reindented { " (reindented)" } else { "" }
                ))
            )?;
        }
        for (_, to, new, re) in &mv.moved_out {
            writeln!(
                io.out,
                "{}",
                s.magenta(&format!(
                    "moved to {to}:{}{}",
                    lines_label(new),
                    if *re { " (reindented)" } else { "" }
                ))
            )?;
        }
        for (_, from, old, re) in &mv.moved_in {
            writeln!(
                io.out,
                "{}",
                s.magenta(&format!(
                    "moved from {from}:{}{}",
                    lines_label(old),
                    if *re { " (reindented)" } else { "" }
                ))
            )?;
        }
        for group in group_hunks(&diff.diff.hunks, args.context) {
            let (header, lines) = group_lines(diff, group, args.context, mv);
            writeln!(io.out, "{}", s.cyan(&header))?;
            for l in lines {
                let painted = match (l.sign, l.moved) {
                    ('-', true) => s.magenta(&format!("<{}", l.text)),
                    ('+', true) => s.blue(&format!(">{}", l.text)),
                    ('-', false) => s.red(&format!("-{}", l.text)),
                    ('+', false) => s.green(&format!("+{}", l.text)),
                    _ => format!(" {}", l.text),
                };
                writeln!(io.out, "{painted}")?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snake_names() {
        assert_eq!(snake_case("PublicApiChanged"), "public_api_changed");
        assert_eq!(snake_case("Config"), "config");
        assert_eq!(snake_case("SourceWithoutTests"), "source_without_tests");
    }

    #[test]
    fn line_labels() {
        assert_eq!(lines_label(&(0..1)), "1");
        assert_eq!(lines_label(&(4..9)), "5-9");
    }
}
