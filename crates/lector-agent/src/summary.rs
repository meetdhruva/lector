//! Turn summaries (v0.7): a compact, deterministic description of what an agent turn changed,
//! for the Agent panel and the header of the turn's review tab.
//!
//! **No language model is involved.** Everything here is derived from the turn's diff and the
//! review queue's risk analysis ([`lector_app::review_queue::analyze`]): the counts come from the
//! line diff, the order of the files is the review queue's order (risk first, generated and lock
//! files grouped last as low value), the "riskiest" sentence is the top reason of the first file
//! with its evidence lines, and "tests touched" is a path heuristic over the changed files.
//!
//! [`TurnSummary::compute`] reads the turn's checkpoints (blocking; run it off the UI thread and
//! cache it per turn). [`TurnSummary::from_diffs`] is the pure part.

use std::sync::Arc;

use lector_app::review_queue::{analyze, AnalysisOptions, QueueGroup};
use lector_diff::DiffOptions;
use lector_git::{ChangeKind, Changeset, Repo};
use lector_syntax::LanguageRegistry;
use lector_workspace::review::{self, DiffKind, FileDiff};
use lector_workspace::risk::{self, EvidenceSide, RiskKind, RiskLevel};

/// Most files analyzed for one summary; the rest are only counted.
pub const MAX_SUMMARY_FILES: usize = 300;

/// Where clicking a file of the summary goes: the file's diff at a line of one side.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NavTarget {
    /// Path of the file.
    pub path: String,
    /// `true`: `line` indexes the old text (removed lines).
    pub old_side: bool,
    /// 0-based line.
    pub line: u32,
}

/// The lines a reason is based on, for display ("lines 40–52").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvidenceLines {
    /// Old (removed) or new text.
    pub old_side: bool,
    /// First line (0-based).
    pub start: u32,
    /// Line after the last (0-based, exclusive).
    pub end: u32,
}

impl EvidenceLines {
    /// "lines 40–52", "line 40", or "old lines 40–52" for removed code (1-based for people).
    pub fn label(&self) -> String {
        let (a, b) = (self.start + 1, self.end.max(self.start + 1));
        let what = if b == a { "line" } else { "lines" };
        let span = if b == a {
            a.to_string()
        } else {
            format!("{a}\u{2013}{b}")
        };
        if self.old_side {
            format!("old {what} {span}")
        } else {
            format!("{what} {span}")
        }
    }
}

/// One file of a summary.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryFile {
    /// Path to show.
    pub path: String,
    /// Added, modified, deleted...
    pub kind: ChangeKind,
    /// Lines added.
    pub added: u32,
    /// Lines removed.
    pub removed: u32,
    /// Whether it is in the attention group or the low-value group (generated, lock, negligible).
    pub low_value: bool,
    /// Risk bucket.
    pub level: RiskLevel,
    /// The strongest reason ("Removes error handling"), if any.
    pub reason: Option<String>,
    /// The lines of that reason, if it has some.
    pub evidence: Option<EvidenceLines>,
    /// A changed test file.
    pub is_test: bool,
    /// Binary (no line counts).
    pub binary: bool,
    /// What clicking the file opens.
    pub target: NavTarget,
}

/// The summary of one turn.
#[derive(Debug, Clone, PartialEq)]
pub struct TurnSummary {
    /// Files in review-queue order (attention group first, then low value).
    pub files: Vec<SummaryFile>,
    /// Lines added overall.
    pub added: u32,
    /// Lines removed overall.
    pub removed: u32,
    /// Changed files that are tests.
    pub tests_touched: usize,
    /// Changed files that were not analyzed (more than [`MAX_SUMMARY_FILES`]).
    pub unanalyzed: usize,
}

/// Why a summary could not be computed.
#[derive(Debug, thiserror::Error)]
pub enum SummaryError {
    /// Reading the checkpoints failed.
    #[error("cannot read the turn's changes: {0}")]
    Git(String),
}

