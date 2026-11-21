//! Classification of a changed file: language family, and test, lockfile, manifest,
//! generated, documentation and security-sensitive paths.

use lector_syntax::Language;

use super::{Param, RiskWeights};
use crate::review::FileDiff;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Family {
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

pub(super) fn family_of(lang: Option<&Language>) -> Family {
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
    pub(super) fn is_code(self) -> bool {
        !matches!(self, Family::Other)
    }
    pub(super) fn comment_prefixes(self) -> &'static [&'static str] {
        match self {
            Family::Python | Family::Shell | Family::Dynamic => &["#", "//"],
            Family::Sql => &["--"],
            Family::Other => &[],
            _ => &["//", "/*", "*"],
        }
    }
}

pub(super) fn path_of(f: &FileDiff) -> String {
    f.change
        .new_path
        .clone()
        .or_else(|| f.change.old_path.clone())
        .unwrap_or_default()
}

pub(super) fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub(super) fn extension(path: &str) -> &str {
    let name = file_name(path);
    name.rsplit_once('.').map_or("", |(_, e)| e)
}

pub(super) fn is_test_path(path: &str) -> bool {
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

pub(super) fn is_lockfile(path: &str) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    LOCKFILES.contains(&name.as_str())
}

pub(super) fn is_manifest(path: &str) -> bool {
    let name = file_name(path).to_ascii_lowercase();
    MANIFESTS.contains(&name.as_str()) || name.ends_with(".csproj") || name.ends_with(".gemspec")
}

pub(super) fn is_config_ext(path: &str) -> bool {
    matches!(
        extension(&path.to_ascii_lowercase()),
        "json" | "yaml" | "yml" | "toml" | "ini" | "cfg" | "conf" | "properties" | "env" | "plist"
    )
}

pub(super) fn is_docs_path(path: &str) -> bool {
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

pub(super) fn generated_reason(path: &str, old: &str, new: &str) -> Option<&'static str> {
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

pub(super) fn security_path_reason(path: &str) -> Option<(Param, String)> {
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
