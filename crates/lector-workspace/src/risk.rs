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
use std::collections::{BTreeMap, HashSet};
use std::ops::Range;
use std::sync::OnceLock;

use lector_diff::LineDiff;
use lector_git::ChangeKind;
use lector_syntax::{Language, LanguageRegistry, OutlineItem, SymbolKind, Syntax};
use lector_text::Text;
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
// Classification

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Family {
    Rust,
    Script, // TypeScript, TSX, JavaScript
    Python,
    Go,
    Jvm, // Java, Kotlin, C#, Swift
    CLike,
    Dynamic, // Ruby, PHP
    Shell,
    Sql,
    Other, // markup, data, docs
}

fn family_of(lang: Option<&Language>) -> Family {
    match lang.map(|l| l.name.as_str()) {
        Some("Rust") => Family::Rust,
        Some("TypeScript" | "TSX" | "JavaScript") => Family::Script,
        Some("Python") => Family::Python,
        Some("Go") => Family::Go,
        Some("Java" | "Kotlin" | "Swift" | "C#") => Family::Jvm,
        Some("C" | "C++") => Family::CLike,
        Some("Ruby" | "PHP") => Family::Dynamic,
        Some("Bash") => Family::Shell,
        Some("SQL") => Family::Sql,
        _ => Family::Other,
    }
}

impl Family {
    fn is_code(self) -> bool {
        !matches!(self, Family::Other)
    }
    fn comment_prefixes(self) -> &'static [&'static str] {
        match self {
            Family::Python | Family::Shell | Family::Dynamic => &["#", "//"],
            Family::Sql => &["--"],
            Family::Other => &[],
            _ => &["//", "/*", "*"],
        }
    }
}

fn path_of(f: &FileDiff) -> String {
    f.change
        .new_path
        .clone()
        .or_else(|| f.change.old_path.clone())
        .unwrap_or_default()
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn extension(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or("", |(_, e)| e)
}

fn is_test_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    lower.split('/').any(|seg| {
        matches!(
            seg,
            "test" | "tests" | "__tests__" | "spec" | "specs" | "testdata" | "fixtures" | "e2e"
        )
    }) || name.starts_with("test_")
        || name.contains("_test.")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("_spec.rb")
        || name.ends_with("tests.rs")
        || name.ends_with("test.java")
        || name.ends_with("tests.swift")
        || name.ends_with("tests.cs")
}

const LOCKFILES: &[&str] = &[
    "cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "poetry.lock",
    "pipfile.lock",
    "gemfile.lock",
    "go.sum",
    "composer.lock",
    "flake.lock",
    "bun.lockb",
    "packages.lock.json",
    "mix.lock",
    "pubspec.lock",
    "podfile.lock",
    "gradle.lockfile",
    "uv.lock",
];

const MANIFESTS: &[&str] = &[
    "cargo.toml",
    "package.json",
    "go.mod",
    "requirements.txt",
    "pyproject.toml",
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "gemfile",
    "composer.json",
    "setup.py",
    "setup.cfg",
    "pipfile",
    "cmakelists.txt",
    "makefile",
    "package.swift",
    "deno.json",
];

fn is_lockfile(path: &str) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    LOCKFILES.contains(&name.as_str())
}

fn is_manifest(path: &str) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    MANIFESTS.contains(&name.as_str()) || name.ends_with(".csproj") || name.ends_with(".gemspec")
}

fn is_config_ext(path: &str) -> bool {
    matches!(
        extension(&path.to_ascii_lowercase()),
        "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "properties" | "env" | "plist"
    )
}

fn is_docs_path(path: &str) -> bool {
    let name = file_name(&path.to_ascii_lowercase()).to_string();
    // `requirements.txt` and `CMakeLists.txt` are manifests, not prose.
    if name.starts_with("requirements") || name == "cmakelists.txt" || is_manifest(path) {
        return false;
    }
    matches!(
        extension(&path.to_ascii_lowercase()),
        "md" | "markdown" | "txt" | "rst" | "adoc" | "mdx"
    )
}

fn generated_reason(path: &str, old: &str, new: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    if lower
        .split('/')
        .any(|s| matches!(s, "vendor" | "node_modules" | "third_party"))
    {
        return Some("vendored code");
    }
    if name.contains(".min.") {
        return Some("minified file");
    }
    if name.ends_with(".pb.go")
        || name.ends_with(".pb.rs")
        || name.ends_with("_pb2.py")
        || name.ends_with(".g.dart")
        || name.ends_with(".generated.ts")
        || name.contains("_generated.")
        || name.ends_with(".snap")
        || name.ends_with(".designer.cs")
    {
        return Some("generated file name");
    }
    let marker = |s: &str| {
        s.lines().take(20).any(|l| {
            l.contains("@generated")
                || l.contains("DO NOT EDIT")
                || l.contains("Code generated")
                || l.contains("auto-generated")
                || l.contains("autogenerated")
        })
    };
    if marker(new) || marker(old) {
        return Some("generated marker in the header");
    }
    // Minified: very long lines in a very short file.
    let looks_minified = |s: &str| {
        let lines = s.lines().count().max(1);
        s.len() > 2000 && s.len() / lines > 300 && lines < 20
    };
    if looks_minified(new) || (new.is_empty() && looks_minified(old)) {
        return Some("minified content");
    }
    None
}

