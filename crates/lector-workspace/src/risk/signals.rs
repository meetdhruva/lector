//! Content signals of the risk model that look at the changed lines of a file
//! (authentication, crypto, migrations, concurrency, dependency manifests, CI,
//! containers, disabled tests, flipped defaults). Each [`Finding`] names the
//! rows it is based on so that the reason can point at its evidence.

use std::ops::Range;
use std::sync::OnceLock;

use regex::Regex;

use super::weights::RiskWeights;
use super::{Family, Param, RiskKind};

/// A changed line with its 0-based row (old text for removed, new text for added lines).
#[derive(Debug, Clone, Copy)]
pub(super) struct Row<'a> {
    pub n: u32,
    pub text: &'a str,
}

/// Which text a finding's rows refer to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum At {
    Old,
    New,
}

/// One detected signal.
#[derive(Debug, Clone)]
pub(super) struct Finding {
    pub kind: RiskKind,
    pub param: Param,
    pub magnitude: f32,
    pub message: String,
    pub at: At,
    pub rows: Range<u32>,
}

/// Everything a detector needs to know about the file.
pub(super) struct Ctx<'a> {
    pub path: &'a str,
    pub family: Family,
    pub is_test: bool,
    pub removed: &'a [Row<'a>],
    pub added: &'a [Row<'a>],
    /// Hunks as (old rows, new rows) to pair flipped lines.
    pub hunks: &'a [(Range<u32>, Range<u32>)],
    pub new_src: &'a str,
}

macro_rules! p {
    ($f:ident) => {
        (|w: &RiskWeights| w.$f) as Param
    };
}

struct Sigs {
    auth_removed: Regex,
    verification_off: Regex,
    weak_crypto: Regex,
    hardening: Regex,
    secret_literal: Regex,
    placeholder: Regex,
    secret_cmp: Regex,
    cmp_noise: Regex,
    destructive: Regex,
    delete_all: Regex,
    alter: Regex,
    sync_removed: Regex,
    conc_weak: Regex,
    conc_added: Regex,
    shell_interp: Regex,
    dep_script: Regex,
    dep_source: Regex,
    ci_unsafe: Regex,
    ci_check: Regex,
    docker_bad: Regex,
    docker_user: Regex,
    test_skip: Regex,
    boolean: Regex,
    key: Regex,
    json_dep: Regex,
    toml_dep: Regex,
    req_dep: Regex,
    gomod_dep: Regex,
}