impl TurnSummary {
    /// Reads and analyzes the changes of `cs` (blocking).
    pub fn compute(
        repo: &Repo,
        languages: &LanguageRegistry,
        cs: &Changeset,
        opts: &DiffOptions,
    ) -> Result<TurnSummary, SummaryError> {
        let changes = repo
            .changes(cs)
            .map_err(|e| SummaryError::Git(e.to_string()))?;
        let unanalyzed = changes.len().saturating_sub(MAX_SUMMARY_FILES);
        let diffs: Vec<Arc<FileDiff>> = changes
            .iter()
            .take(MAX_SUMMARY_FILES)
            .filter_map(|c| review::load_file_diff_with(repo, languages, c, opts).ok())
            .map(Arc::new)
            .collect();
        let mut summary = TurnSummary::from_diffs(&diffs);
        summary.unanalyzed = unanalyzed;
        Ok(summary)
    }

    /// The summary of already loaded diffs (pure).
    pub fn from_diffs(diffs: &[Arc<FileDiff>]) -> TurnSummary {
        let analysis = analyze(
            diffs,
            &AnalysisOptions {
                detect_moved: false,
            },
        );
        let mut files = Vec::with_capacity(diffs.len());
        for entry in &analysis.order {
            let Some(diff) = diffs.get(entry.index) else {
                continue;
            };
            files.push(summary_file(diff, entry));
        }
        let (mut added, mut removed) = (0, 0);
        for f in &files {
            added += f.added;
            removed += f.removed;
        }
        let tests_touched = files.iter().filter(|f| f.is_test).count();
        TurnSummary {
            files,
            added,
            removed,
            tests_touched,
            unanalyzed: 0,
        }
    }

    /// Number of changed files, including those not analyzed.
    pub fn file_count(&self) -> usize {
        self.files.len() + self.unanalyzed
    }

    /// Files worth a careful look, riskiest first.
    pub fn attention(&self) -> impl Iterator<Item = &SummaryFile> {
        self.files.iter().filter(|f| !f.low_value)
    }

    /// Generated, lock and negligible files.
    pub fn low_value(&self) -> impl Iterator<Item = &SummaryFile> {
        self.files.iter().filter(|f| f.low_value)
    }

    /// The first attention file that has a reason to be looked at.
    pub fn riskiest(&self) -> Option<&SummaryFile> {
        self.attention().find(|f| f.reason.is_some())
    }

    /// Whether every changed file is generated, a lock file or negligible.
    pub fn only_low_value(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(|f| f.low_value)
    }

    /// Whether every changed file is a test.
    pub fn only_tests(&self) -> bool {
        !self.files.is_empty() && self.files.iter().all(|f| f.is_test)
    }

    /// "+62 −17".
    pub fn stats_label(&self) -> String {
        format!("+{} \u{2212}{}", self.added, self.removed)
    }

    /// "Tests touched: 2 files." / "Tests touched: none."
    pub fn tests_label(&self) -> String {
        match self.tests_touched {
            0 => "Tests touched: none.".into(),
            1 => "Tests touched: 1 file.".into(),
            n => format!("Tests touched: {n} files."),
        }
    }

    /// "Changed 4 files (+62 −17)." — or the single file's name.
    pub fn changed_label(&self) -> String {
        match self.files.as_slice() {
            [one] if self.unanalyzed == 0 => {
                format!("Changed {} ({}).", one.path, self.stats_label())
            }
            _ => format!(
                "Changed {} file{} ({}).",
                self.file_count(),
                if self.file_count() == 1 { "" } else { "s" },
                self.stats_label()
            ),
        }
    }

    /// "Riskiest: src/auth/session.rs — removes error handling (lines 40–52)." (empty when no
    /// file stands out).
    pub fn riskiest_sentence(&self) -> Option<String> {
        let f = self.riskiest()?;
        let reason = f.reason.as_deref()?;
        let mut s = format!("{} \u{2014} {}", f.path, lowercase_first(reason));
        if let Some(ev) = &f.evidence {
            s.push_str(&format!(" ({})", ev.label()));
        }
        Some(s)
    }

