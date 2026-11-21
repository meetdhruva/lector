//! Explainable risk assessment of a file diff, to review the risky changes of
//! an agent's work first.
//!
//! [`assess`] looks at one [`FileDiff`] and returns a [`RiskReport`]: a score
//! in `0..=1` and the typed [`RiskReason`]s behind it, each with a weight and a
//! human sentence. [`rank_files`] orders a whole changeset by risk (and adds
//! the changeset-level "source changed without any test" signal).
//!
//! Signals (all deterministic, no I/O):
//!
//! * **Public API** (tree-sitter outline): removed, changed (signature) or
//!   added exported items. Exported means Rust `pub`, TS/JS `export`, Go
//!   capitalized, Python non-underscore top-level, Java/C#/Swift `public`,
//!   Kotlin not private/internal/protected. Data types (structs, enums,
//!   interfaces, aliases, constants) compare their whole definition, others
//!   their header.
//! * **Deleted logic**: error handling (`?`, `catch`, `except`,
//!   `if err != nil`, ...), conditionals, assertions/tests removed, and
//!   `unwrap`/`expect`/`panic!` added.
//! * **Dangerous constructs added**: `unsafe`, `eval`, process execution,
//!   shell tricks, unsafe deserialization, SQL built by string formatting.
//! * **Security-sensitive paths**: auth, crypto, permissions, CI workflows,
//!   Dockerfiles, migrations, secrets, infrastructure.
//! * **Changeset context**: source changed without any test changed.
//! * **Churn**, deleted files, dependency manifests, config, binary/too-large.
//! * **Dampening** (lowers the score): generated files, lockfiles, minified
//!   code, test files, docs.
//!
//! `score = (1 - Π(1 - w_raise)) · Π(1 - w_damp)`, rounded to 3 decimals.

use std::borrow::Borrow;
use std::ops::Range;

use lector_diff::LineDiff;
use lector_git::ChangeKind;
use lector_syntax::{Language, LanguageRegistry, SymbolKind};
use regex::Regex;

use crate::review::{DiffKind, DiffSide, FileDiff};

mod signals;
mod weights;

pub use weights::RiskWeights;

use signals::{At, Row};

/// A weight of [`RiskWeights`] (picked by the signal, scaled by its magnitude).
type Param = fn(&RiskWeights) -> f32;

macro_rules! param {
    ($f:ident) => {
        (|w: &RiskWeights| w.$f) as Param
    };
}

// Declared after `param!` so that the rule-family modules can use it.
mod api;
mod classify;
mod patterns;
#[cfg(test)]
mod tests;

use api::api_reasons;
use classify::{
    extension, family_of, file_name, generated_reason, is_config_ext, is_docs_path, is_lockfile,
    is_manifest, is_test_path, path_of, security_path_reason, Family,
};
use patterns::{is_comment, net_added, patterns};

/// What a [`RiskReason`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum RiskKind {
    /// An exported item was removed.
    PublicApiRemoved,
    /// The signature/definition of an exported item changed.
    PublicApiChanged,
    /// A new exported item.
    PublicApiAdded,
    /// Error handling (`?`, `catch`, `except`, `if err != nil`...) removed.
    ErrorHandlingRemoved,
    /// Conditional branches removed.
    ConditionalsRemoved,
    /// `unwrap`/`expect`/`panic!` added.
    PanicAdded,
    /// `unsafe` added.
    UnsafeCode,
    /// `eval`, process execution, shell, unsafe deserialization added.
    DynamicExecution,
    /// SQL assembled with string formatting.
    SqlStringBuilding,
    /// The path belongs to a security-sensitive area.
    SecuritySensitivePath,
    /// Tests or assertions removed.
    TestsRemoved,
    /// Source changed but no test file of the changeset did.
    SourceWithoutTests,
    /// Many lines changed.
    LargeChurn,
    /// A file was deleted.
    DeletedFile,
    /// A dependency or build manifest changed.
    DependencyManifest,
    /// A configuration file changed.
    Config,
    /// A lockfile changed.
    Lockfile,
    /// A binary file changed.
    BinaryChange,
    /// Too large to analyze.
    TooLarge,
    /// Dampening: generated or minified file.
    Generated,
    /// Dampening: a test file.
    TestFile,
    /// Dampening: documentation.
    Docs,
    /// An authentication/authorization check was removed.
    AuthCheckRemoved,
    /// Certificate or signature verification was disabled.
    VerificationDisabled,
    /// A weak hash/cipher was added or a hardening primitive removed.
    WeakCrypto,
    /// A credential literal was added.
    HardcodedSecret,
    /// A secret is compared with `==`.
    SecretCompare,
    /// A destructive or risky schema/data migration.
    DestructiveMigration,
    /// Locking, awaiting or memory ordering was removed.
    ConcurrencyRemoved,
    /// Thread-safety guarantees were weakened.
    ConcurrencyWeakened,
    /// Threads, tasks or goroutines were added.
    ConcurrencyAdded,
    /// A new, unpinned or non-registry dependency, or an install-time script.
    DependencyRisk,
    /// A CI change that weakens its safety.
    CiWeakened,
    /// A container change that weakens isolation.
    ContainerWeakened,
    /// Tests were skipped or ignored.
    TestsDisabled,
    /// A boolean default was flipped.
    ConfigDefaultFlipped,
}

