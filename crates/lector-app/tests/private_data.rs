//! ADR 0010, item 7: Lector's data is private. Under an explicit `umask 022` (in a child
//! process, so the umask of other tests is untouched), everything Lector creates in its data
//! directory is owner-only: directories `0700`, files `0600`, the shadow repository's objects
//! included; and an existing data directory with wider permissions is narrowed at start.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use lector_app::commands::{CommandContext, Source};
use lector_app::health::{write_report, HealthPaths, ProcessIdentity, SessionHealth};
use lector_app::Project;
use lector_app::{AppCore, CoreOptions};
use serde_json::json;

const CHILD_ENV: &str = "LECTOR_PRIVATE_DATA_CHILD";

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git");
    assert!(out.status.success(), "git {args:?}: {out:?}");
}

/// Every path under `dir` whose mode gives group or others any access (sockets excluded).
fn too_wide(dir: &Path) -> Vec<(PathBuf, u32)> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let Ok(meta) = fs::symlink_metadata(e.path()) else {
                continue;
            };
            let mode = meta.permissions().mode() & 0o777;
            if meta.is_dir() {
                stack.push(e.path());
            }
            if (meta.is_dir() || meta.is_file()) && mode & 0o077 != 0 {
                out.push((e.path(), mode));
            }
        }
    }
    out
}

/// Runs in the child under `umask 022`: creates the database, a shadow backup (and a second
/// one, so git writes more objects into an existing repository), a crash report and a session
/// record, then checks the modes.
#[test]
fn private_data_child() {
    let Some(base) = std::env::var_os(CHILD_ENV) else {
        return;
    };
    let base = PathBuf::from(base);
    let data = base.join("data");
    let work = base.join("work");
    fs::create_dir_all(&work).expect("mkdir");
    git(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("a.txt"), "one\n").expect("write");
    fs::write(work.join(".env"), "SECRET=1\n").expect("write");
    git(&work, &["add", "a.txt"]);
    git(&work, &["commit", "-q", "-m", "init"]);
    let core = AppCore::open(CoreOptions {
        workers: Some(2),
        data_dir: Some(data.clone()),
        db_path: Some(data.join("lector.redb")),
        ..Default::default()
    });
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&work, settings).expect("project");
    let cx = CommandContext::new(core.clone(), Some(project), Source::Internal);
    for text in ["two\n", "three\n"] {
        fs::write(work.join("a.txt"), text).expect("write");
        core.registry()
            .invoke("git.discard", json!({ "paths": ["a.txt"] }), &cx)
            .expect("discard with a backup");
    }
    let paths = HealthPaths::in_dir(&data);
    write_report(&paths.crashes, 1, "message: test\n", 5).expect("report");
    let session = SessionHealth::begin_in(paths, ProcessIdentity::current(), "0.0.0");
    session.end_clean();
    assert!(
        data.join("shadow").is_dir(),
        "no shadow repository was created"
    );
    let wide = too_wide(&data);
    assert!(
        wide.is_empty(),
        "group/world accessible under umask 022: {wide:#?}"
    );
    let top = fs::metadata(&data).expect("data").permissions().mode() & 0o777;
    assert_eq!(top, 0o700);
}

#[test]
fn everything_in_the_data_directory_is_owner_only_under_umask_022() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let exe = std::env::current_exe().expect("exe");
    let out = Command::new("sh")
        .arg("-c")
        .arg("umask 022 && exec \"$0\" --exact private_data_child --nocapture --test-threads=1")
        .arg(&exe)
        .env(CHILD_ENV, tmp.path())
        .output()
        .expect("child");
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("1 passed"), "{text}");
}

/// An existing data directory with wider permissions (an older Lector, umask 022) is narrowed
/// when a core opens.
#[test]
fn a_wider_data_directory_is_narrowed_at_start() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    fs::create_dir_all(data.join("shadow")).expect("mkdir");
    fs::set_permissions(&data, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::set_permissions(data.join("shadow"), fs::Permissions::from_mode(0o775)).expect("chmod");
    let _core = AppCore::open(CoreOptions {
        in_memory_db: true,
        workers: Some(1),
        data_dir: Some(data.clone()),
        ..Default::default()
    });
    let mode = |p: &Path| fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("shadow")), 0o700);
}