/// Words of a path, split on separators and camelCase boundaries, lowercased.
fn path_words(path: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in path.chars() {
        if !c.is_alphanumeric() {
            if !cur.is_empty() {
                words.push(std::mem::take(&mut cur));
            }
            prev_lower = false;
            continue;
        }
        if c.is_uppercase() && prev_lower && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        prev_lower = c.is_lowercase();
        cur.extend(c.to_lowercase());
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    words
}

fn security_path_reason(path: &str) -> Option<(Param, String)> {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    if lower.contains(".github/workflows/") || lower.contains(".gitlab-ci") || name == "jenkinsfile"
    {
        return Some((
            param!(path_ci),
            "CI/CD pipeline definition (runs with repository secrets)".into(),
        ));
    }
    if name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
        || name.ends_with(".dockerfile")
    {
        return Some((
            param!(path_container),
            "container build/runtime definition".into(),
        ));
    }
    if lower
        .split('/')
        .any(|s| matches!(s, "migrations" | "migrate" | "migration"))
    {
        return Some((param!(path_migration), "database migration".into()));
    }
    if name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
    {
        return Some((param!(path_secrets), "secrets/credentials file".into()));
    }
    if name.ends_with(".tf")
        || lower
            .split('/')
            .any(|s| matches!(s, "terraform" | "k8s" | "kubernetes" | "helm" | "ansible"))
    {
        return Some((param!(path_infra), "infrastructure definition".into()));
    }
    const WORDS: &[&str] = &[
        "auth",
        "authn",
        "authz",
        "authentication",
        "authorization",
        "oauth",
        "oidc",
        "saml",
        "sso",
        "login",
        "logout",
        "session",
        "sessions",
        "token",
        "tokens",
        "jwt",
        "credential",
        "credentials",
        "secret",
        "secrets",
        "password",
        "passwd",
        "crypto",
        "cryptography",
        "encrypt",
        "encryption",
        "decrypt",
        "cipher",
        "tls",
        "ssl",
        "certificate",
        "certs",
        "cert",
        "permission",
        "permissions",
        "acl",
        "rbac",
        "sudo",
        "security",
        "firewall",
        "sandbox",
        "cors",
        "csrf",
        "sanitize",
        "sanitizer",
        "keychain",
        "keystore",
        "signing",
        "signature",
    ];
    let words = path_words(path);
    let hit = words.iter().find(|w| WORDS.contains(&w.as_str()))?;
    Some((
        param!(path_auth),
        format!("`{hit}` in the path suggests authentication/crypto/permissions code"),
    ))
}

// ---------------------------------------------------------------------------
// Patterns

struct Patterns {
    cond: Regex,
    err_rust: Regex,
    err_script: Regex,
    err_python: Regex,
    err_go: Regex,
    err_jvm: Regex,
    err_generic: Regex,
    panic_rust: Regex,
    panic_go: Regex,
    assertion: Regex,
    test_decl: Regex,
    sql_stmt: Regex,
    sql_dynamic: Regex,
    danger: Vec<Danger>,
}

struct Danger {
    re: Regex,
    families: &'static [Family],
    kind: RiskKind,
    param: Param,
    what: &'static str,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static pattern");
        let d = |s: &str, families: &'static [Family], kind, param, what| Danger {
            re: re(s),
            families,
            kind,
            param,
            what,
        };
        use Family::*;
        Patterns {
            cond: re(r"\b(if|else|elif|match|switch|case|while|guard|unless|when)\b"),
            err_rust: re(r"(?:\w|\)|\]|>)\?\s*(?:[;.,)}\]]|$)|\.map_err\(|\.ok_or(?:_else)?\(|\.context\(|\.with_context\(|\bErr\("),
            err_script: re(r"\bcatch\b|\.catch\(|\bfinally\b|\bthrow\b|\btry\s*\{"),
            err_python: re(r"\bexcept\b|\bfinally\b|\braise\b|\btry\s*:"),
            err_go: re(r"\berr\s*!=\s*nil|errors\.(?:Is|As|New)\b|fmt\.Errorf|\brecover\(\)"),
            err_jvm: re(r"\bcatch\b|\bfinally\b|\bthrows?\b|\btry\b|\bguard\b"),
            err_generic: re(r"\bcatch\b|\bfinally\b|\brescue\b|\bensure\b|\braise\b|\bthrow\b|\berrno\b|\bperror\b|set\s+-e|\|\|\s*exit|\btrap\b"),
            panic_rust: re(r"\.unwrap\(\)|\.expect\(|\bpanic!\(|\bunreachable!\(|\btodo!\(|\bunimplemented!\("),
            panic_go: re(r"\bpanic\("),
            assertion: re(r"\bassert\w*\s*[!(]|\bassert\s|\bexpect\(|\.should\b|\brequire\("),
            test_decl: re(r"#\[test\]|#\[tokio::test\]|\bdef test_|\bfunc Test|\b(?:it|test)\(|@Test\b|\bfn test_"),
            sql_stmt: re(r"(?i)\b(?:select\s.+\sfrom|insert\s+into|update\s+\w+\s+set|delete\s+from)\b"),
            sql_dynamic: re(r#"\+\s*\w|"\s*\+|\bformat!?\(|\bf"|\bf'|["']\s*%\s*[(\w]|%s[^"']*["']\s*(?:$|;)|\$\{|\.format\(|\{\w*\}|\bSprintf\("#),
            danger: vec![
                d(r"\bunsafe\b", &[Rust, Jvm], RiskKind::UnsafeCode, param!(unsafe_code), "`unsafe` code"),
                d(r#"\bunsafe\.|"unsafe""#, &[Go], RiskKind::UnsafeCode, param!(unsafe_code), "`unsafe` package use"),
                d(r"\beval\s*\(|\bnew Function\(|\b__import__\(|\bexec\s*\(\s*[^)]", &[Script, Python, Dynamic], RiskKind::DynamicExecution, param!(dynamic_eval), "dynamic code evaluation (`eval`/`exec`)"),
                d(r"child_process|\bexecSync\(|\bspawnSync\(|\bspawn\(|\bexecFile\(", &[Script], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution"),
                d(r"\bsubprocess\.|\bos\.system\(|\bos\.popen\(|shell\s*=\s*True", &[Python], RiskKind::DynamicExecution, param!(dynamic_exec), "subprocess/shell execution"),
                d(r"\bpickle\.loads?\(|\byaml\.load\(|\bmarshal\.loads?\(", &[Python], RiskKind::DynamicExecution, param!(dynamic_exec), "unsafe deserialization"),
                d(r"Command::new|std::process::Command|\bprocess::Command", &[Rust], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution"),
                d(r#"\bexec\.Command\(|"os/exec"|\bsyscall\.Exec"#, &[Go], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution"),
                d(r"Runtime\.getRuntime\(\)\.exec|\bProcessBuilder\b|\bProcess\.Start\(|Class\.forName\(|ObjectInputStream", &[Jvm], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution / reflection / deserialization"),
                d(r"\b(?:shell_exec|passthru|popen|proc_open|system|exec)\s*\(|%x\{|\bunserialize\(", &[Dynamic], RiskKind::DynamicExecution, param!(dynamic_exec), "process execution"),
                d(r"\bsystem\s*\(|\bpopen\s*\(|\bstrcpy\s*\(|\bsprintf\s*\(|\bgets\s*\(", &[CLike], RiskKind::DynamicExecution, param!(dynamic_exec), "shell execution / unbounded string function"),
                d(r"\beval\b|curl[^|]*\|\s*(?:ba)?sh|\bsudo\b|\brm\s+-rf\b|\bchmod\s+(?:-R\s+)?777", &[Shell], RiskKind::DynamicExecution, param!(dynamic_exec), "shell eval/sudo/destructive command"),
                d(r"\binnerHTML\b|dangerouslySetInnerHTML|document\.write\(", &[Script], RiskKind::DynamicExecution, param!(html_injection), "raw HTML injection"),
            ],
        }
    })
}

fn count(re: &Regex, lines: &[&str]) -> usize {
    lines.iter().map(|l| re.find_iter(l).count()).sum()
}

fn net_added(re: &Regex, removed: &[&str], added: &[&str]) -> isize {
    count(re, added) as isize - count(re, removed) as isize
}

fn is_comment(line: &str, prefixes: &[&str]) -> bool {
    let t = line.trim_start();
    prefixes.iter().any(|p| t.starts_with(p))
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
            if is_test {
                let p = patterns();
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
    if is_test {
        return;
    }
    let p = patterns();
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

// ---------------------------------------------------------------------------
// Public API comparison

#[derive(Debug, Clone)]
struct ApiItem {
    key: String,
    label: &'static str,
    display: String,
    signature: String,
    /// Rows of the item (its first lines), in the text of its own side.
    rows: Range<u32>,
}

fn is_whole_text_kind(k: SymbolKind) -> bool {
    matches!(
        k,
        SymbolKind::Struct
            | SymbolKind::Enum
            | SymbolKind::Interface
            | SymbolKind::Type
            | SymbolKind::Constant
            | SymbolKind::Variable
            | SymbolKind::Field
    )
}

fn strip_comments(s: &str, c_style: bool) -> String {
    if !c_style {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else {
            let ch_len = s[i..].chars().next().map_or(1, char::len_utf8);
            out.push_str(&s[i..i + ch_len]);
            i += ch_len;
        }
    }
    out
}

fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Byte length of the header of `item` (up to the body).
fn header_len(item: &str, python: bool) -> usize {
    let (mut paren, mut bracket) = (0i32, 0i32);
    let b = item.as_bytes();
    for (i, &c) in b.iter().enumerate() {
        match c {
            b'(' => paren += 1,
            b')' => paren -= 1,
            b'[' => bracket += 1,
            b']' => bracket -= 1,
            b'{' | b';' if paren <= 0 && bracket <= 0 && !python => return i,
            b':' if python && paren <= 0 && bracket <= 0 => return i,
            b'=' if paren <= 0 && bracket <= 0 && b.get(i + 1) == Some(&b'>') => return i,
            _ => {}
        }
    }
    item.len()
}

fn first_word_is(s: &str, words: &[&str]) -> bool {
    s.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|w| words.contains(&w))
}

fn rust_pub(head: &str) -> bool {
    let mut h = head.trim_start();
    while h.starts_with("#[") {
        match h.find(']') {
            Some(e) => h = h[e + 1..].trim_start(),
            None => break,
        }
    }
    h.strip_prefix("pub")
        .is_some_and(|rest| rest.starts_with(char::is_whitespace))
}

fn api_items(lang: &Language, syntax: &Syntax, text: &Text, src: &str) -> Vec<ApiItem> {
    let outline = syntax.outline(text);
    let mut out: Vec<ApiItem> = Vec::new();
    let mut stack: Vec<(usize, &OutlineItem, bool)> = Vec::new(); // (index, item, exported)
    let name = lang.name.as_str();
    let c_style = !matches!(name, "Python" | "Ruby");
    for (ix, item) in outline.iter().enumerate() {
        while stack.last().is_some_and(|(_, p, _)| p.depth >= item.depth) {
            stack.pop();
        }
        let parent = stack.last().copied();
        let range = item.range.clone();
        let Some(item_src) = src.get(range.clone()) else {
            continue;
        };
        let line_start = src[..range.start].rfind('\n').map_or(0, |i| i + 1);
        let prefix = &src[line_start..range.start];
        let python = name == "Python";
        let hl = if name == "Ruby" {
            item_src.find('\n').unwrap_or(item_src.len())
        } else {
            header_len(item_src, python)
        };
        let header = format!("{prefix}{}", &item_src[..hl]);
        let has_methods = outline[ix + 1..]
            .iter()
            .take_while(|n| n.depth > item.depth)
            .any(|n| matches!(n.kind, SymbolKind::Method | SymbolKind::Function));
        let whole = is_whole_text_kind(item.kind) && !has_methods;

        // Local items (inside function bodies) are not API; fields of whole-text
        // parents are part of the parent's definition.
        let inside_fn = stack
            .iter()
            .any(|(_, p, _)| matches!(p.kind, SymbolKind::Function | SymbolKind::Method));
        let parent_whole = stack.iter().any(|(pi, p, _)| {
            is_whole_text_kind(p.kind)
                && !outline[pi + 1..]
                    .iter()
                    .take_while(|n| n.depth > p.depth)
                    .any(|n| matches!(n.kind, SymbolKind::Method | SymbolKind::Function))
        });
        let parent_exported = parent.map(|(_, _, e)| e);
        let inherits = matches!(
            parent.map(|(_, p, _)| p.kind),
            Some(SymbolKind::Trait | SymbolKind::Interface)
        ) || (matches!(parent.map(|(_, p, _)| p.kind), Some(SymbolKind::Enum))
            && item.kind == SymbolKind::Field);

        let exported_own = match name {
            "Rust" => rust_pub(&header),
            "TypeScript" | "TSX" | "JavaScript" => {
                let h = header.trim_start();
                if parent.is_some_and(|(_, p, _)| matches!(p.kind, SymbolKind::Class)) {
                    !first_word_is(
                        &header[..header.find(&item.name).unwrap_or(header.len())],
                        &["private", "protected"],
                    ) && !item.name.starts_with('#')
                } else {
                    h.starts_with("export")
                }
            }
            "Python" => {
                !item.name.starts_with('_')
                    || (item.name.starts_with("__") && item.name.ends_with("__"))
            }
            "Go" => item.name.chars().next().is_some_and(char::is_uppercase),
            "Java" | "C#" => first_word_is(&header, &["public", "protected"]),
            "Swift" => first_word_is(&header, &["public", "open"]),
            "Kotlin" => !first_word_is(&header, &["private", "internal", "protected"]),
            "PHP" => !first_word_is(&header, &["private", "protected"]),
            "C" | "C++" => !first_word_is(&header, &["static"]),
            "Ruby" => !item.name.starts_with('_'),
            _ => false,
        };
        let exported = if inherits {
            parent_exported.unwrap_or(false)
        } else if let Some(pe) = parent_exported {
            // Members need an exported container (except Go, where names decide, and Rust `impl` blocks).
            let container_kind = parent.map(|(_, p, _)| p.kind);
            if name == "Go" || (name == "Rust" && container_kind == Some(SymbolKind::Other)) {
                exported_own
            } else {
                exported_own && pe
            }
        } else {
            exported_own
        };
        stack.push((ix, item, exported));
        if !exported || inside_fn || parent_whole {
            continue;
        }

        let sig_src = if whole { item_src } else { &item_src[..hl] };
        let signature = normalize_ws(&strip_comments(sig_src, c_style));
        let qualified: Vec<&str> = stack
            .iter()
            .take(stack.len() - 1)
            .map(|(_, p, _)| p.name.as_str())
            .chain(std::iter::once(item.name.as_str()))
            .collect();
        let display = qualified.join("::");
        out.push(ApiItem {
            key: format!("{}:{display}", item.kind.label()),
            label: item.kind.label(),
            display,
            signature,
            rows: {
                let first = text.offset_to_point(item.range.start).row;
                let last = text.offset_to_point(item.range.end).row;
                first..(last + 1).min(first + 8)
            },
        });
    }
    out
}

fn side_api(diff_side: &DiffSide, lang: &Language, src: &str) -> Vec<ApiItem> {
    if !diff_side.exists {
        return Vec::new();
    }
    let (Some(text), Some(syntax)) = (&diff_side.text, &diff_side.syntax) else {
        return Vec::new();
    };
    api_items(lang, syntax, text, src)
}

fn clip(s: &str) -> String {
    if s.chars().count() > 90 {
        let cut: String = s.chars().take(87).collect();
        format!("{cut}...")
    } else {
        s.to_string()
    }
}

fn api_reasons(b: &mut Builder, lang: &Language, diff: &FileDiff, old_src: &str, new_src: &str) {
    // Both sides must have been parsed (or be absent).
    if (diff.old.exists && diff.old.syntax.is_none())
        || (diff.new.exists && diff.new.syntax.is_none())
    {
        return;
    }
    let old = side_api(&diff.old, lang, old_src);
    let new = side_api(&diff.new, lang, new_src);
    let mut old_by: BTreeMap<&str, Vec<&ApiItem>> = BTreeMap::new();
    for i in &old {
        old_by.entry(i.key.as_str()).or_default().push(i);
    }
    let mut new_by: BTreeMap<&str, Vec<&ApiItem>> = BTreeMap::new();
    for i in &new {
        new_by.entry(i.key.as_str()).or_default().push(i);
    }
    let mut removed = Vec::new();
    let mut changed = Vec::new();
    let mut added = Vec::new();
    let keys: HashSet<&str> = old_by.keys().chain(new_by.keys()).copied().collect();
    let mut keys: Vec<&str> = keys.into_iter().collect();
    keys.sort_unstable();
    for k in keys {
        match (old_by.get(k), new_by.get(k)) {
            (Some(o), None) => removed.extend(o.iter().copied()),
            (None, Some(n)) => added.extend(n.iter().copied()),
            (Some(o), Some(n)) => {
                let o_sigs: Vec<&str> = o.iter().map(|i| i.signature.as_str()).collect();
                let n_sigs: Vec<&str> = n.iter().map(|i| i.signature.as_str()).collect();
                let mut o_left: Vec<&ApiItem> = o
                    .iter()
                    .copied()
                    .filter(|i| !n_sigs.contains(&i.signature.as_str()))
                    .collect();
                let mut n_left: Vec<&ApiItem> = n
                    .iter()
                    .copied()
                    .filter(|i| !o_sigs.contains(&i.signature.as_str()))
                    .collect();
                while !o_left.is_empty() && !n_left.is_empty() {
                    changed.push((o_left.remove(0), n_left.remove(0)));
                }
                removed.extend(o_left);
                added.extend(n_left);
            }
            (None, None) => {}
        }
    }
    const SHOWN: usize = 5;
    for i in removed.iter().take(SHOWN) {
        b.add_at(
            RiskKind::PublicApiRemoved,
            param!(api_removed),
            1.0,
            format!("Removes public {} `{}`", i.label, i.display),
            At::Old,
            i.rows.clone(),
        );
    }
    for (o, n) in changed.iter().take(SHOWN) {
        b.add_at(
            RiskKind::PublicApiChanged,
            param!(api_changed),
            1.0,
            format!(
                "Changes the signature of public {} `{}`: `{}` -> `{}`",
                n.label,
                n.display,
                clip(&o.signature),
                clip(&n.signature)
            ),
            At::New,
            n.rows.clone(),
        );
    }
    // New exports are additive and rarely break callers: one reason for all of them,
    // growing slowly with their number.
    if let Some(first) = added.first() {
        let names: Vec<String> = added
            .iter()
            .take(3)
            .map(|i| format!("{} `{}`", i.label, i.display))
            .collect();
        let more = added.len().saturating_sub(3);
        let list = if more > 0 {
            format!("{} and {more} more", names.join(", "))
        } else {
            names.join(", ")
        };
        b.add_at(
            RiskKind::PublicApiAdded,
            param!(api_added),
            1.0 + 0.1 * (added.len() - 1).min(4) as f32,
            format!("Adds public {list}"),
            At::New,
            first.rows.clone(),
        );
    }
    let extra = |total: usize| total.saturating_sub(SHOWN);
    if extra(removed.len()) > 0 {
        b.add(
            RiskKind::PublicApiRemoved,
            param!(api_removed),
            1.0,
            format!("... and {} more public items removed", extra(removed.len())),
        );
    }
    if extra(changed.len()) > 0 {
        b.add(
            RiskKind::PublicApiChanged,
            param!(api_changed),
            1.0,
            format!(
                "... and {} more public signatures changed",
                extra(changed.len())
            ),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_diff::DiffOptions;
    use lector_git::{FileChange, Side};

    fn change(path: &str, kind: ChangeKind) -> FileChange {
        FileChange {
            old_path: (kind != ChangeKind::Added).then(|| path.to_string()),
            new_path: (kind != ChangeKind::Deleted).then(|| path.to_string()),
            kind,
            old: if kind == ChangeKind::Added {
                Side::None
            } else {
                Side::Workdir(path.into())
            },
            new: if kind == ChangeKind::Deleted {
                Side::None
            } else {
                Side::Workdir(path.into())
            },
            binary: None,
        }
    }

    fn diff_of(path: &str, old: Option<&str>, new: Option<&str>) -> FileDiff {
        let kind = match (old, new) {
            (None, _) => ChangeKind::Added,
            (_, None) => ChangeKind::Deleted,
            _ => ChangeKind::Modified,
        };
        crate::review::diff_bytes(
            change(path, kind),
            old.map(|s| s.as_bytes().to_vec()),
            new.map(|s| s.as_bytes().to_vec()),
            &LanguageRegistry::new(),
            &DiffOptions::default(),
        )
    }

    fn report(path: &str, old: &str, new: &str) -> RiskReport {
        assess(
            &diff_of(path, Some(old), Some(new)),
            &LanguageRegistry::new(),
        )
    }

    fn has(r: &RiskReport, kind: RiskKind) -> bool {
        r.reasons.iter().any(|x| x.kind == kind)
    }

    #[test]
    fn identical_file_has_no_risk() {
        let r = report("src/lib.rs", "pub fn a() {}\n", "pub fn a() {}\n");
        assert_eq!(r.score, 0.0);
        assert!(r.reasons.is_empty());
        assert_eq!(r.level(), RiskLevel::Low);
    }

    #[test]
    fn rust_public_api_signature_change() {
        let old = "pub fn load(path: &str) -> String {\n    String::new()\n}\n\nfn helper() {}\n";
        let new = "pub fn load(path: &str, strict: bool) -> Result<String, Error> {\n    Ok(String::new())\n}\n\nfn helper() {}\n";
        let r = report("src/lib.rs", old, new);
        assert!(has(&r, RiskKind::PublicApiChanged), "{r:?}");
        assert!(r.score >= 0.4);
        assert!(r.reasons[0].message.contains("load"));
        // Body-only change keeps the API unchanged.
        let body = report(
            "src/lib.rs",
            old,
            &old.replace("String::new()", "String::from(\"x\")"),
        );
        assert!(!has(&body, RiskKind::PublicApiChanged), "{body:?}");
    }

    #[test]
    fn rust_private_changes_are_not_api() {
        let r = report(
            "src/lib.rs",
            "fn a(x: i32) {}\n",
            "fn a(x: i32, y: i32) {}\n",
        );
        assert!(
            !r.reasons.iter().any(|x| matches!(
                x.kind,
                RiskKind::PublicApiChanged | RiskKind::PublicApiRemoved
            )),
            "{r:?}"
        );
        let r = report(
            "src/lib.rs",
            "pub(crate) fn a(x: i32) {}\n",
            "pub(crate) fn a() {}\n",
        );
        assert!(!has(&r, RiskKind::PublicApiChanged), "{r:?}");
    }

    #[test]
    fn rust_public_api_removed_and_added() {
        let old = "pub fn keep() {}\npub fn gone() {}\n";
        let new = "pub fn keep() {}\npub fn fresh() {}\n";
        let r = report("src/lib.rs", old, new);
        assert!(has(&r, RiskKind::PublicApiRemoved));
        assert!(has(&r, RiskKind::PublicApiAdded));
        assert!(r.reasons.iter().any(|x| x.message.contains("gone")));
    }

    #[test]
    fn rust_struct_field_change_is_api_change() {
        let old = "pub struct Config {\n    pub name: String,\n}\n";
        let new = "pub struct Config {\n    pub name: String,\n    pub retries: u32,\n}\n";
        let r = report("src/lib.rs", old, new);
        assert!(has(&r, RiskKind::PublicApiChanged), "{r:?}");
    }

    #[test]
    fn typescript_export_change() {
        let old = "export function area(w: number, h: number): number {\n  return w * h;\n}\nfunction internal() {}\n";
        let new = "export function area(w: number): number {\n  return w * w;\n}\nfunction internal(x: number) {}\n";
        let r = report("src/shapes.ts", old, new);
        assert!(has(&r, RiskKind::PublicApiChanged), "{r:?}");
        assert_eq!(
            r.reasons
                .iter()
                .filter(|x| x.kind == RiskKind::PublicApiChanged)
                .count(),
            1
        );
    }

    #[test]
    fn python_underscore_is_private() {
        let old = "def public(a):\n    return a\n\ndef _private(a):\n    return a\n";
        let new = "def public(a, b):\n    return a\n\ndef _private(a, b):\n    return a\n";
        let r = report("pkg/mod.py", old, new);
        let changed: Vec<_> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .collect();
        assert_eq!(changed.len(), 1, "{r:?}");
        assert!(changed[0].message.contains("public"));
    }

    #[test]
    fn go_capitalized_is_exported() {
        let old = "package x\n\nfunc Exported(a int) int {\n\treturn a\n}\n\nfunc private(a int) int {\n\treturn a\n}\n";
        let new = "package x\n\nfunc Exported(a int, b int) int {\n\treturn a\n}\n\nfunc private(a int, b int) int {\n\treturn a\n}\n";
        let r = report("x/x.go", old, new);
        let n = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .count();
        assert_eq!(n, 1, "{r:?}");
    }

    #[test]
    fn java_public_method() {
        let old = "public class A {\n    public int f(int a) {\n        return a;\n    }\n    private int g(int a) {\n        return a;\n    }\n}\n";
        let new = "public class A {\n    public int f(int a, int b) {\n        return a;\n    }\n    private int g(int a, int b) {\n        return a;\n    }\n}\n";
        let r = report("src/A.java", old, new);
        let n = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .count();
        assert_eq!(n, 1, "{r:?}");
    }

    #[test]
    fn removed_error_handling() {
        let old = "fn run() -> Result<(), Error> {\n    let a = read()?;\n    let b = parse(a)?;\n    Ok(())\n}\n";
        let new = "fn run() {\n    let a = read().unwrap();\n    let b = parse(a).unwrap();\n}\n";
        let r = report("src/run.rs", old, new);
        assert!(has(&r, RiskKind::ErrorHandlingRemoved), "{r:?}");
        assert!(has(&r, RiskKind::PanicAdded), "{r:?}");
        let go_old = "func f() error {\n\tif err != nil {\n\t\treturn err\n\t}\n\treturn nil\n}\n";
        let go_new = "func f() error {\n\treturn nil\n}\n";
        assert!(has(
            &report("a.go", go_old, go_new),
            RiskKind::ErrorHandlingRemoved
        ));
        let py_old = "def f():\n    try:\n        g()\n    except ValueError:\n        pass\n";
        let py_new = "def f():\n    g()\n";
        assert!(has(
            &report("a.py", py_old, py_new),
            RiskKind::ErrorHandlingRemoved
        ));
        let js_old =
            "async function f() {\n  try {\n    await g();\n  } catch (e) {\n    log(e);\n  }\n}\n";
        let js_new = "async function f() {\n  await g();\n}\n";
        assert!(has(
            &report("a.js", js_old, js_new),
            RiskKind::ErrorHandlingRemoved
        ));
    }

    #[test]
    fn deleted_conditionals() {
        let old = "fn f(x: i32) -> i32 {\n    if x < 0 {\n        return 0;\n    }\n    if x > 10 {\n        return 10;\n    }\n    x\n}\n";
        let new = "fn f(x: i32) -> i32 {\n    x\n}\n";
        let r = report("src/f.rs", old, new);
        assert!(has(&r, RiskKind::ConditionalsRemoved), "{r:?}");
    }

    #[test]
    fn unwrap_in_test_module_is_ignored() {
        let old = "fn a() {}\n";
        let new = "fn a() {}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn t() {\n        a();\n        Some(1).unwrap();\n    }\n}\n";
        assert!(!has(&report("src/a.rs", old, new), RiskKind::PanicAdded));
        let new2 = "fn a() {\n    Some(1).unwrap();\n}\n";
        assert!(has(&report("src/a.rs", old, new2), RiskKind::PanicAdded));
    }

    #[test]
    fn dangerous_constructs() {
        let r = report(
            "src/a.rs",
            "fn a() {}\n",
            "fn a() {\n    unsafe { std::ptr::null::<u8>().read() };\n}\n",
        );
        assert!(has(&r, RiskKind::UnsafeCode), "{r:?}");
        let r = report(
            "src/a.rs",
            "fn a() {}\n",
            "fn a() {\n    std::process::Command::new(\"sh\").arg(x).status();\n}\n",
        );
        assert!(has(&r, RiskKind::DynamicExecution), "{r:?}");
        let r = report("a.js", "f();\n", "f();\neval(userInput);\n");
        assert!(has(&r, RiskKind::DynamicExecution), "{r:?}");
        let r = report("a.py", "f()\n", "f()\nsubprocess.run(cmd, shell=True)\n");
        assert!(has(&r, RiskKind::DynamicExecution), "{r:?}");
        let r = report(
            "a.py",
            "f()\n",
            "f()\nq = \"SELECT * FROM users WHERE id = \" + uid\n",
        );
        assert!(has(&r, RiskKind::SqlStringBuilding), "{r:?}");
        let r = report(
            "a.py",
            "f()\n",
            "f()\nq = \"SELECT * FROM users WHERE id = %s\"\n",
        );
        assert!(has(&r, RiskKind::SqlStringBuilding), "{r:?}");
        // Comments and unchanged occurrences do not count.
        let r = report(
            "src/a.rs",
            "fn a() { unsafe { x() } }\n",
            "fn a() { unsafe { x() } }\nfn b() {}\n",
        );
        assert!(!has(&r, RiskKind::UnsafeCode), "{r:?}");
        let r = report(
            "src/a.rs",
            "fn a() {}\n",
            "fn a() {}\n// unsafe is mentioned here\n",
        );
        assert!(!has(&r, RiskKind::UnsafeCode), "{r:?}");
    }

    #[test]
    fn security_sensitive_paths() {
        for p in [
            ".github/workflows/ci.yml",
            "Dockerfile",
            "db/migrations/001_init.sql",
            "src/auth/session.rs",
            "src/crypto.rs",
            "app/PermissionService.ts",
        ] {
            let r = report(p, "a: 1\n", "a: 2\n");
            assert!(has(&r, RiskKind::SecuritySensitivePath), "{p}: {r:?}");
        }
        for p in ["src/author.rs", "src/main.rs", "docs/authentication.md"] {
            let r = report(p, "a\n", "b\n");
            assert!(!has(&r, RiskKind::SecuritySensitivePath), "{p}: {r:?}");
        }
    }

    #[test]
    fn dampening_for_generated_lock_and_tests() {
        let body: String = (0..300).map(|i| format!("line {i}\n")).collect();
        let body2: String = (0..300).map(|i| format!("line {}\n", i + 1)).collect();
        let lock = report("Cargo.lock", &body, &body2);
        assert!(lock.score < 0.15, "{lock:?}");
        assert!(has(&lock, RiskKind::Lockfile));
        let generated = report(
            "src/gen.rs",
            "// @generated\nfn a() { unsafe { x() } }\n",
            "// @generated\nfn a() { unsafe { x() } }\nfn b() { unsafe { y() } }\n",
        );
        assert!(has(&generated, RiskKind::Generated));
        assert!(generated.score < 0.1, "{generated:?}");
        let plain = report(
            "src/gen.rs",
            "fn a() {}\n",
            "fn a() {}\nfn b() { unsafe { y() } }\n",
        );
        assert!(plain.score > generated.score);
        let in_test = report(
            "tests/t.rs",
            "fn a() {}\n",
            "fn a() {}\nfn b() { unsafe { y() } }\n",
        );
        assert!(in_test.score < plain.score);
        assert!(has(&in_test, RiskKind::TestFile));
    }

    #[test]
    fn tests_removed() {
        let old = "#[test]\nfn a() {\n    assert_eq!(1, 1);\n}\n#[test]\nfn b() {\n    assert_eq!(2, 2);\n}\n";
        let new = "#[test]\nfn a() {\n    assert_eq!(1, 1);\n}\n";
        let r = report("tests/t.rs", old, new);
        assert!(has(&r, RiskKind::TestsRemoved), "{r:?}");
        let d = assess(
            &diff_of("tests/t.rs", Some(old), None),
            &LanguageRegistry::new(),
        );
        assert!(has(&d, RiskKind::TestsRemoved), "{d:?}");
    }

    #[test]
    fn churn_and_manifest() {
        let old: String = (0..10).map(|i| format!("let a{i} = {i};\n")).collect();
        let new: String = (0..300)
            .map(|i| format!("let a{i} = {};\n", i + 1))
            .collect();
        let r = report("src/big.rs", &old, &new);
        assert!(has(&r, RiskKind::LargeChurn), "{r:?}");
        let m = report(
            "Cargo.toml",
            "[dependencies]\n",
            "[dependencies]\nevil = \"1\"\n",
        );
        assert!(has(&m, RiskKind::DependencyManifest), "{m:?}");
    }

    #[test]
    fn binary_and_summary() {
        let mut d = diff_of("a.bin", Some("x"), Some("y"));
        d.kind = DiffKind::Binary;
        let r = assess(&d, &LanguageRegistry::new());
        assert!(has(&r, RiskKind::BinaryChange));
        let s = report("src/auth.rs", "pub fn f() {}\n", "pub fn f(x: u8) {}\n").summary();
        assert!(s.starts_with("score "), "{s}");
    }

    #[test]
    fn ranking_is_deterministic_and_adds_test_context() {
        let big_old: String = (0..30).map(|i| format!("fn f{i}() {{}}\n")).collect();
        let big_new: String = (0..30)
            .map(|i| format!("fn f{i}() {{ g({i}); }}\n"))
            .collect();
        let files = vec![
            diff_of("README.md", Some("a\n"), Some("b\n")),
            diff_of("src/core.rs", Some(&big_old), Some(&big_new)),
            diff_of(
                "src/auth.rs",
                Some("pub fn login() {}\n"),
                Some("pub fn login(x: u8) {}\n"),
            ),
            diff_of("src/same.rs", Some("fn a() {}\n"), Some("fn a() {}\n")),
        ];
        let ranked = rank_files(&files);
        assert_eq!(ranked.len(), 4);
        assert_eq!(ranked[0].0, 2);
        assert_eq!(ranked.last().map(|r| r.0), Some(3));
        let again = rank_files(&files);
        assert_eq!(ranked, again);
        assert!(has(
            &ranked.iter().find(|r| r.0 == 1).unwrap().1,
            RiskKind::SourceWithoutTests
        ));
        // With a test file in the changeset the signal disappears.
        let mut with_test = files;
        with_test.push(diff_of("tests/t.rs", Some("a\n"), Some("b\n")));
        let ranked = rank_files(&with_test);
        assert!(!has(
            &ranked.iter().find(|r| r.0 == 1).unwrap().1,
            RiskKind::SourceWithoutTests
        ));
    }

    #[test]
    fn score_is_bounded() {
        let new = "pub fn a(x: u8) { unsafe { eval(x) }; let _ = f().unwrap(); }\n";
        for p in ["src/auth/crypto.rs", ".github/workflows/x.rs"] {
            let r = report(p, "pub fn a() {}\n", new);
            assert!((0.0..=1.0).contains(&r.score), "{r:?}");
        }
    }

    // ---- content signals, evidence, weights (0.7 evaluation set) ----

    fn reason(r: &RiskReport, kind: RiskKind) -> &RiskReason {
        r.reasons
            .iter()
            .find(|x| x.kind == kind)
            .unwrap_or_else(|| panic!("no {kind:?} in {:?}", r.reasons))
    }

    /// The text of the evidence lines of `r` on the side it points at.
    fn evidence_text(r: &RiskReason, old: &str, new: &str) -> String {
        let ev = r.evidence.as_ref().expect("evidence");
        let src = if ev.side == EvidenceSide::Old {
            old
        } else {
            new
        };
        src.lines()
            .skip(ev.lines.start as usize)
            .take((ev.lines.end - ev.lines.start) as usize)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn removed_authorization_check_has_old_side_evidence() {
        let old = "admin.delete('/u/:id', async (req, res) => {\n  if (!req.user.isAdmin) {\n    return res.status(403).end();\n  }\n  await removeUser(req.params.id);\n});\n";
        let new = "admin.delete('/u/:id', async (req, res) => {\n  await removeUser(req.params.id);\n});\n";
        let r = report("src/routes.ts", old, new);
        let a = reason(&r, RiskKind::AuthCheckRemoved);
        let ev = a.evidence.as_ref().expect("evidence");
        assert_eq!(ev.side, EvidenceSide::Old);
        assert_eq!(ev.path, "src/routes.ts");
        assert!(evidence_text(a, old, new).contains("isAdmin"), "{ev:?}");
        assert_eq!(a.line, None, "old-side evidence has no new-text line");
    }

    #[test]
    fn disabled_verification_points_at_the_added_line() {
        let old = "import requests\n\n\ndef get(u):\n    return requests.get(u, timeout=5)\n";
        let new = "import requests\n\n\ndef get(u):\n    return requests.get(u, timeout=5, verify=False)\n";
        let r = report("app/http.py", old, new);
        let v = reason(&r, RiskKind::VerificationDisabled);
        assert_eq!(v.line, Some(4));
        assert!(evidence_text(v, old, new).contains("verify=False"));
        assert_eq!(v.evidence.as_ref().map(|e| e.side), Some(EvidenceSide::New));
    }

    #[test]
    fn hardcoded_secret_but_not_environment_lookups() {
        let old = "export const cfg = { apiKey: process.env.API_KEY ?? '' };\n";
        let hard = "export const cfg = { apiKey: 'not-a-real-key-fixture-0000' };\n";
        assert!(has(
            &report("src/c.ts", old, hard),
            RiskKind::HardcodedSecret
        ));
        let env = "export const cfg = { apiKey: process.env.OTHER_KEY ?? 'unset-value' };\n";
        assert!(!has(
            &report("src/c.ts", old, env),
            RiskKind::HardcodedSecret
        ));
        let placeholder = "export const cfg = { apiKey: 'changeme-before-use' };\n";
        assert!(!has(
            &report("src/c.ts", old, placeholder),
            RiskKind::HardcodedSecret
        ));
    }

    #[test]
    fn weak_hash_and_removed_hardening() {
        let old = "import hashlib, os\n\ndef h(p):\n    salt = os.urandom(16)\n    return hashlib.pbkdf2_hmac('sha256', p.encode(), salt, 1000)\n";
        let md5 = "import hashlib\n\ndef h(p):\n    return hashlib.md5(p.encode()).hexdigest()\n";
        let r = report("users/store.py", old, md5);
        assert!(has(&r, RiskKind::WeakCrypto), "{r:?}");
        let calm = "import hashlib, os\n\ndef h(p):\n    salt = os.urandom(16)\n    return hashlib.pbkdf2_hmac('sha256', p.encode(), salt, 2000)\n";
        assert!(!has(
            &report("users/store.py", old, calm),
            RiskKind::WeakCrypto
        ));
    }

    #[test]
    fn secret_comparison_ignores_length_checks() {
        let old = "fn check(token: &[u8], expected: &[u8]) -> bool {\n    if token.len() != expected.len() {\n        return false;\n    }\n    ct_eq(token, expected)\n}\n";
        let new = "fn check(token: &[u8], expected: &[u8]) -> bool {\n    token == expected\n}\n";
        let r = report("src/guard.rs", old, new);
        assert!(has(&r, RiskKind::SecretCompare), "{r:?}");
        let same = "fn check(token: &[u8], expected: &[u8]) -> bool {\n    if token.len() == 0 {\n        return false;\n    }\n    ct_eq(token, expected)\n}\n";
        assert!(!has(
            &report("src/guard.rs", old, same),
            RiskKind::SecretCompare
        ));
    }

    #[test]
    fn migrations_destructive_or_alter() {
        let drop = diff_of(
            "db/migrations/0042_cleanup.sql",
            None,
            Some("DROP TABLE legacy_accounts;\n"),
        );
        let r = assess(&drop, &LanguageRegistry::new());
        assert!(has(&r, RiskKind::DestructiveMigration), "{r:?}");
        let ev = reason(&r, RiskKind::DestructiveMigration)
            .evidence
            .clone()
            .expect("evidence");
        assert_eq!((ev.side, ev.lines), (EvidenceSide::New, 0..1));
        let delete = diff_of(
            "db/migrations/0107.sql",
            None,
            Some("DELETE FROM sessions;\n"),
        );
        assert!(has(
            &assess(&delete, &LanguageRegistry::new()),
            RiskKind::DestructiveMigration
        ));
        let alter = diff_of(
            "db/migrations/0108.sql",
            None,
            Some("ALTER TABLE users ALTER COLUMN email SET NOT NULL;\n"),
        );
        let a = assess(&alter, &LanguageRegistry::new());
        let d = reason(&a, RiskKind::DestructiveMigration).weight;
        assert!(
            d < reason(&r, RiskKind::DestructiveMigration).weight,
            "alter is weaker than drop"
        );
        let benign = diff_of(
            "db/migrations/0109.sql",
            None,
            Some("CREATE INDEX idx_users_email ON users (email);\n"),
        );
        assert!(!has(
            &assess(&benign, &LanguageRegistry::new()),
            RiskKind::DestructiveMigration
        ));
        let alembic = "def upgrade():\n    op.drop_column('orders', 'total')\n";
        let a = assess(
            &diff_of("migrations/versions/9f_drop.py", None, Some(alembic)),
            &LanguageRegistry::new(),
        );
        assert!(has(&a, RiskKind::DestructiveMigration), "{a:?}");
    }

    #[test]
    fn concurrency_signals() {
        let flag_old = "use std::sync::atomic::{AtomicBool, Ordering};\nfn publish(f: &AtomicBool) {\n    f.store(true, Ordering::Release);\n}\n";
        let flag_new = flag_old.replace("Ordering::Release", "Ordering::Relaxed");
        let r = report("src/flag.rs", flag_old, &flag_new);
        assert!(has(&r, RiskKind::ConcurrencyWeakened), "{r:?}");
        assert!(
            has(&r, RiskKind::ConcurrencyRemoved),
            "Release removed: {r:?}"
        );
        let go_old = "func (r *R) Set(k string, v int) {\n\tr.mu.Lock()\n\tdefer r.mu.Unlock()\n\tr.m[k] = v\n}\n";
        let go_new = "func (r *R) Set(k string, v int) {\n\tr.m[k] = v\n}\n";
        let r = report("registry/r.go", go_old, go_new);
        let c = reason(&r, RiskKind::ConcurrencyRemoved);
        assert_eq!(c.evidence.as_ref().map(|e| e.side), Some(EvidenceSide::Old));
        let ts_old = "async function f(db) {\n  await db.insert(1);\n  await db.commit();\n}\n";
        let ts_new = "async function f(db) {\n  db.insert(1);\n  db.commit();\n}\n";
        assert!(has(
            &report("src/f.ts", ts_old, ts_new),
            RiskKind::ConcurrencyRemoved
        ));
        let spawn = "fn go() {\n    std::thread::spawn(|| work());\n}\n";
        assert!(has(
            &report("src/t.rs", "fn go() {}\n", spawn),
            RiskKind::ConcurrencyAdded
        ));
        // Moving code around does not remove anything.
        let moved_old = "func a() {\n\tmu.Lock()\n\tmu.Unlock()\n}\n\nfunc b() {}\n";
        let moved_new = "func b() {}\n\nfunc a() {\n\tmu.Lock()\n\tmu.Unlock()\n}\n";
        assert!(!has(
            &report("x.go", moved_old, moved_new),
            RiskKind::ConcurrencyRemoved
        ));
    }

    #[test]
    fn dependency_manifest_signals() {
        let pkg = "{\n  \"name\": \"web\",\n  \"scripts\": {\n    \"test\": \"jest\"\n  },\n  \"dependencies\": {\n    \"zod\": \"^3.23.8\"\n  }\n}\n";
        let with_script = pkg.replace(
            "\"test\": \"jest\"",
            "\"test\": \"jest\",\n    \"postinstall\": \"node setup.js\"",
        );
        let r = report("package.json", pkg, &with_script);
        let s = reason(&r, RiskKind::DependencyRisk);
        assert!(s.message.contains("install-time script"), "{r:?}");
        assert!(evidence_text(s, pkg, &with_script).contains("postinstall"));
        let with_dep = pkg.replace(
            "\"zod\": \"^3.23.8\"",
            "\"zod\": \"^3.23.8\",\n    \"left-padder\": \"^1.0.0\"",
        );
        let r = report("package.json", pkg, &with_dep);
        assert!(
            reason(&r, RiskKind::DependencyRisk)
                .message
                .contains("left-padder"),
            "{r:?}"
        );
        let bump = pkg.replace("^3.23.8", "^3.24.0");
        assert!(
            !has(
                &report("package.json", pkg, &bump),
                RiskKind::DependencyRisk
            ),
            "a version bump is not a new dependency"
        );

        let cargo =
            "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\nreqwest = \"0.12\"\n";
        let git = cargo.replace(
            "reqwest = \"0.12\"",
            "reqwest = { git = \"https://example.invalid/fork\" }",
        );
        let r = report("Cargo.toml", cargo, &git);
        assert!(
            reason(&r, RiskKind::DependencyRisk).message.contains("git"),
            "{r:?}"
        );
        let new_dep = format!("{cargo}tokio = \"1\"\n");
        assert!(reason(
            &report("Cargo.toml", cargo, &new_dep),
            RiskKind::DependencyRisk
        )
        .message
        .contains("tokio"));
        // `[package]` keys are not dependencies.
        let renamed = cargo.replace("name = \"x\"", "name = \"y\"");
        assert!(!has(
            &report("Cargo.toml", cargo, &renamed),
            RiskKind::DependencyRisk
        ));

        let reqs = "flask==3.0.3\nrequests==2.32.3\n";
        let loose = "flask\nrequests==2.32.3\n";
        let r = report("requirements.txt", reqs, loose);
        assert!(
            reason(&r, RiskKind::DependencyRisk)
                .message
                .contains("flask"),
            "{r:?}"
        );
        assert!(
            !r.reasons.iter().any(|x| x.kind == RiskKind::Docs),
            "requirements.txt is not prose"
        );

        let gomod = "module x\n\nrequire github.com/lib/pq v1.10.9\n";
        let replace =
            format!("{gomod}\nreplace github.com/lib/pq => example.invalid/fork/pq v1.0.0\n");
        assert!(has(
            &report("go.mod", gomod, &replace),
            RiskKind::DependencyRisk
        ));
    }

    #[test]
    fn ci_and_container_signals() {
        let ci =
            "on:\n  pull_request:\njobs:\n  t:\n    steps:\n      - run: cargo test --workspace\n";
        let weak =
            "on:\n  pull_request_target:\njobs:\n  t:\n    steps:\n      - run: echo skipped\n";
        let r = report(".github/workflows/ci.yml", ci, weak);
        let kinds: Vec<&str> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::CiWeakened)
            .map(|x| x.message.as_str())
            .collect();
        assert_eq!(kinds.len(), 2, "{r:?}");
        assert!(kinds.iter().any(|m| m.contains("pull_request_target")));
        assert!(kinds.iter().any(|m| m.contains("Removes a test")));
        let docker = "FROM node:20\nUSER node\nCMD [\"node\", \"a.js\"]\n";
        let root = docker.replace("USER node", "ADD https://example.invalid/t.tgz /opt/");
        let r = report("Dockerfile", docker, &root);
        assert!(has(&r, RiskKind::ContainerWeakened), "{r:?}");
        let calm = docker.replace("node:20", "node:22");
        assert!(!has(
            &report("Dockerfile", docker, &calm),
            RiskKind::ContainerWeakened
        ));
    }

    #[test]
    fn disabled_tests_are_flagged_in_test_files() {
        let old = "import pytest\n\ndef test_a():\n    assert f() == 1\n";
        let new = "import pytest\n\n@pytest.mark.skip(reason='flaky')\ndef test_a():\n    assert f() == 1\n";
        let r = report("tests/test_a.py", old, new);
        let d = reason(&r, RiskKind::TestsDisabled);
        assert_eq!(d.line, Some(2));
        assert!(has(&r, RiskKind::TestFile));
        let jest_old = "it('a', () => {});\n";
        assert!(has(
            &report("a.test.ts", jest_old, "it.skip('a', () => {});\n"),
            RiskKind::TestsDisabled
        ));
        assert!(has(
            &report(
                "a_test.go",
                "func TestA(t *testing.T) {\n}\n",
                "func TestA(t *testing.T) {\n\tt.Skip(\"flaky\")\n}\n"
            ),
            RiskKind::TestsDisabled
        ));
        assert!(!has(
            &report("tests/test_a.py", old, old.replace("== 1", "== 2").as_str()),
            RiskKind::TestsDisabled
        ));
    }

    #[test]
    fn flipped_defaults_weigh_by_key_polarity() {
        let yaml = "server:\n  debug: false\n  tls_verify: true\n  colour: true\n";
        let flipped = "server:\n  debug: true\n  tls_verify: false\n  colour: false\n";
        let r = report("config/prod.yaml", yaml, flipped);
        let flips: Vec<&RiskReason> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::ConfigDefaultFlipped)
            .collect();
        assert_eq!(flips.len(), 3, "{r:?}");
        let by = |k: &str| {
            flips
                .iter()
                .find(|f| f.message.contains(k))
                .expect(k)
                .weight
        };
        assert!(by("debug") > by("colour") && by("tls_verify") > by("colour"));
        // The safe direction counts less than a neutral flip.
        let safe = report("config/prod.yaml", flipped, yaml);
        let w = |r: &RiskReport, k: &str| {
            r.reasons
                .iter()
                .find(|x| x.kind == RiskKind::ConfigDefaultFlipped && x.message.contains(k))
                .map(|x| x.weight)
        };
        assert!(w(&safe, "debug").is_some_and(|d| d < by("colour")));
        let code_old = "pub const DEFAULT_STRICT: bool = true;\n";
        let code_new = "pub const DEFAULT_STRICT: bool = false;\n";
        let r = report("src/options.rs", code_old, code_new);
        assert!(
            reason(&r, RiskKind::ConfigDefaultFlipped)
                .message
                .contains("DEFAULT_STRICT"),
            "{r:?}"
        );
        // Not a flip: other text changed too.
        assert!(!has(
            &report("c.yaml", "a: true\n", "b: false\n"),
            RiskKind::ConfigDefaultFlipped
        ));
    }

    #[test]
    fn shell_command_built_from_strings() {
        let old = "use std::process::Command;\nfn run(a: &[String]) {\n    Command::new(\"tool\").args(a).output().ok();\n}\n";
        let new = "use std::process::Command;\nfn run(a: &[String]) {\n    Command::new(\"sh\").arg(\"-c\").arg(a.join(\" \")).output().ok();\n}\n";
        assert!(
            reason(&report("src/r.rs", old, new), RiskKind::DynamicExecution)
                .message
                .contains("shell")
        );
        let ts_old = "import { execFile } from 'child_process';\nexecFile('convert', [n]);\n";
        let ts_new = "import { exec } from 'child_process';\nexec(`convert ${n}`);\n";
        assert!(has(
            &report("t.ts", ts_old, ts_new),
            RiskKind::DynamicExecution
        ));
    }

    #[test]
    fn sql_placeholders_are_not_string_building() {
        let old =
            "def f(cur, n):\n    cur.execute(\"SELECT id FROM users WHERE name = %s\", (n,))\n";
        let fstr =
            "def f(cur, n):\n    cur.execute(f\"SELECT id FROM users WHERE name = '{n}'\")\n";
        assert!(has(
            &report("db/q.py", old, fstr),
            RiskKind::SqlStringBuilding
        ));
        let same = old.replace("id", "id, name");
        assert!(!has(
            &report("db/q.py", old, &same),
            RiskKind::SqlStringBuilding
        ));
    }

    #[test]
    fn go_error_checks_with_init_statements_count() {
        let old = "func F(f File) error {\n\tif err := f.Sync(); err != nil {\n\t\treturn err\n\t}\n\treturn nil\n}\n";
        let new = "func F(f File) error {\n\tf.Sync()\n\treturn nil\n}\n";
        assert!(has(
            &report("io/f.go", old, new),
            RiskKind::ErrorHandlingRemoved
        ));
    }

    #[test]
    fn added_public_api_is_one_reason() {
        let old = "pub fn base() {}\n";
        let new = "pub fn base() {}\npub fn a() {}\npub fn b() {}\npub fn c() {}\npub fn d() {}\n";
        let r = report("src/lib.rs", old, new);
        let added: Vec<_> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiAdded)
            .collect();
        assert_eq!(added.len(), 1, "{r:?}");
        assert!(
            added[0].message.contains("and 1 more"),
            "{}",
            added[0].message
        );
        let one = report("src/lib.rs", old, "pub fn base() {}\npub fn a() {}\n");
        assert!(one.score < r.score, "more additions, a little more risk");
        assert!(r.score < 0.25, "additions alone stay low: {}", r.score);
    }

    #[test]
    fn signatures_in_c_php_and_ruby() {
        let c_old = "int parse(const char *s) {\n    return 0;\n}\n\nstatic int helper(int x) {\n    return x;\n}\n";
        let c_new = "int parse(const char *s, int flags) {\n    return 0;\n}\n\nstatic int helper(int x, int y) {\n    return x;\n}\n";
        let r = report("src/parse.c", c_old, c_new);
        let changed: Vec<_> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .collect();
        assert_eq!(changed.len(), 1, "static helper is not API: {r:?}");
        assert!(changed[0].message.contains("parse"));
        let php_old = "<?php\nclass Repo {\n    public function find($id) { return $id; }\n    private function key($id) { return $id; }\n}\n";
        let php_new = php_old
            .replace("find($id)", "find($id, $strict)")
            .replace("key($id)", "key($id, $x)");
        let r = report("src/Repo.php", php_old, &php_new);
        let changed: Vec<_> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .collect();
        assert_eq!(changed.len(), 1, "{r:?}");
        assert!(changed[0].message.contains("find"));
        let rb_old =
            "class Repo\n  def find(id)\n    id\n  end\n\n  def _key(id)\n    id\n  end\nend\n";
        let rb_new = rb_old
            .replace("def find(id)", "def find(id, strict)")
            .replace("def _key(id)", "def _key(id, x)");
        let r = report("lib/repo.rb", rb_old, &rb_new);
        let changed: Vec<_> = r
            .reasons
            .iter()
            .filter(|x| x.kind == RiskKind::PublicApiChanged)
            .collect();
        assert_eq!(changed.len(), 1, "{r:?}");
        // A body-only change of a Ruby method is not a signature change.
        let body = rb_old.replace(
            "    id\n  end\n\n  def _key",
            "    id.to_s\n  end\n\n  def _key",
        );
        assert!(!has(
            &report("lib/repo.rb", rb_old, &body),
            RiskKind::PublicApiChanged
        ));
    }

    #[test]
    fn api_evidence_covers_old_and_new_items() {
        let old = "pub fn keep() {}\npub fn gone(a: u32) {}\n";
        let new = "pub fn keep() {}\n";
        let r = report("src/lib.rs", old, new);
        let g = reason(&r, RiskKind::PublicApiRemoved);
        let ev = g.evidence.as_ref().expect("evidence");
        assert_eq!((ev.side, ev.lines.clone()), (EvidenceSide::Old, 1..2));
        let changed = report(
            "src/lib.rs",
            "pub fn f(a: u32) {}\n",
            "pub fn f(a: u64) {}\n",
        );
        assert_eq!(
            reason(&changed, RiskKind::PublicApiChanged)
                .evidence
                .as_ref()
                .map(|e| (e.side, e.lines.clone())),
            Some((EvidenceSide::New, 0..1))
        );
    }

    #[test]
    fn facts_score_like_assess_and_follow_the_weights() {
        let langs = LanguageRegistry::new();
        let d = diff_of(
            "src/a.rs",
            Some("fn f() -> Result<(), E> {\n    g()?;\n    Ok(())\n}\n"),
            Some("fn f() -> Result<(), E> {\n    let _ = g();\n    Ok(())\n}\n"),
        );
        let facts = collect_facts(&d, &langs, None);
        let w = RiskWeights::default();
        assert_eq!(facts.score(&w), assess(&d, &langs));
        assert_eq!(assess_with(&d, &langs, &w), assess(&d, &langs));
        let mut stronger = w.clone();
        stronger.error_handling = 0.7;
        assert!(facts.score(&stronger).score > facts.score(&w).score);
        let mut off = w.clone();
        off.error_handling = 0.0;
        off.conditionals = 0.0;
        assert_eq!(facts.score(&off).score, 0.0);
        assert_eq!(facts.churn(), 2);
    }

    #[test]
    fn rank_facts_agrees_with_rank_files() {
        let langs = LanguageRegistry::new();
        let files = vec![
            diff_of("README.md", Some("a\n"), Some("b\n")),
            diff_of(
                "src/a.rs",
                Some("pub fn f() {}\n"),
                Some("pub fn f(x: u8) {}\n"),
            ),
            diff_of("src/b.rs", Some("fn g() { }\n"), Some("fn g() { 1; }\n")),
        ];
        let tests = Some(tests_changed_in(&files));
        let facts: Vec<RiskFacts> = files
            .iter()
            .map(|f| collect_facts(f, &langs, tests))
            .collect();
        let a: Vec<usize> = rank_facts(&facts, &RiskWeights::default())
            .iter()
            .map(|(i, _)| *i)
            .collect();
        let b: Vec<usize> = rank_files_with(&files, &langs)
            .iter()
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(a, b);
        assert_eq!(a[0], 1);
    }

    #[test]
    fn low_value_files_are_generated_lock_or_negligible() {
        let lock = report("Cargo.lock", "a = 1\n", "a = 2\n");
        assert!(lock.is_low_value());
        let calm = report("src/a.rs", "fn a() {}\n", "fn a() { }\n");
        assert!(calm.is_low_value());
        let api = report("src/lib.rs", "pub fn f() {}\n", "pub fn f(x: u8) {}\n");
        assert!(!api.is_low_value());
    }

    #[test]
    fn evidence_ranges_stay_inside_the_text() {
        // Every reason with evidence points at existing lines of the right side.
        let old = "pub fn a(x: u32) -> u32 {\n    let v = parse(x)?;\n    if v > 1 { return v; }\n    v\n}\n";
        let new = "pub fn a(x: u64) -> u32 {\n    let v = parse(x).unwrap();\n    v\n}\n";
        let r = report("src/lib.rs", old, new);
        for reason in &r.reasons {
            if let Some(ev) = &reason.evidence {
                let n = if ev.side == EvidenceSide::Old {
                    old
                } else {
                    new
                }
                .lines()
                .count() as u32;
                assert!(
                    ev.lines.start < ev.lines.end && ev.lines.end <= n,
                    "{reason:?}"
                );
                assert_eq!(ev.path, "src/lib.rs");
            }
        }
        assert!(
            r.reasons.iter().filter(|x| x.evidence.is_some()).count() >= 3,
            "{r:?}"
        );
    }
}