impl RiskKind {
    /// True for kinds that lower the score instead of raising it.
    pub fn is_dampening(self) -> bool {
        matches!(
            self,
            RiskKind::Generated | RiskKind::TestFile | RiskKind::Docs
        )
    }
}

/// Which text an [`Evidence`] range refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSide {
    /// The old (base) text: the lines that were removed or replaced.
    Old,
    /// The new (head) text.
    New,
}

/// The lines a reason is based on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evidence {
    /// Path of the file (new path, or old path of a deleted file).
    pub path: String,
    /// Which side of the diff `lines` index into.
    pub side: EvidenceSide,
    /// 0-based, end-exclusive line range.
    pub lines: Range<u32>,
}

/// One explainable signal.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskReason {
    /// Signal type.
    pub kind: RiskKind,
    /// For raising kinds: contribution in `0..=1`. For dampening kinds: the
    /// fraction of the score that is removed.
    pub weight: f32,
    /// Human-readable sentence.
    pub message: String,
    /// A (0-based) line of the new text the reason points at, when it has one.
    pub line: Option<u32>,
    /// The file and lines (old or new side) the reason is based on, when it has them.
    pub evidence: Option<Evidence>,
}

/// Coarse risk bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum RiskLevel {
    /// Score below 0.25.
    Low,
    /// Score below 0.55.
    Medium,
    /// Score of 0.55 or more.
    High,
}

/// The assessment of one file diff.
#[derive(Debug, Clone, PartialEq)]
pub struct RiskReport {
    /// Risk in `0..=1` (0 = nothing to worry about).
    pub score: f32,
    /// Reasons, strongest first (dampening reasons last).
    pub reasons: Vec<RiskReason>,
}

impl RiskReport {
    /// Bucket of [`score`](Self::score).
    pub fn level(&self) -> RiskLevel {
        if self.score >= 0.55 {
            RiskLevel::High
        } else if self.score >= 0.25 {
            RiskLevel::Medium
        } else {
            RiskLevel::Low
        }
    }

    /// The strongest raising reason.
    pub fn top_reason(&self) -> Option<&RiskReason> {
        self.reasons.iter().find(|r| !r.kind.is_dampening())
    }

    /// True for generated and lock files and for scores of 0.05 or less: the files
    /// the review queue collapses into its trailing low-risk group.
    pub fn is_low_value(&self) -> bool {
        self.reasons
            .iter()
            .any(|r| matches!(r.kind, RiskKind::Generated | RiskKind::Lockfile))
            || self.score <= 0.05
    }

    /// One line: the top reasons, `"score 0.62: ...; ..."`.
    pub fn summary(&self) -> String {
        let top: Vec<&str> = self
            .reasons
            .iter()
            .filter(|r| !r.kind.is_dampening())
            .take(3)
            .map(|r| r.message.as_str())
            .collect();
        if top.is_empty() {
            format!("score {:.2}: no risk signals", self.score)
        } else {
            format!("score {:.2}: {}", self.score, top.join("; "))
        }
    }
}

/// Changeset-level facts that influence a single file's assessment.
#[derive(Debug, Clone, Copy, Default)]
struct Context {
    /// `Some(false)`: the changeset touches no test file.
    tests_changed: Option<bool>,
}

