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
use std::sync::OnceLock;

use lector_diff::LineDiff;
use lector_git::ChangeKind;
use lector_syntax::{Language, LanguageRegistry, OutlineItem, SymbolKind, Syntax};
use lector_text::Text;
use regex::Regex;

use crate::review::{DiffKind, DiffSide, FileDiff};

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

/// Assesses one file diff.
pub fn assess(diff: &FileDiff, langs: &LanguageRegistry) -> RiskReport {
    assess_in(diff, langs, Context::default())
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
    let tests_changed = files.iter().any(|f| {
        let f: &FileDiff = f.borrow();
        let p = path_of(f);
        is_test_path(&p) && !f.is_identical()
    });
    let ctx = Context {
        tests_changed: Some(tests_changed),
    };
    let mut out: Vec<(usize, RiskReport, u32)> = files
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let f: &FileDiff = f.borrow();
            let s = f.stats();
            (i, assess_in(f, langs, ctx), s.added + s.removed)
        })
        .collect();
    out.sort_by(|a, b| {
        b.1.score
            .total_cmp(&a.1.score)
            .then(b.2.cmp(&a.2))
            .then(a.0.cmp(&b.0))
    });
    out.into_iter().map(|(i, r, _)| (i, r)).collect()
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

fn security_path_reason(path: &str) -> Option<(f32, String)> {
    let lower = path.to_ascii_lowercase();
    let name = file_name(&lower);
    if lower.contains(".github/workflows/") || lower.contains(".gitlab-ci") || name == "jenkinsfile"
    {
        return Some((
            0.35,
            "CI/CD pipeline definition (runs with repository secrets)".into(),
        ));
    }
    if name.starts_with("dockerfile")
        || name.starts_with("docker-compose")
        || name.ends_with(".dockerfile")
    {
        return Some((0.3, "container build/runtime definition".into()));
    }
    if lower
        .split('/')
        .any(|s| matches!(s, "migrations" | "migrate" | "migration"))
    {
        return Some((0.3, "database migration".into()));
    }
    if name == ".env"
        || name.starts_with(".env.")
        || name.ends_with(".pem")
        || name.ends_with(".key")
    {
        return Some((0.4, "secrets/credentials file".into()));
    }
    if name.ends_with(".tf")
        || lower
            .split('/')
            .any(|s| matches!(s, "terraform" | "k8s" | "kubernetes" | "helm" | "ansible"))
    {
        return Some((0.3, "infrastructure definition".into()));
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
        0.3,
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
    weight: f32,
    what: &'static str,
}

fn patterns() -> &'static Patterns {
    static P: OnceLock<Patterns> = OnceLock::new();
    P.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static pattern");
        let d = |s: &str, families: &'static [Family], kind, weight, what| Danger {
            re: re(s),
            families,
            kind,
            weight,
            what,
        };
        use Family::*;
        Patterns {
            cond: re(r"\b(if|else|elif|match|switch|case|while|guard|unless|when)\b"),
            err_rust: re(r"(?:\w|\)|\]|>)\?\s*(?:[;.,)}\]]|$)|\.map_err\(|\.ok_or(?:_else)?\(|\.context\(|\.with_context\(|\bErr\("),
            err_script: re(r"\bcatch\b|\.catch\(|\bfinally\b|\bthrow\b|\btry\s*\{"),
            err_python: re(r"\bexcept\b|\bfinally\b|\braise\b|\btry\s*:"),
            err_go: re(r"if\s+err\s*!=\s*nil|errors\.(?:Is|As|New)\b|fmt\.Errorf|\brecover\(\)"),
            err_jvm: re(r"\bcatch\b|\bfinally\b|\bthrows?\b|\btry\b|\bguard\b"),
            err_generic: re(r"\bcatch\b|\bfinally\b|\brescue\b|\bensure\b|\braise\b|\bthrow\b|\berrno\b|\bperror\b|set\s+-e|\|\|\s*exit|\btrap\b"),
            panic_rust: re(r"\.unwrap\(\)|\.expect\(|\bpanic!\(|\bunreachable!\(|\btodo!\(|\bunimplemented!\("),
            panic_go: re(r"\bpanic\("),
            assertion: re(r"\bassert\w*\s*[!(]|\bassert\s|\bexpect\(|\.should\b|\brequire\("),
            test_decl: re(r"#\[test\]|#\[tokio::test\]|\bdef test_|\bfunc Test|\b(?:it|test)\(|@Test\b|\bfn test_"),
            sql_stmt: re(r"(?i)\b(?:select\s.+\sfrom|insert\s+into|update\s+\w+\s+set|delete\s+from)\b"),
            sql_dynamic: re(r#"\+\s*\w|"\s*\+|\bformat!?\(|\bf"|\bf'|%s|\$\{|\.format\(|\{\w*\}|\bSprintf\("#),
            danger: vec![
                d(r"\bunsafe\b", &[Rust, Jvm], RiskKind::UnsafeCode, 0.45, "`unsafe` code"),
                d(r#"\bunsafe\.|"unsafe""#, &[Go], RiskKind::UnsafeCode, 0.45, "`unsafe` package use"),
                d(r"\beval\s*\(|\bnew Function\(|\b__import__\(|\bexec\s*\(\s*[^)]", &[Script, Python, Dynamic], RiskKind::DynamicExecution, 0.45, "dynamic code evaluation (`eval`/`exec`)"),
                d(r"child_process|\bexecSync\(|\bspawnSync\(|\bspawn\(|\bexecFile\(", &[Script], RiskKind::DynamicExecution, 0.4, "process execution"),
                d(r"\bsubprocess\.|\bos\.system\(|\bos\.popen\(|shell\s*=\s*True", &[Python], RiskKind::DynamicExecution, 0.4, "subprocess/shell execution"),
                d(r"\bpickle\.loads?\(|\byaml\.load\(|\bmarshal\.loads?\(", &[Python], RiskKind::DynamicExecution, 0.4, "unsafe deserialization"),
                d(r"Command::new|std::process::Command|\bprocess::Command", &[Rust], RiskKind::DynamicExecution, 0.4, "process execution"),
                d(r#"\bexec\.Command\(|"os/exec"|\bsyscall\.Exec"#, &[Go], RiskKind::DynamicExecution, 0.4, "process execution"),
                d(r"Runtime\.getRuntime\(\)\.exec|\bProcessBuilder\b|\bProcess\.Start\(|Class\.forName\(|ObjectInputStream", &[Jvm], RiskKind::DynamicExecution, 0.4, "process execution / reflection / deserialization"),
                d(r"\b(?:shell_exec|passthru|popen|proc_open|system|exec)\s*\(|%x\{|\bunserialize\(", &[Dynamic], RiskKind::DynamicExecution, 0.4, "process execution"),
                d(r"\bsystem\s*\(|\bpopen\s*\(|\bstrcpy\s*\(|\bsprintf\s*\(|\bgets\s*\(", &[CLike], RiskKind::DynamicExecution, 0.4, "shell execution / unbounded string function"),
                d(r"\beval\b|curl[^|]*\|\s*(?:ba)?sh|\bsudo\b|\brm\s+-rf\b|\bchmod\s+(?:-R\s+)?777", &[Shell], RiskKind::DynamicExecution, 0.4, "shell eval/sudo/destructive command"),
                d(r"\binnerHTML\b|dangerouslySetInnerHTML|document\.write\(", &[Script], RiskKind::DynamicExecution, 0.3, "raw HTML injection"),
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
    raise: Vec<RiskReason>,
    damp: Vec<RiskReason>,
}

impl Builder {
    fn add(&mut self, kind: RiskKind, weight: f32, message: String, line: Option<u32>) {
        let r = RiskReason {
            kind,
            weight: weight.clamp(0.0, 0.95),
            message,
            line,
        };
        if kind.is_dampening() {
            self.damp.push(r);
        } else {
            self.raise.push(r);
        }
    }

    fn finish(mut self) -> RiskReport {
        let cmp = |a: &RiskReason, b: &RiskReason| {
            b.weight
                .total_cmp(&a.weight)
                .then(a.kind.cmp(&b.kind))
                .then(a.message.cmp(&b.message))
        };
        self.raise.sort_by(cmp);
        self.damp.sort_by(cmp);
        let raised = 1.0 - self.raise.iter().fold(1.0f32, |p, r| p * (1.0 - r.weight));
        let damped = self.damp.iter().fold(1.0f32, |p, r| p * (1.0 - r.weight));
        let score = ((raised * damped).clamp(0.0, 1.0) * 1000.0).round() / 1000.0;
        let mut reasons = self.raise;
        reasons.extend(self.damp);
        RiskReport { score, reasons }
    }
}

fn side_text(side: &DiffSide) -> String {
    match (&side.text, side.exists) {
        (Some(t), true) => t.to_string(),
        _ => String::new(),
    }
}

fn pick_lines(text: &str, ranges: impl Iterator<Item = std::ops::Range<u32>>) -> Vec<&str> {
    let all: Vec<&str> = text.split('\n').collect();
    let mut out = Vec::new();
    for r in ranges {
        for n in r {
            if let Some(l) = all.get(n as usize) {
                out.push(l.strip_suffix('\r').unwrap_or(l));
            }
        }
    }
    out
}

fn assess_in(diff: &FileDiff, langs: &LanguageRegistry, ctx: Context) -> RiskReport {
    let mut b = Builder {
        raise: Vec::new(),
        damp: Vec::new(),
    };
    let path = path_of(diff);
    let is_test = is_test_path(&path);

    match diff.kind {
        DiffKind::Binary => b.add(
            RiskKind::BinaryChange,
            0.2,
            "Binary file changed (cannot be reviewed as text)".into(),
            None,
        ),
        DiffKind::TooLarge => b.add(
            RiskKind::TooLarge,
            0.25,
            "Too large to analyze; review manually".into(),
            None,
        ),
        DiffKind::Image(_) => {}
        DiffKind::Text => {}
    }
    if let Some((w, why)) = security_path_reason(&path) {
        if !is_docs_path(&path) {
            let w = if is_test { w * 0.5 } else { w };
            b.add(
                RiskKind::SecuritySensitivePath,
                w,
                format!("Security-sensitive file: {why}"),
                None,
            );
        }
    }
    if diff.kind != DiffKind::Text {
        return b.finish();
    }

    let old_src = side_text(&diff.old);
    let new_src = side_text(&diff.new);
    let ld: &LineDiff = &diff.diff;
    let stats = ld.stats();
    let churn = stats.added + stats.removed;
    let deleted_file = !diff.new.exists || diff.change.kind == ChangeKind::Deleted;

    // Language (new side preferred; a deleted file only has the old one).
    let language: Option<std::sync::Arc<Language>> = diff
        .new
        .language
        .clone()
        .filter(|_| diff.new.exists)
        .or_else(|| diff.old.language.clone())
        .or_else(|| langs.language_for_path(std::path::Path::new(&path), new_src.lines().next()));
    let family = family_of(language.as_deref());

    // Dampening classes.
    let generated = generated_reason(&path, &old_src, &new_src);
    let lock = is_lockfile(&path);
    if let Some(why) = generated {
        b.add(
            RiskKind::Generated,
            0.7,
            format!("Generated file ({why}): little review value"),
            None,
        );
    }
    if lock {
        b.add(
            RiskKind::Lockfile,
            0.1,
            "Lockfile changed (dependency versions)".into(),
            None,
        );
        b.add(
            RiskKind::Generated,
            0.5,
            "Lockfiles are machine-written: contents damped".into(),
            None,
        );
    }
    if is_test {
        b.add(RiskKind::TestFile, 0.35, "Test file".into(), None);
    }
    if is_docs_path(&path) {
        b.add(RiskKind::Docs, 0.5, "Documentation".into(), None);
    }
    let low_value = generated.is_some() || lock;

    // Manifests and config.
    if is_manifest(&path) && !lock {
        b.add(
            RiskKind::DependencyManifest,
            0.2,
            format!(
                "`{}` changed: dependencies or build configuration",
                file_name(&path)
            ),
            None,
        );
    } else if is_config_ext(&path) && !lock && generated.is_none() {
        b.add(
            RiskKind::Config,
            0.1,
            "Configuration file changed".into(),
            None,
        );
    }

    // Churn.
    if !low_value {
        let w = match churn {
            0..=39 => 0.0,
            40..=149 => 0.1,
            150..=399 => 0.2,
            400..=999 => 0.3,
            _ => 0.35,
        };
        if w > 0.0 {
            b.add(
                RiskKind::LargeChurn,
                w,
                format!(
                    "Large change: {} lines added, {} removed",
                    stats.added, stats.removed
                ),
                None,
            );
        }
    }

    // Deleted files.
    if deleted_file && stats.removed > 0 {
        if is_test {
            b.add(
                RiskKind::TestsRemoved,
                0.3,
                format!("Test file deleted ({} lines)", stats.removed),
                None,
            );
        } else if family.is_code() && stats.removed >= 20 && !low_value {
            b.add(
                RiskKind::DeletedFile,
                0.15,
                format!("Source file deleted ({} lines)", stats.removed),
                None,
            );
        }
    }

    // Line-level signals need code.
    if family.is_code() && !low_value {
        let removed_all = pick_lines(&old_src, ld.hunks.iter().map(|h| h.old.clone()));
        let added_rows: Vec<(u32, &str)> = {
            let all: Vec<&str> = new_src.split('\n').collect();
            ld.hunks
                .iter()
                .flat_map(|h| h.new.clone())
                .filter_map(|n| {
                    all.get(n as usize)
                        .map(|l| (n, l.strip_suffix('\r').unwrap_or(l)))
                })
                .collect()
        };
        let prefixes = family.comment_prefixes();
        let removed: Vec<&str> = removed_all
            .into_iter()
            .filter(|l| !is_comment(l, prefixes))
            .collect();

        // Rows of the new text that are test code (`mod tests`) are skipped
        // for added-code signals.
        let test_rows = if family == Family::Rust && !is_test {
            rust_test_rows(diff)
        } else {
            Vec::new()
        };
        let added: Vec<&str> = added_rows
            .iter()
            .filter(|(n, l)| !is_comment(l, prefixes) && !test_rows.iter().any(|r| r.contains(n)))
            .map(|(_, l)| *l)
            .collect();
        let first_row = |re: &Regex| {
            added_rows
                .iter()
                .find(|(_, l)| re.is_match(l))
                .map(|(n, _)| *n)
        };

        let p = patterns();

        if !is_test {
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
                let net = -net_added(re, &removed, &added);
                if net >= 1 {
                    let w = 0.28 + 0.07 * ((net - 1).min(4) as f32);
                    b.add(RiskKind::ErrorHandlingRemoved, w, format!("Removes error handling ({} fewer error paths: `?`/catch/except/err checks)", net), None);
                }
            }
            // Conditionals removed.
            if family != Family::Sql {
                let net = -net_added(&p.cond, &removed, &added);
                if net >= 1 {
                    let w = 0.12 + 0.04 * (net.min(5) as f32);
                    b.add(
                        RiskKind::ConditionalsRemoved,
                        w,
                        format!("Deletes {} conditional branch(es) (if/match/switch)", net),
                        None,
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
                let net = net_added(re, &removed, &added);
                if net >= 1 {
                    let w = 0.2 + 0.05 * ((net - 1).min(4) as f32);
                    b.add(
                        RiskKind::PanicAdded,
                        w,
                        format!(
                            "Adds {} unwrap()/expect()/panic! call(s) that can crash at runtime",
                            net
                        ),
                        first_row(re),
                    );
                }
            }
            // Dangerous constructs.
            for d in &p.danger {
                if !d.families.contains(&family) {
                    continue;
                }
                let net = net_added(&d.re, &removed, &added);
                if net >= 1 {
                    b.add(
                        d.kind,
                        d.weight,
                        format!("Adds {}", d.what),
                        first_row(&d.re),
                    );
                }
            }
            // SQL built with string formatting.
            let sql_net = |lines: &[&str]| {
                lines
                    .iter()
                    .filter(|l| p.sql_stmt.is_match(l) && p.sql_dynamic.is_match(l))
                    .count() as isize
            };
            if family != Family::Sql && sql_net(&added) - sql_net(&removed) >= 1 {
                let row = added_rows
                    .iter()
                    .find(|(_, l)| p.sql_stmt.is_match(l) && p.sql_dynamic.is_match(l))
                    .map(|(n, _)| *n);
                b.add(
                    RiskKind::SqlStringBuilding,
                    0.4,
                    "Builds SQL by string formatting (injection risk)".into(),
                    row,
                );
            }
            // Assertions removed from source.
            let net = -net_added(&p.assertion, &removed, &added);
            if net >= 2 {
                b.add(
                    RiskKind::TestsRemoved,
                    0.2,
                    format!("Removes {net} assertions from non-test code"),
                    None,
                );
            }
        } else {
            // Tests/assertions removed.
            let tests_net = -net_added(&p.test_decl, &removed, &added);
            let asserts_net = -net_added(&p.assertion, &removed, &added);
            if tests_net >= 1 {
                b.add(
                    RiskKind::TestsRemoved,
                    0.3 + 0.05 * ((tests_net - 1).min(4) as f32),
                    format!("Removes {tests_net} test(s)"),
                    None,
                );
            } else if asserts_net >= 2 {
                b.add(
                    RiskKind::TestsRemoved,
                    0.25,
                    format!("Removes {asserts_net} assertions from tests"),
                    None,
                );
            }
        }

        // Source changed, no test changed anywhere in the changeset.
        if !is_test && ctx.tests_changed == Some(false) && churn >= 15 && !is_docs_path(&path) {
            b.add(
                RiskKind::SourceWithoutTests,
                0.1,
                "Source changed but no test file changed in this changeset".into(),
                None,
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
            ) && !is_test
            {
                api_reasons(&mut b, lang, diff, &old_src, &new_src);
            }
        }
    }
    b.finish()
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
    row: u32,
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
    let c_style = name != "Python";
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
        let hl = header_len(item_src, python);
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
            row: text.offset_to_point(item.name_range.start).row,
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
        b.add(
            RiskKind::PublicApiRemoved,
            0.5,
            format!("Removes public {} `{}`", i.label, i.display),
            None,
        );
    }
    for (o, n) in changed.iter().take(SHOWN) {
        b.add(
            RiskKind::PublicApiChanged,
            0.45,
            format!(
                "Changes the signature of public {} `{}`: `{}` -> `{}`",
                n.label,
                n.display,
                clip(&o.signature),
                clip(&n.signature)
            ),
            Some(n.row),
        );
    }
    for i in added.iter().take(SHOWN) {
        b.add(
            RiskKind::PublicApiAdded,
            0.12,
            format!("Adds public {} `{}`", i.label, i.display),
            Some(i.row),
        );
    }
    let extra = |total: usize| total.saturating_sub(SHOWN);
    if extra(removed.len()) > 0 {
        b.add(
            RiskKind::PublicApiRemoved,
            0.5,
            format!("... and {} more public items removed", extra(removed.len())),
            None,
        );
    }
    if extra(changed.len()) > 0 {
        b.add(
            RiskKind::PublicApiChanged,
            0.45,
            format!(
                "... and {} more public signatures changed",
                extra(changed.len())
            ),
            None,
        );
    }
    if extra(added.len()) > 0 {
        b.add(
            RiskKind::PublicApiAdded,
            0.12,
            format!("... and {} more public items added", extra(added.len())),
            None,
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
}
