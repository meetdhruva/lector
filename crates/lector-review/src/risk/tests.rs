//! Unit tests of the risk assessment: signals, evidence, weights, facts and ranking.

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
        &crate::content::NoCache,
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
    let new =
        "admin.delete('/u/:id', async (req, res) => {\n  await removeUser(req.params.id);\n});\n";
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
    let new =
        "import requests\n\n\ndef get(u):\n    return requests.get(u, timeout=5, verify=False)\n";
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

    let cargo = "[package]\nname = \"x\"\n\n[dependencies]\nserde = \"1\"\nreqwest = \"0.12\"\n";
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
    let replace = format!("{gomod}\nreplace github.com/lib/pq => example.invalid/fork/pq v1.0.0\n");
    assert!(has(
        &report("go.mod", gomod, &replace),
        RiskKind::DependencyRisk
    ));
}

#[test]
fn ci_and_container_signals() {
    let ci = "on:\n  pull_request:\njobs:\n  t:\n    steps:\n      - run: cargo test --workspace\n";
    let weak = "on:\n  pull_request_target:\njobs:\n  t:\n    steps:\n      - run: echo skipped\n";
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
    let new =
        "import pytest\n\n@pytest.mark.skip(reason='flaky')\ndef test_a():\n    assert f() == 1\n";
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
    let old = "def f(cur, n):\n    cur.execute(\"SELECT id FROM users WHERE name = %s\", (n,))\n";
    let fstr = "def f(cur, n):\n    cur.execute(f\"SELECT id FROM users WHERE name = '{n}'\")\n";
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
    let old =
        "pub fn a(x: u32) -> u32 {\n    let v = parse(x)?;\n    if v > 1 { return v; }\n    v\n}\n";
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