/// The weight-independent result of analyzing one file: every signal with its
/// magnitude and evidence. [`score`](Self::score) turns it into a [`RiskReport`]
/// for a set of [`RiskWeights`] without looking at the file again, which is what
/// makes fitting the weights cheap.
#[derive(Clone)]
pub struct RiskFacts {
    facts: Vec<Fact>,
    churn: u32,
}

#[derive(Clone)]
struct Fact {
    kind: RiskKind,
    param: Param,
    magnitude: f32,
    message: String,
    line: Option<u32>,
    evidence: Option<Evidence>,
}

impl std::fmt::Debug for RiskFacts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RiskFacts")
            .field("signals", &self.facts.len())
            .field("churn", &self.churn)
            .finish()
    }
}

impl RiskFacts {
    /// Added plus removed lines of the file (the tie-breaker of the ordering).
    pub fn churn(&self) -> u32 {
        self.churn
    }

    /// Scores the signals: `score = (1 - Π(1 - w_raise)) · Π(1 - w_damp)` with
    /// `w = base weight · magnitude` (clamped to 0.95), rounded to 3 decimals.
    pub fn score(&self, weights: &RiskWeights) -> RiskReport {
        let mut raise = Vec::new();
        let mut damp = Vec::new();
        for f in &self.facts {
            let r = RiskReason {
                kind: f.kind,
                weight: ((f.param)(weights) * f.magnitude).clamp(0.0, 0.95),
                message: f.message.clone(),
                line: f.line,
                evidence: f.evidence.clone(),
            };
            if f.kind.is_dampening() {
                damp.push(r);
            } else {
                raise.push(r);
            }
        }
        let cmp = |a: &RiskReason, b: &RiskReason| {
            b.weight
                .total_cmp(&a.weight)
                .then(a.kind.cmp(&b.kind))
                .then(a.message.cmp(&b.message))
        };
        raise.sort_by(cmp);
        damp.sort_by(cmp);
        let raised = 1.0 - raise.iter().fold(1.0f32, |p, r| p * (1.0 - r.weight));
        let damped = damp.iter().fold(1.0f32, |p, r| p * (1.0 - r.weight));
        let score = ((raised * damped).clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
        let mut reasons = raise;
        reasons.extend(damp);
        RiskReport { score, reasons }
    }
}

/// Analyzes one file into weight-independent [`RiskFacts`]. `tests_changed` is the
/// changeset fact from [`tests_changed_in`] (`None` when unknown).
pub fn collect_facts(
    diff: &FileDiff,
    langs: &LanguageRegistry,
    tests_changed: Option<bool>,
) -> RiskFacts {
    collect(diff, langs, Context { tests_changed })
}

/// True when some file of the changeset is a changed test file.
pub fn tests_changed_in<D: Borrow<FileDiff>>(files: &[D]) -> bool {
    files.iter().any(|f| {
        let f: &FileDiff = f.borrow();
        is_test_path(&path_of(f)) && !f.is_identical()
    })
}

/// Orders already analyzed files: `(index, report)` by descending score, ties by
/// churn, then input order.
pub fn rank_facts(facts: &[RiskFacts], weights: &RiskWeights) -> Vec<(usize, RiskReport)> {
    let mut out: Vec<(usize, RiskReport, u32)> = facts
        .iter()
        .enumerate()
        .map(|(i, f)| (i, f.score(weights), f.churn))
        .collect();
    out.sort_by(|a, b| {
        b.1.score
            .total_cmp(&a.1.score)
            .then(b.2.cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });
    out.into_iter().map(|(i, r, _)| (i, r)).collect()
}

/// Assesses one file diff.
pub fn assess(diff: &FileDiff, langs: &LanguageRegistry) -> RiskReport {
    assess_with(diff, langs, &RiskWeights::default())
}

/// [`assess`] with explicit weights.
pub fn assess_with(diff: &FileDiff, langs: &LanguageRegistry, weights: &RiskWeights) -> RiskReport {
    collect(diff, langs, Context::default()).score(weights)
}

/// Assesses and orders `files` by risk (highest first; ties by churn, then
/// input order). Returns `(index into files, report)` for every file.
pub fn rank_files<D: Borrow<FileDiff>>(files: &[D]) -> Vec<(usize, RiskReport)> {
    rank_files_with(files, &LanguageRegistry::new())
}

/// [`rank_files`] with an explicit language registry (`D` may be `FileDiff`, `Arc<FileDiff>`, ...).
pub fn rank_files_with<D: Borrow<FileDiff>>(
    files: &[D],
    langs: &LanguageRegistry,
) -> Vec<(usize, RiskReport)> {
    rank_files_weighted(files, langs, &RiskWeights::default())
}

/// [`rank_files_with`] with explicit weights.
pub fn rank_files_weighted<D: Borrow<FileDiff>>(
    files: &[D],
    langs: &LanguageRegistry,
    weights: &RiskWeights,
) -> Vec<(usize, RiskReport)> {
    let tests_changed = Some(tests_changed_in(files));
    let facts: Vec<RiskFacts> = files
        .iter()
        .map(|f| collect_facts(f.borrow(), langs, tests_changed))
        .collect();
    rank_facts(&facts, weights)
}

// ---------------------------------------------------------------------------
// Assessment

struct Builder {
    path: String,
    facts: Vec<Fact>,
}

impl Builder {
    fn add(&mut self, kind: RiskKind, param: Param, magnitude: f32, message: String) {
        self.facts.push(Fact {
            kind,
            param,
            magnitude,
            message,
            line: None,
            evidence: None,
        });
    }

    fn add_at(
        &mut self,
        kind: RiskKind,
        param: Param,
        magnitude: f32,
        message: String,
        at: At,
        rows: Range<u32>,
    ) {
        let side = match at {
            At::Old => EvidenceSide::Old,
            At::New => EvidenceSide::New,
        };
        self.facts.push(Fact {
            kind,
            param,
            magnitude,
            message,
            line: (at == At::New).then_some(rows.start),
            evidence: Some(Evidence {
                path: self.path.clone(),
                side,
                lines: rows,
            }),
        });
    }
}

fn side_text(side: &DiffSide) -> String {
    match (&side.text, side.exists) {
        (Some(t), true) => t.to_string(),
        _ => String::new(),
    }
}

/// The lines of `text` in `ranges` (0-based rows), without a trailing `\r`.
fn rows_in<'a>(text: &'a str, ranges: impl Iterator<Item = Range<u32>>) -> Vec<Row<'a>> {
    let all: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for r in ranges {
        for n in r {
            if let Some(l) = all.get(n as usize) {
                out.push(Row {
                    n,
                    text: l.strip_suffix('\r').unwrap_or(l),
                });
            }
        }
    }
    out
}