    /// The whole sentence block shown in the card: counts, the riskiest file and the tests.
    ///
    /// Representative outputs:
    /// * `Changed 4 files (+62 −17). Riskiest: src/auth/session.rs — removes error handling
    ///   (lines 40–52). Tests touched: 2 files.`
    /// * `Changed src/a.rs (+3 −1) — changes a public function signature (lines 4–5). Tests
    ///   touched: none.` (one file)
    /// * `Changed 2 files (+30 −2): only test files.`
    /// * `Changed 2 files (+1204 −3): only generated, lock or low-value files.`
    pub fn headline(&self) -> String {
        if self.files.is_empty() {
            return String::new();
        }
        if self.only_tests() {
            return format!(
                "{}: only test files.",
                self.changed_label().trim_end_matches('.')
            );
        }
        if self.only_low_value() {
            return format!(
                "{}: only generated, lock or low-value files.",
                self.changed_label().trim_end_matches('.')
            );
        }
        let mut out = String::new();
        match (self.files.as_slice(), self.riskiest_sentence()) {
            ([one], Some(_)) if self.unanalyzed == 0 => {
                let reason = one.reason.as_deref().unwrap_or_default();
                out.push_str(&format!(
                    "Changed {} ({}) \u{2014} {}",
                    one.path,
                    self.stats_label(),
                    lowercase_first(reason)
                ));
                if let Some(ev) = &one.evidence {
                    out.push_str(&format!(" ({})", ev.label()));
                }
                out.push('.');
            }
            (_, Some(riskiest)) => {
                out.push_str(&self.changed_label());
                out.push_str(&format!(" Riskiest: {riskiest}."));
            }
            (_, None) => out.push_str(&self.changed_label()),
        }
        out.push(' ');
        out.push_str(&self.tests_label());
        out
    }
}

fn summary_file(diff: &FileDiff, entry: &lector_app::review_queue::QueueEntry) -> SummaryFile {
    let path = diff.change.path().to_string();
    let stats = diff.stats();
    let reason = entry.report.top_reason();
    let evidence = reason
        .and_then(|r| r.evidence.as_ref())
        .map(|e| EvidenceLines {
            old_side: e.side == EvidenceSide::Old,
            start: e.lines.start,
            end: e.lines.end,
        });
    let target = match (&evidence, reason.and_then(|r| r.line)) {
        (Some(ev), _) => NavTarget {
            path: path.clone(),
            old_side: ev.old_side,
            line: ev.start,
        },
        (None, Some(line)) => NavTarget {
            path: path.clone(),
            old_side: false,
            line,
        },
        (None, None) => first_change_target(diff, &path),
    };
    SummaryFile {
        low_value: entry.group == QueueGroup::LowRisk,
        level: entry.report.level(),
        reason: reason.map(|r| r.message.clone()),
        evidence,
        is_test: risk::tests_changed_in(&[diff]),
        binary: diff.kind != DiffKind::Text,
        kind: diff.change.kind,
        added: stats.added,
        removed: stats.removed,
        path,
        target,
    }
}

/// The first changed line of the file: on the new side, or the old side for a deleted file.
fn first_change_target(diff: &FileDiff, path: &str) -> NavTarget {
    match diff.diff.hunks.first() {
        Some(h) if diff.new.exists => NavTarget {
            path: path.to_string(),
            old_side: false,
            line: h.new.start,
        },
        Some(h) => NavTarget {
            path: path.to_string(),
            old_side: true,
            line: h.old.start,
        },
        None => NavTarget {
            path: path.to_string(),
            old_side: false,
            line: 0,
        },
    }
}

/// "Removes error handling" -> "removes error handling"; acronyms ("SQL ...") stay as they are.
fn lowercase_first(s: &str) -> String {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(a), Some(b)) if a.is_uppercase() && !b.is_uppercase() => {
            a.to_lowercase().collect::<String>() + &s[a.len_utf8()..]
        }
        _ => s.to_string(),
    }
}