fn sigs() -> &'static Sigs {
    static S: OnceLock<Sigs> = OnceLock::new();
    S.get_or_init(|| {
        let re = |s: &str| Regex::new(s).expect("static pattern");
        Sigs {
            auth_removed: re(r"(?i)\b(is_?admin|authorize\w*|authorise\w*|authenticat\w*|login_required|require_?(auth|login|role|permission)\w*|has_?(role|permission|perm)\w*|check_?(access|permission|auth)\w*|permission_required|PreAuthorize|forbidden|unauthori[sz]ed|ensure_?auth\w*|verify_?(token|user|signature|jwt)\w*)\b|\b(401|403)\b"),
            verification_off: re(r#"(?i)\bverify\s*=\s*False\b|InsecureSkipVerify\s*:\s*true|rejectUnauthorized\s*:\s*false|danger_accept_invalid_(certs|hostnames)\(\s*true|ssl\._create_unverified_context|NODE_TLS_REJECT_UNAUTHORIZED|\b(tls|ssl)_?verify\w*\s*[:=]\s*(false|0|no)\b|check_hostname\s*=\s*False|ParseUnverified|verify_signature["']?\s*[:=]\s*False|SSL_VERIFYPEER[^;\n]*(false|0)|\bverify_none\b|StrictHostKeyChecking\s+no"#),
            weak_crypto: re(r#"(?i)hashlib\.(md5|sha1)\b|createHash\(\s*["'](md5|sha1)["']|\bmd5::|\bMd5\b|\bSha1\b|sha1::|crypto/(md5|sha1|des|rc4)|\bDES_|AES/ECB|\bECB\b|\brc4\b|\bMessageDigest\.getInstance\(\s*"(MD5|SHA-?1)"|Math\.random\(\)"#),
            hardening: re(r"(?i)pbkdf2|bcrypt|scrypt|argon2|constant_?time|ct_eq|compare_digest|timingSafeEqual|ConstantTimeCompare|subtle::|hmac\.new|\bsalt\b|os\.urandom|OsRng|SecureRandom|crypto\.randomBytes"),
            secret_literal: re(r#"(?i)\b\w*(api_?key|secret|passw(or)?d|token|credential|private_?key)\w*\s*[:=]\s*["'][^"'\s]{8,}["']|sk_live_|AKIA[0-9A-Z]{16}|-----BEGIN (RSA |EC |OPENSSH )?PRIVATE KEY"#),
            placeholder: re(r#"(?i)example|changeme|placeholder|xxxx|your[_-]|<[^>]+>|\$\{|process\.env|os\.environ|getenv"#),
            secret_cmp: re(r"(?i)\b\w*(token|secret|passw(or)?d|digest|hmac|signature|credential|api_?key|presented)\w*\b[^;\n(]*(==|!=|===|!==)|(==|!=|===|!==)[^;\n]*\b\w*(token|secret|passw(or)?d|digest|hmac|signature|credential|api_?key|expected)\w*\b"),
            cmp_noise: re(r#"\.len\(\)|\blen\(|\.length\b|\bnil\b|\bnull\b|\bNone\b|undefined|[=!]=\s*0\b|\bis_empty"#),
            destructive: re(r"(?i)\bdrop\s+(table|column|database|schema|index|constraint|view)\b|\btruncate\b|\bop\.drop_\w+\(|\bdrop_(table|column|index)\(|\bremove_column\b|\bdrop_table\b"),
            delete_all: re(r"(?i)^\s*delete\s+from\s+[\w.`\x22]+\s*;"),
            alter: re(r"(?i)\balter\s+table\b[^;]*\b(not\s+null|type|rename|set\s+default)\b|\balter_column\(|\bchange_column\b|\brename_(column|table)\b"),
            sync_removed: re(r"(?i)\.lock\(\)|\.Lock\(\)|\.RLock\(\)|\bsynchronized\b|with\s+[\w.]*lock\w*\s*:|\bMutex\b|\bRwLock\b|\bsemaphore\b|\bwg\.Wait\(\)|\.join\(\)|\bawait\b|\.Unlock\(\)|atomic\.|\bSeqCst\b|\bAcquire\b|\bRelease\b"),
            conc_weak: re(r"Ordering::Relaxed|unsafe\s+impl\s+(Send|Sync)\b|\bstatic\s+mut\b|\bvolatile\b|\bsync\.Pool\b"),
            conc_added: re(r"\bgo\s+func\b|\bgo\s+\w+(\.\w+)*\(|thread::spawn|tokio::spawn|\bThread\(|asyncio\.create_task|\bnew Worker\(|ThreadPoolExecutor|std::thread::scope|rayon::|\bPromise\.all\b|\bsetInterval\("),
            shell_interp: re(r#"\bexec(Sync)?\(\s*[`"'][^)\n]*(\$\{|["']\s*\+|\+\s*["'])|\bexec(Sync)?\(\s*`|Command::new\(\s*"(sh|bash|zsh|cmd|cmd\.exe|powershell)"|\bexec\.Command\(\s*"(sh|bash|zsh|cmd)"|\bos\.system\(|shell\s*=\s*True|\bRuntime\.getRuntime\(\)\.exec\(\s*"[^"]*"\s*\+"#),
            dep_script: re(r#""(preinstall|install|postinstall|prepare|prepublish|prepublishOnly)"\s*:"#),
            dep_source: re(r#"(?i)git\+|\bgit\s*=|"git"\s*:|\bgit@|github:|https?://|\bfile:|\bpath\s*=|^\s*replace\b|--index-url|--extra-index-url|--trusted-host|[:=]\s*"\*"|:\s*"latest""#),
            ci_unsafe: re(r"(?i)pull_request_target|permissions:\s*write-all|continue-on-error:\s*true|--no-verify|\bif:\s*(false|\$\{\{\s*false)|curl[^|\n]*\|\s*(ba)?sh|ACTIONS_ALLOW_UNSECURE_COMMANDS|\ballow_failure:\s*true|--insecure\b|\bset\s+\+e\b"),
            ci_check: re(r"(?i)\b(run|script):.*\b(test|tests|lint|clippy|check|audit|scan|verify|fmt|pytest|jest|vet)\b|uses:\s*\S*(codeql|scan|audit|lint|trivy|snyk)"),
            docker_bad: re(r"(?i)^\s*USER\s+(root|0)\b|--privileged|^\s*ADD\s+https?://|chmod\s+(-R\s+)?777|curl[^|\n]*\|\s*(ba)?sh|--no-check-certificate|--allow-unauthenticated|--cap-add\s+(ALL|SYS_ADMIN)|^\s*FROM\s+\S+:latest\b"),
            docker_user: re(r"(?i)^\s*USER\s+\S+"),
            test_skip: re(r"#\[ignore\b|@pytest\.mark\.skip|@unittest\.skip|\b(it|test|describe)\.skip\(|\bx(it|describe|test)\(|\bt\.Skip(f|Now)?\(|@Disabled\b|@Ignore\b|\bpytest\.skip\(|\bskipTest\(|\bit\.todo\(|@pytest\.mark\.xfail"),
            boolean: re(r"\b(true|false|True|False|TRUE|FALSE|yes|no|on|off)\b"),
            key: re(r#"^\s*(?:pub\s+)?(?:const\s+|static\s+|let\s+|var\s+|export\s+)?["']?([A-Za-z_][\w.-]*)["']?\s*[:=]"#),
            json_dep: re(r#"^\s*"([@\w./-]+)"\s*:\s*"((?:[\^~<>=*\d]|git|http|file|npm|workspace|latest)[^"]*)""#),
            toml_dep: re(r#"^\s*([\w-]+)\s*=\s*("[^"]*"|\{.*\})"#),
            req_dep: re(r"^\s*([A-Za-z0-9][A-Za-z0-9_.-]*)\s*(==|>=|~=|<=|@|\[|$)"),
            gomod_dep: re(r"^\s*(?:require\s+)?([\w.-]+\.[\w./-]+)\s+v\d"),
        }
    })
}

fn is_match_row(r: &Row, re: &Regex) -> bool {
    re.is_match(r.text)
}

/// Rows of the first match of `re`, extended over directly following matching rows.
fn run_of(rows: &[Row], re: &Regex) -> Option<Range<u32>> {
    let first = rows.iter().position(|r| is_match_row(r, re))?;
    let start = rows[first].n;
    let mut end = start + 1;
    for r in &rows[first + 1..] {
        if r.n == end && is_match_row(r, re) {
            end += 1;
        } else if r.n >= end {
            break;
        }
    }
    Some(start..end)
}

fn count(rows: &[Row], re: &Regex) -> usize {
    rows.iter().map(|r| re.find_iter(r.text).count()).sum()
}

/// Matches added minus matches removed.
fn net(cx: &Ctx, re: &Regex) -> isize {
    count(cx.added, re) as isize - count(cx.removed, re) as isize
}

fn finding(
    kind: RiskKind,
    param: Param,
    magnitude: f32,
    message: impl Into<String>,
    at: At,
    rows: Option<Range<u32>>,
) -> Option<Finding> {
    Some(Finding {
        kind,
        param,
        magnitude,
        message: message.into(),
        at,
        rows: rows?,
    })
}

fn file_name(path: &str) -> String {
    path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase()
}

fn is_ci_path(lower: &str, name: &str) -> bool {
    lower.contains(".github/workflows/") || lower.contains(".gitlab-ci") || name == "jenkinsfile"
}

fn is_container_path(name: &str) -> bool {
    name.starts_with("dockerfile")
        || name.ends_with(".dockerfile")
        || name.starts_with("docker-compose")
}

fn in_migration(lower: &str, family: Family) -> bool {
    family == Family::Sql
        || lower
            .split('/')
            .any(|s| matches!(s, "migrations" | "migrate" | "migration" | "alembic"))
}

/// Runs every content detector.
pub(super) fn detect(cx: &Ctx) -> Vec<Finding> {
    let lower = cx.path.to_ascii_lowercase();
    let name = file_name(cx.path);
    let s = sigs();
    let mut out: Vec<Finding> = Vec::new();

    if cx.is_test {
        // Disabled tests.
        if net(cx, &s.test_skip) >= 1 {
            out.extend(finding(
                RiskKind::TestsDisabled,
                p!(test_disabled),
                1.0,
                "Disables tests (skip/ignore)",
                At::New,
                run_of(cx.added, &s.test_skip),
            ));
        }
        return out;
    }

    if cx.family.is_code() {
        security(cx, s, &mut out);
        concurrency(cx, s, &mut out);
    }
    if in_migration(&lower, cx.family) {
        migration(cx, s, &mut out);
    }
    if is_ci_path(&lower, &name) {
        ci(cx, s, &mut out);
    }
    if is_container_path(&name) {
        container(cx, s, &mut out);
    }
    manifest(cx, s, &name, &mut out);
    config_flips(cx, s, &mut out);
    out
}

fn security(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    let n = -net(cx, &s.auth_removed);
    if n >= 1 {
        out.extend(finding(
            RiskKind::AuthCheckRemoved,
            p!(auth_removed),
            1.0 + 0.1 * (n - 1).min(3) as f32,
            "Removes an authentication/authorization check",
            At::Old,
            run_of(cx.removed, &s.auth_removed),
        ));
    }
    if net(cx, &s.verification_off) >= 1 {
        out.extend(finding(
            RiskKind::VerificationDisabled,
            p!(verification_disabled),
            1.0,
            "Disables certificate or signature verification",
            At::New,
            run_of(cx.added, &s.verification_off),
        ));
    }
    if net(cx, &s.weak_crypto) >= 1 {
        out.extend(finding(
            RiskKind::WeakCrypto,
            p!(weak_crypto),
            1.0,
            "Adds a weak hash/cipher or non-cryptographic randomness",
            At::New,
            run_of(cx.added, &s.weak_crypto),
        ));
    } else if net(cx, &s.hardening) <= -1 {
        out.extend(finding(
            RiskKind::WeakCrypto,
            p!(weak_crypto),
            0.9,
            "Removes a password-hashing, salting or constant-time primitive",
            At::Old,
            run_of(cx.removed, &s.hardening),
        ));
    }
    let secret_rows: Vec<Row> = cx
        .added
        .iter()
        .filter(|r| s.secret_literal.is_match(r.text) && !s.placeholder.is_match(r.text))
        .copied()
        .collect();
    if let Some(first) = secret_rows.first() {
        out.push(Finding {
            kind: RiskKind::HardcodedSecret,
            param: p!(hardcoded_secret),
            magnitude: 1.0,
            message: "Adds what looks like a hard-coded credential".into(),
            at: At::New,
            rows: first.n..first.n + 1,
        });
    }
    let secret_cmp = |r: &&Row| s.secret_cmp.is_match(r.text) && !s.cmp_noise.is_match(r.text);
    let cmp_added: Vec<Row> = cx.added.iter().filter(secret_cmp).copied().collect();
    let cmp_removed = cx.removed.iter().filter(secret_cmp).count();
    if cmp_added.len() > cmp_removed && net(cx, &s.hardening) <= 0 {
        // Only when the comparison is new and nothing constant-time was added.
        if let Some(first) = cmp_added.first() {
            out.push(Finding {
                kind: RiskKind::SecretCompare,
                param: p!(secret_compare),
                magnitude: 1.0,
                message: "Compares a secret with `==` (not constant time)".into(),
                at: At::New,
                rows: first.n..first.n + 1,
            });
        }
    }
    if net(cx, &s.shell_interp) >= 1 {
        out.extend(finding(
            RiskKind::DynamicExecution,
            p!(shell_interpolated),
            1.0,
            "Runs a shell command built from interpolated strings",
            At::New,
            run_of(cx.added, &s.shell_interp),
        ));
    }
}

fn concurrency(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    let removed = -net(cx, &s.sync_removed);
    if removed >= 1 {
        out.extend(finding(
            RiskKind::ConcurrencyRemoved,
            p!(concurrency_removed),
            1.0 + 0.15 * (removed - 1).min(3) as f32,
            format!("Removes {removed} lock/await/ordering operation(s): possible race or unfinished work"),
            At::Old,
            run_of(cx.removed, &s.sync_removed),
        ));
    }
    if net(cx, &s.conc_weak) >= 1 {
        out.extend(finding(
            RiskKind::ConcurrencyWeakened,
            p!(concurrency_weakened),
            1.0,
            "Weakens thread-safety guarantees (Relaxed ordering, unsafe Send/Sync, static mut)",
            At::New,
            run_of(cx.added, &s.conc_weak),
        ));
    }
    if net(cx, &s.conc_added) >= 1 {
        out.extend(finding(
            RiskKind::ConcurrencyAdded,
            p!(concurrency_added),
            1.0,
            "Adds concurrent execution (threads, tasks, goroutines)",
            At::New,
            run_of(cx.added, &s.conc_added),
        ));
    }
}

fn migration(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    let destructive = run_of(cx.added, &s.destructive).or_else(|| run_of(cx.added, &s.delete_all));
    if destructive.is_some() {
        out.extend(finding(
            RiskKind::DestructiveMigration,
            p!(migration_destructive),
            1.0,
            "Destructive migration: drops or deletes schema or data",
            At::New,
            destructive,
        ));
    } else if net(cx, &s.alter) >= 1 {
        out.extend(finding(
            RiskKind::DestructiveMigration,
            p!(migration_alter),
            1.0,
            "Alters existing columns (can fail or lose data on populated tables)",
            At::New,
            run_of(cx.added, &s.alter),
        ));
    }
}

fn ci(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    if net(cx, &s.ci_unsafe) >= 1 {
        out.extend(finding(
            RiskKind::CiWeakened,
            p!(ci_unsafe),
            1.0,
            "Weakens CI safety (pull_request_target, write-all, failures ignored, ...)",
            At::New,
            run_of(cx.added, &s.ci_unsafe),
        ));
    }
    if -net(cx, &s.ci_check) >= 1 {
        out.extend(finding(
            RiskKind::CiWeakened,
            p!(ci_check_removed),
            1.0,
            "Removes a test/lint/scan step from CI",
            At::Old,
            run_of(cx.removed, &s.ci_check),
        ));
    }
}

fn container(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    if net(cx, &s.docker_bad) >= 1 {
        out.extend(finding(
            RiskKind::ContainerWeakened,
            p!(container_unsafe),
            1.0,
            "Weakens container isolation (root user, remote ADD, privileged, curl | sh)",
            At::New,
            run_of(cx.added, &s.docker_bad),
        ));
    } else if -net(cx, &s.docker_user) >= 1 && count(cx.added, &s.docker_user) == 0 {
        out.extend(finding(
            RiskKind::ContainerWeakened,
            p!(container_unsafe),
            1.0,
            "Drops the non-root USER of the container",
            At::Old,
            run_of(cx.removed, &s.docker_user),
        ));
    }
}

/// Section header (`[name]`) in effect at `row` of a TOML text.
fn toml_section(src: &str, row: u32) -> String {
    let mut current = String::new();
    for (i, l) in src.lines().enumerate() {
        if i as u32 > row {
            break;
        }
        let t = l.trim();
        if t.starts_with('[') && t.ends_with(']') {
            current = t
                .trim_matches(|c| c == '[' || c == ']')
                .to_ascii_lowercase();
        }
    }
    current
}

fn dep_key<'a>(s: &Sigs, name: &str, row: &Row<'a>, src: &str, side_new: bool) -> Option<String> {
    let key_of = |re: &Regex| {
        re.captures(row.text)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())
    };
    match name {
        "package.json" | "composer.json" | "deno.json" => {
            let c = s.json_dep.captures(row.text)?;
            let key = c.get(1)?.as_str();
            (!matches!(
                key,
                "version" | "name" | "main" | "type" | "license" | "engines" | "node"
            ))
            .then(|| key.to_string())
        }
        "cargo.toml" | "pyproject.toml" => {
            if side_new && !toml_section(src, row.n).ends_with("dependencies") {
                return None;
            }
            key_of(&s.toml_dep)
        }
        "requirements.txt" | "pipfile" => key_of(&s.req_dep),
        "go.mod" => key_of(&s.gomod_dep),
        _ => None,
    }
}

fn manifest(cx: &Ctx, s: &Sigs, name: &str, out: &mut Vec<Finding>) {
    let supported = matches!(
        name,
        "package.json"
            | "composer.json"
            | "deno.json"
            | "cargo.toml"
            | "pyproject.toml"
            | "requirements.txt"
            | "pipfile"
            | "go.mod"
            | "gemfile"
    );
    if !supported {
        return;
    }
    if name == "package.json" {
        if let Some(rows) = run_of(cx.added, &s.dep_script) {
            let new_script = count(cx.removed, &s.dep_script) == 0;
            if new_script {
                out.extend(finding(
                    RiskKind::DependencyRisk,
                    p!(dep_script),
                    1.0,
                    "Adds an install-time script (runs on every install)",
                    At::New,
                    Some(rows),
                ));
            }
        }
    }
    let removed_keys: Vec<String> = cx
        .removed
        .iter()
        .filter_map(|r| dep_key(s, name, r, "", false))
        .collect();
    let added: Vec<(&Row, String)> = cx
        .added
        .iter()
        .filter_map(|r| dep_key(s, name, r, cx.new_src, true).map(|k| (r, k)))
        .collect();
    let fresh: Vec<&(&Row, String)> = added
        .iter()
        .filter(|(_, k)| !removed_keys.contains(k))
        .collect();
    if let Some((row, key)) = fresh.first() {
        out.push(Finding {
            kind: RiskKind::DependencyRisk,
            param: p!(dep_added),
            magnitude: 1.0 + 0.1 * (fresh.len() - 1).min(4) as f32,
            message: format!("Adds dependency `{key}`: new code and a new supply-chain link"),
            at: At::New,
            rows: row.n..row.n + 1,
        });
    }
    // Dependency source or pin changes: git/URL/path/wildcard, loosened pins, `replace`.
    let source_rows: Vec<Row> = cx
        .added
        .iter()
        .filter(|r| s.dep_source.is_match(r.text) && !r.text.contains("registry"))
        .copied()
        .collect();
    let new_source = source_rows
        .iter()
        .find(|r| !cx.removed.iter().any(|o| o.text.trim() == r.text.trim()));
    let loosened = (name == "requirements.txt" || name == "pipfile")
        .then(|| {
            added.iter().find(|(r, k)| {
                !r.text.contains("==")
                    && cx.removed.iter().any(|o| {
                        o.text.contains("==")
                            && dep_key(s, name, o, "", false).as_deref() == Some(k)
                    })
            })
        })
        .flatten();
    if let Some(r) = new_source {
        out.push(Finding {
            kind: RiskKind::DependencyRisk,
            param: p!(dep_source),
            magnitude: 1.0,
            message: "Dependency taken from git/URL/path, wildcard version or replace directive"
                .into(),
            at: At::New,
            rows: r.n..r.n + 1,
        });
    } else if let Some((r, k)) = loosened {
        out.push(Finding {
            kind: RiskKind::DependencyRisk,
            param: p!(dep_source),
            magnitude: 0.8,
            message: format!("Loosens the version pin of `{k}`"),
            at: At::New,
            rows: r.n..r.n + 1,
        });
    }
}

/// Key of a `key: value` / `key = value` / `const KEY: T = value` line.
fn key_of(s: &Sigs, text: &str) -> Option<String> {
    let k = s.key.captures(text)?.get(1)?.as_str().to_string();
    // Rust constants and fields: `pub const DEFAULT_STRICT: bool = true;` -> DEFAULT_STRICT.
    Some(k)
}

/// +1 when flipping the key to `true` is the unsafe direction, -1 when `false` is, 0 unknown.
fn polarity(key: &str) -> i8 {
    let k = key.to_ascii_lowercase();
    const UNSAFE_WHEN_TRUE: &[&str] = &[
        "debug",
        "insecure",
        "allow",
        "skip",
        "disable",
        "trust",
        "unsafe",
        "permissive",
        "bypass",
        "admin",
    ];
    const UNSAFE_WHEN_FALSE: &[&str] = &[
        "verify", "secure", "strict", "require", "enforce", "auth", "valid", "check", "encrypt",
        "ssl", "tls", "csrf", "https", "sign", "sandbox", "protect", "audit",
    ];
    if UNSAFE_WHEN_TRUE.iter().any(|w| k.contains(w)) {
        1
    } else if UNSAFE_WHEN_FALSE.iter().any(|w| k.contains(w)) {
        -1
    } else {
        0
    }
}

fn config_flips(cx: &Ctx, s: &Sigs, out: &mut Vec<Finding>) {
    for (old_rows, new_rows) in cx.hunks {
        let removed: Vec<&Row> = cx
            .removed
            .iter()
            .filter(|r| old_rows.contains(&r.n))
            .collect();
        let added: Vec<&Row> = cx
            .added
            .iter()
            .filter(|r| new_rows.contains(&r.n))
            .collect();
        for a in &added {
            for r in &removed {
                if r.text == a.text || !s.boolean.is_match(r.text) || !s.boolean.is_match(a.text) {
                    continue;
                }
                let norm = |t: &str| {
                    s.boolean
                        .replace_all(t, "B")
                        .split_whitespace()
                        .collect::<Vec<_>>()
                        .join(" ")
                };
                if norm(r.text) != norm(a.text) {
                    continue;
                }
                let value = |t: &str| {
                    s.boolean
                        .find(t)
                        .map(|m| matches!(m.as_str(), "true" | "True" | "TRUE" | "yes" | "on"))
                        .unwrap_or(false)
                };
                let (was, now) = (value(r.text), value(a.text));
                if was == now {
                    continue;
                }
                let key = key_of(s, a.text).unwrap_or_else(|| "value".into());
                let pol = polarity(&key);
                let unsafe_dir = (pol > 0 && now) || (pol < 0 && !now);
                let magnitude = if unsafe_dir {
                    1.5
                } else if pol == 0 {
                    1.0
                } else {
                    0.5
                };
                out.push(Finding {
                    kind: RiskKind::ConfigDefaultFlipped,
                    param: p!(config_flip),
                    magnitude,
                    message: format!("Flips `{key}` from {was} to {now}"),
                    at: At::New,
                    rows: a.n..a.n + 1,
                });
                break;
            }
        }
    }
    // Keep the strongest flips only.
    out.sort_by(|a, b| b.magnitude.total_cmp(&a.magnitude));
    let mut seen = 0;
    out.retain(|f| {
        if f.kind == RiskKind::ConfigDefaultFlipped {
            seen += 1;
            seen <= 3
        } else {
            true
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&'static str]) -> Vec<Row<'static>> {
        lines
            .iter()
            .enumerate()
            .map(|(i, t)| Row {
                n: i as u32,
                text: t,
            })
            .collect()
    }

    #[test]
    fn polarity_follows_key_words() {
        assert_eq!(polarity("tls_verify"), -1);
        assert_eq!(polarity("DEBUG"), 1);
        assert_eq!(polarity("colour"), 0);
    }

    #[test]
    fn run_of_extends_over_consecutive_rows() {
        let r = rows(&["a", "verify=False", "verify=False", "b", "verify=False"]);
        let re = Regex::new("verify").expect("pattern");
        assert_eq!(run_of(&r, &re), Some(1..3));
    }

    #[test]
    fn sigs_compile() {
        let _ = sigs();
    }
}