fn texts<'a>(rows: &[Row<'a>]) -> Vec<&'a str> {
    rows.iter().map(|r| r.text).collect()
}

/// Rows of the first run of consecutive rows that match `re`.
fn first_run(rows: &[Row], re: &Regex) -> Option<Range<u32>> {
    let first = rows.iter().position(|r| re.is_match(r.text))?;
    let start = rows[first].n;
    let mut end = start + 1;
    for r in &rows[first + 1..] {
        if r.n == end && re.is_match(r.text) {
            end += 1;
        } else if r.n >= end {
            break;
        }
    }
    Some(start..end)
}

/// Comment prefixes by file type (for files that have no code family).
fn comment_prefixes_for(family: Family, path: &str) -> &'static [&'static str] {
    if family != Family::Other {
        return family.comment_prefixes();
    }
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    match extension(&lower) {
        "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "properties" | "env" | "dockerfile"
        | "tf" => &["#"],
        "json" => &[],
        "sql" => &["--"],
        _ if name.starts_with("dockerfile")
            || name.starts_with(".env")
            || name == "requirements.txt"
            || name == "pipfile"
            || name == "gemfile"
            || name == "makefile" =>
        {
            &["#"]
        }
        _ if name == "go.mod" => &["//"],
        _ => &[],
    }
}