/// Whether a reason of this kind makes a file "risky" for the headline (kept for callers that
/// want to filter).
pub fn is_raising(kind: RiskKind) -> bool {
    !kind.is_dampening()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_git::{FileChange, Side};
    use lector_workspace::review::diff_bytes;

    fn diff_for(path: &str, old: Option<&str>, new: Option<&str>) -> Arc<FileDiff> {
        let change = FileChange {
            old_path: old.map(|_| path.to_string()),
            new_path: new.map(|_| path.to_string()),
            kind: match (old, new) {
                (None, _) => ChangeKind::Added,
                (_, None) => ChangeKind::Deleted,
                _ => ChangeKind::Modified,
            },
            old: Side::None,
            new: Side::None,
            binary: None,
        };
        Arc::new(diff_bytes(
            change,
            old.map(|s| s.as_bytes().to_vec()),
            new.map(|s| s.as_bytes().to_vec()),
            &LanguageRegistry::new(),
            &DiffOptions::default(),
        ))
    }

    const SESSION_OLD: &str = "pub fn load(path: &str) -> Result<String, Error> {\n    let text = read(path)?;\n    let v = parse(&text)?;\n    Ok(v)\n}\n";
    const SESSION_NEW: &str = "pub fn load(path: &str) -> String {\n    let text = read(path).unwrap();\n    parse(&text).unwrap()\n}\n";

    #[test]
    fn no_tests_touched() {
        let files = vec![
            diff_for("src/auth/session.rs", Some(SESSION_OLD), Some(SESSION_NEW)),
            diff_for("src/util.rs", Some("fn a() {}\n"), Some("fn a() { 1 }\n")),
        ];
        let s = TurnSummary::from_diffs(&files);
        assert_eq!(s.files.len(), 2);
        assert_eq!(s.tests_touched, 0);
        let text = s.headline();
        assert!(text.starts_with("Changed 2 files (+"), "{text}");
        assert!(
            text.contains("Riskiest: src/auth/session.rs \u{2014} "),
            "{text}"
        );
        assert!(text.ends_with("Tests touched: none."), "{text}");
        assert_eq!(s.files[0].path, "src/auth/session.rs");
        assert!(s.stats_label().contains('\u{2212}'));
    }

    #[test]
    fn tests_touched_counts_files() {
        let files = vec![
            diff_for(
                "src/api.rs",
                Some("pub fn f() {}\n"),
                Some("pub fn f(x: u8) {}\n"),
            ),
            diff_for(
                "tests/api_test.rs",
                Some("fn t() {}\n"),
                Some("fn t() { f(1) }\n"),
            ),
            diff_for("tests/other.rs", None, Some("fn u() {}\n")),
        ];
        let s = TurnSummary::from_diffs(&files);
        assert_eq!(s.tests_touched, 2);
        assert!(
            s.headline().ends_with("Tests touched: 2 files."),
            "{}",
            s.headline()
        );
        assert!(s.files.iter().filter(|f| f.is_test).count() == 2);
    }

    #[test]
    fn only_tests_changed() {
        let files = vec![
            diff_for("tests/a.rs", Some("fn a() {}\n"), Some("fn a() { 1 }\n")),
            diff_for("tests/b.rs", None, Some("fn b() {}\n")),
        ];
        let s = TurnSummary::from_diffs(&files);
        assert!(s.only_tests());
        let h = s.headline();
        assert!(h.starts_with("Changed 2 files (+"), "{h}");
        assert!(h.ends_with("only test files."), "{h}");
        assert!(!h.contains("Riskiest"), "{h}");
    }

    #[test]
    fn generated_and_lock_files_only() {
        let lock = (0..40).map(|i| format!("dep{i} = 1\n")).collect::<String>();
        let lock2 = lock.replace("dep3 = 1", "dep3 = 2");
        let files = vec![
            diff_for("Cargo.lock", Some(&lock), Some(&lock2)),
            diff_for("web/app.min.js", Some("var a=1;\n"), Some("var a=2;\n")),
        ];
        let s = TurnSummary::from_diffs(&files);
        assert!(s.only_low_value(), "{:?}", s.files);
        assert!(s.riskiest().is_none());
        let h = s.headline();
        assert!(h.starts_with("Changed 2 files (+"), "{h}");
        assert!(
            h.ends_with(": only generated, lock or low-value files."),
            "{h}"
        );
        assert!(!h.contains("Riskiest"), "{h}");
    }

    #[test]
    fn a_single_file_names_it() {
        let files = vec![diff_for(
            "src/auth/session.rs",
            Some(SESSION_OLD),
            Some(SESSION_NEW),
        )];
        let s = TurnSummary::from_diffs(&files);
        let h = s.headline();
        assert!(h.starts_with("Changed src/auth/session.rs (+"), "{h}");
        assert!(h.contains(" \u{2014} "), "{h}");
        assert!(h.ends_with("Tests touched: none."), "{h}");
        assert!(!h.contains("Riskiest"), "{h}");
    }

    #[test]
    fn order_is_the_review_queues_order() {
        let files: Vec<Arc<FileDiff>> = vec![
            diff_for("src/util.rs", Some("fn a() {}\n"), Some("fn a() { 1 }\n")),
            diff_for(
                "Cargo.lock",
                Some("# lock\nversion = 1\n"),
                Some("# lock\nversion = 2\n"),
            ),
            diff_for("src/auth/session.rs", Some(SESSION_OLD), Some(SESSION_NEW)),
            diff_for(
                "src/api.rs",
                Some("pub fn load(path: &str) -> String {\n    String::new()\n}\n"),
                Some("pub fn load(path: &str, strict: bool) -> String {\n    String::new()\n}\n"),
            ),
        ];
        let queue = analyze(
            &files,
            &AnalysisOptions {
                detect_moved: false,
            },
        );
        let expected: Vec<String> = queue
            .order
            .iter()
            .map(|e| files[e.index].change.path().to_string())
            .collect();
        let s = TurnSummary::from_diffs(&files);
        let got: Vec<String> = s.files.iter().map(|f| f.path.clone()).collect();
        assert_eq!(got, expected);
        // Low-value files trail.
        let first_low = s.files.iter().position(|f| f.low_value).unwrap();
        assert!(s.files[first_low..].iter().all(|f| f.low_value));
        assert!(s
            .files
            .iter()
            .any(|f| f.path == "Cargo.lock" && f.low_value));
    }

    #[test]
    fn evidence_gives_the_navigation_target() {
        let files = vec![diff_for(
            "src/auth/session.rs",
            Some(SESSION_OLD),
            Some(SESSION_NEW),
        )];
        let s = TurnSummary::from_diffs(&files);
        let f = &s.files[0];
        let reason = lector_app::review_queue::analyze(
            &files,
            &AnalysisOptions {
                detect_moved: false,
            },
        )
        .order[0]
            .report
            .top_reason()
            .cloned()
            .expect("a reason");
        let ev = reason.evidence.expect("evidence");
        assert_eq!(
            f.target,
            NavTarget {
                path: "src/auth/session.rs".into(),
                old_side: ev.side == EvidenceSide::Old,
                line: ev.lines.start
            }
        );
        assert_eq!(f.evidence.as_ref().unwrap().start, ev.lines.start);
        let label = f.evidence.as_ref().unwrap().label();
        assert!(label.contains("line"), "{label}");
    }

    #[test]
    fn a_file_without_evidence_opens_at_its_first_change() {
        let old = (0..30).map(|i| format!("// c{i}\n")).collect::<String>();
        let new = old.replace("// c20\n", "// c20 changed\n");
        let files = vec![diff_for("notes/readme.txt", Some(&old), Some(&new))];
        let s = TurnSummary::from_diffs(&files);
        assert_eq!(s.files[0].target.line, 20);
        assert!(!s.files[0].target.old_side);
        // A deleted file opens on the old side.
        let files = vec![diff_for("notes/gone.txt", Some("a\nb\n"), None)];
        let s = TurnSummary::from_diffs(&files);
        assert!(s.files[0].target.old_side);
    }

    #[test]
    fn evidence_labels_and_wording() {
        let ev = |old_side, start, end| EvidenceLines {
            old_side,
            start,
            end,
        };
        assert_eq!(ev(false, 39, 52).label(), "lines 40\u{2013}52");
        assert_eq!(ev(false, 39, 40).label(), "line 40");
        assert_eq!(ev(true, 3, 6).label(), "old lines 4\u{2013}6");
        assert_eq!(
            lowercase_first("Removes error handling"),
            "removes error handling"
        );
        assert_eq!(
            lowercase_first("SQL built with format!"),
            "SQL built with format!"
        );
        assert!(TurnSummary::from_diffs(&[]).headline().is_empty());
    }
}