fn collect(diff: &FileDiff, langs: &LanguageRegistry, ctx: Context) -> RiskFacts {
    let path = path_of(diff);
    let mut b = Builder {
        path: path.clone(),
        facts: Vec::new(),
    };
    let is_test = is_test_path(&path);
    let stats = diff.diff.stats();
    let churn = stats.added + stats.removed;

    match diff.kind {
        DiffKind::Binary => b.add(
            RiskKind::BinaryChange,
            param!(binary),
            1.0,
            "Binary file changed (cannot be reviewed as text)".into(),
        ),
        DiffKind::TooLarge => b.add(
            RiskKind::TooLarge,
            param!(too_large),
            1.0,
            "Too large to analyze; review manually".into(),
        ),
        DiffKind::Image(_) => {}
        DiffKind::Text => {}
    }
    if let Some((param, why)) = security_path_reason(&path) {
        if !is_docs_path(&path) {
            let magnitude = if is_test { 0.5 } else { 1.0 };
            b.add(
                RiskKind::SecuritySensitivePath,
                param,
                magnitude,
                format!("Security-sensitive file: {why}"),
            );
        }
    }
    if diff.kind != DiffKind::Text {
        return RiskFacts {
            facts: b.facts,
            churn,
        };
    }

    let old_src = side_text(&diff.old);
    let new_src = side_text(&diff.new);
    let ld: &LineDiff = &diff.diff;
    let deleted_file = !diff.new.exists || diff.change.kind == ChangeKind::Deleted;

    // Language (new side preferred; a deleted file only has the old one).
    let language: Option<std::sync::Arc<Language>> = diff
        .new
        .language
        .clone()
        .filter(|_| diff.new.exists)
        .or_else(|| diff.old.language.clone())
        .or_else(|| langs.language_for_path(std::path::Path::new(&path), new_src.lines().next()));
    let mut family = family_of(language.as_deref());
    if family == Family::Other && extension(&path.to_ascii_lowercase()) == "sql" {
        family = Family::Sql;
    }

    // Dampening classes.
    let generated = generated_reason(&path, &old_src, &new_src);
    let lock = is_lockfile(&path);
    if let Some(why) = generated {
        b.add(
            RiskKind::Generated,
            param!(damp_generated),
            1.0,
            format!("Generated file ({why}): little review value"),
        );
    }
    if lock {
        b.add(
            RiskKind::Lockfile,
            param!(lockfile),
            1.0,
            "Lockfile changed (dependency versions)".into(),
        );
        b.add(
            RiskKind::Generated,
            param!(damp_lockfile),
            1.0,
            "Lockfiles are machine-written: contents damped".into(),
        );
    }
    if is_test {
        b.add(
            RiskKind::TestFile,
            param!(damp_test),
            1.0,
            "Test file".into(),
        );
    }
    if is_docs_path(&path) {
        b.add(
            RiskKind::Docs,
            param!(damp_docs),
            1.0,
            "Documentation".into(),
        );
    }
    let low_value = generated.is_some() || lock;

    // Manifests and config.
    if is_manifest(&path) && !lock {
        b.add(
            RiskKind::DependencyManifest,
            param!(manifest),
            1.0,
            format!(
                "`{}` changed: dependencies or build configuration",
                file_name(&path)
            ),
        );
    } else if is_config_ext(&path) && !lock && generated.is_none() {
        b.add(
            RiskKind::Config,
            param!(config),
            1.0,
            "Configuration file changed".into(),
        );
    }

    // Churn.
    if !low_value {
        let tier = match churn {
            0..=39 => 0.0,
            40..=149 => 1.0,
            150..=399 => 2.0,
            400..=999 => 3.0,
            _ => 3.5,
        };
        if tier > 0.0 {
            b.add(
                RiskKind::LargeChurn,
                param!(churn),
                tier,
                format!(
                    "Large change: {} lines added, {} removed",
                    stats.added, stats.removed
                ),
            );
        }
    }

    // Deleted files.
    if deleted_file && stats.removed > 0 {
        if is_test {
            b.add(
                RiskKind::TestsRemoved,
                param!(tests_removed),
                1.0,
                format!("Test file deleted ({} lines)", stats.removed),
            );
        } else if family.is_code() && stats.removed >= 20 && !low_value {
            b.add(
                RiskKind::DeletedFile,
                param!(deleted_file),
                1.0,
                format!("Source file deleted ({} lines)", stats.removed),
            );
        }
    }

    // Line-level signals: code, configuration, manifests, CI and container files.
    if !low_value && !is_docs_path(&path) {
        let prefixes = comment_prefixes_for(family, &path);
        let removed_rows: Vec<Row> = rows_in(&old_src, ld.hunks.iter().map(|h| h.old.clone()))
            .into_iter()
            .filter(|r| !is_comment(r.text, prefixes))
            .collect();
        // Rows of the new text that are test code (`mod tests`) are skipped
        // for added-code signals.
        let test_rows = if family == Family::Rust && !is_test {
            rust_test_rows(diff)
        } else {
            Vec::new()
        };
        let added_rows: Vec<Row> = rows_in(&new_src, ld.hunks.iter().map(|h| h.new.clone()))
            .into_iter()
            .filter(|r| {
                !is_comment(r.text, prefixes) && !test_rows.iter().any(|t| t.contains(&r.n))
            })
            .collect();
        let removed = texts(&removed_rows);
        let added = texts(&added_rows);
        let hunks: Vec<(Range<u32>, Range<u32>)> = ld
            .hunks
            .iter()
            .map(|h| (h.old.clone(), h.new.clone()))
            .collect();

        if family.is_code() {
            code_signals(
                &mut b,
                family,
                is_test,
                &removed_rows,
                &added_rows,
                &removed,
                &added,
            );
        }

        // Content signals (auth, crypto, migrations, concurrency, manifests, CI, containers, tests, defaults).
        let cx = signals::Ctx {
            path: &path,
            family,
            is_test,
            removed: &removed_rows,
            added: &added_rows,
            hunks: &hunks,
            new_src: &new_src,
        };
        for f in signals::detect(&cx) {
            b.add_at(f.kind, f.param, f.magnitude, f.message, f.at, f.rows);
        }

        if family.is_code() {
            // Tests/assertions removed from test files.
            if let (true, Some(p)) = (is_test, patterns()) {
                let tests_net = -net_added(&p.test_decl, &removed, &added);
                let asserts_net = -net_added(&p.assertion, &removed, &added);
                if tests_net >= 1 {
                    let rows = first_run(&removed_rows, &p.test_decl);
                    let mag = 1.0 + (0.05 / 0.3) * ((tests_net - 1).min(4) as f32);
                    let msg = format!("Removes {tests_net} test(s)");
                    match rows {
                        Some(r) => b.add_at(
                            RiskKind::TestsRemoved,
                            param!(tests_removed),
                            mag,
                            msg,
                            At::Old,
                            r,
                        ),
                        None => b.add(RiskKind::TestsRemoved, param!(tests_removed), mag, msg),
                    }
                } else if asserts_net >= 2 {
                    let msg = format!("Removes {asserts_net} assertions from tests");
                    match first_run(&removed_rows, &p.assertion) {
                        Some(r) => b.add_at(
                            RiskKind::TestsRemoved,
                            param!(tests_removed),
                            0.8,
                            msg,
                            At::Old,
                            r,
                        ),
                        None => b.add(RiskKind::TestsRemoved, param!(tests_removed), 0.8, msg),
                    }
                }
            }

            // Source changed, no test changed anywhere in the changeset.
            if !is_test && ctx.tests_changed == Some(false) && churn >= 15 {
                b.add(
                    RiskKind::SourceWithoutTests,
                    param!(source_without_tests),
                    1.0,
                    "Source changed but no test file changed in this changeset".into(),
                );
            }

            // Public API.
            if let Some(lang) = &language {
                if matches!(
                    lang.name.as_str(),
                    "Rust"
                        | "TypeScript"
                        | "TSX"
                        | "JavaScript"
                        | "Python"
                        | "Go"
                        | "Java"
                        | "Kotlin"
                        | "Swift"
                        | "C#"
                        | "C"
                        | "C++"
                        | "PHP"
                        | "Ruby"
                ) && !is_test
                {
                    api_reasons(&mut b, lang, diff, &old_src, &new_src);
                }
            }
        }
    }
    RiskFacts {
        facts: b.facts,
        churn,
    }
}

/// Signals of the code families that need regex tables (error handling, conditionals, panics, danger, SQL).
fn code_signals(
    b: &mut Builder,
    family: Family,
    is_test: bool,
    removed_rows: &[Row],
    added_rows: &[Row],
    removed: &[&str],
    added: &[&str],
) {
    let Some(p) = patterns().filter(|_| !is_test) else {
        return;
    };
    let new_run = |re: &Regex| first_run(added_rows, re);
    let old_run = |re: &Regex| first_run(removed_rows, re);
    let at = |b: &mut Builder,
              kind,
              param,
              mag: f32,
              msg: String,
              new: Option<Range<u32>>,
              old: Option<Range<u32>>| {
        match (new, old) {
            (Some(r), _) => b.add_at(kind, param, mag, msg, At::New, r),
            (None, Some(r)) => b.add_at(kind, param, mag, msg, At::Old, r),
            (None, None) => b.add(kind, param, mag, msg),
        }
    };

    // Error handling removed.
    let err = match family {
        Family::Rust => Some(&p.err_rust),
        Family::Script => Some(&p.err_script),
        Family::Python => Some(&p.err_python),
        Family::Go => Some(&p.err_go),
        Family::Jvm => Some(&p.err_jvm),
        Family::CLike | Family::Dynamic | Family::Shell => Some(&p.err_generic),
        _ => None,
    };
    if let Some(re) = err {
        let net = -net_added(re, removed, added);
        if net >= 1 {
            let mag = 1.0 + 0.25 * ((net - 1).min(4) as f32);
            at(
                b,
                RiskKind::ErrorHandlingRemoved,
                param!(error_handling),
                mag,
                format!(
                    "Removes error handling ({net} fewer error paths: `?`/catch/except/err checks)"
                ),
                None,
                old_run(re),
            );
        }
    }
    // Conditionals removed.
    if family != Family::Sql {
        let net = -net_added(&p.cond, removed, added);
        if net >= 1 {
            let mag = 1.0 + (net.min(5) as f32) / 3.0;
            at(
                b,
                RiskKind::ConditionalsRemoved,
                param!(conditionals),
                mag,
                format!("Deletes {net} conditional branch(es) (if/match/switch)"),
                None,
                old_run(&p.cond),
            );
        }
    }
    // Panics added.
    let panic_re = match family {
        Family::Rust => Some(&p.panic_rust),
        Family::Go => Some(&p.panic_go),
        _ => None,
    };
    if let Some(re) = panic_re {
        let net = net_added(re, removed, added);
        if net >= 1 {
            let mag = 1.0 + 0.25 * ((net - 1).min(4) as f32);
            at(
                b,
                RiskKind::PanicAdded,
                param!(panic_added),
                mag,
                format!("Adds {net} unwrap()/expect()/panic! call(s) that can crash at runtime"),
                new_run(re),
                None,
            );
        }
    }
    // Dangerous constructs.
    for d in &p.danger {
        if !d.families.contains(&family) {
            continue;
        }
        let net = net_added(&d.re, removed, added);
        if net >= 1 {
            at(
                b,
                d.kind,
                d.param,
                1.0,
                format!("Adds {}", d.what),
                new_run(&d.re),
                None,
            );
        }
    }
    // SQL built with string formatting.
    let sql_line = |l: &str| p.sql_stmt.is_match(l) && p.sql_dynamic.is_match(l);
    let sql_net = |lines: &[&str]| lines.iter().filter(|l| sql_line(l)).count() as isize;
    if family != Family::Sql && sql_net(added) - sql_net(removed) >= 1 {
        let row = added_rows
            .iter()
            .find(|r| sql_line(r.text))
            .map(|r| r.n..r.n + 1);
        at(
            b,
            RiskKind::SqlStringBuilding,
            param!(sql_string),
            1.0,
            "Builds SQL by string formatting (injection risk)".into(),
            row,
            None,
        );
    }
    // Assertions removed from source.
    let net = -net_added(&p.assertion, removed, added);
    if net >= 2 {
        at(
            b,
            RiskKind::TestsRemoved,
            param!(assertions_removed),
            1.0,
            format!("Removes {net} assertions from non-test code"),
            None,
            old_run(&p.assertion),
        );
    }
}

fn rust_test_rows(diff: &FileDiff) -> Vec<std::ops::Range<u32>> {
    let (Some(syntax), Some(text)) = (&diff.new.syntax, &diff.new.text) else {
        return Vec::new();
    };
    syntax
        .outline(text)
        .iter()
        .filter(|i| i.kind == SymbolKind::Module && matches!(i.name.as_str(), "tests" | "test"))
        .map(|i| text.offset_to_point(i.range.start).row..text.offset_to_point(i.range.end).row + 1)
        .collect()
}
