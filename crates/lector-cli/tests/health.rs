//! `lector health`: local crash log and session health, through the console binary.
//!
//! Each test points `LECTOR_DATA_DIR` at its own folder, so the user's real state is never read.

use std::path::{Path, PathBuf};
use std::process::Command;

use lector_app::health::{
    render_report, write_report, HealthPaths, PanicDetails, ProcessIdentity, ReportContext,
    SessionHealth,
};
use serde_json::Value;

const BIN: &str = env!("CARGO_BIN_EXE_lector-cli");

struct Fx {
    _tmp: tempfile::TempDir,
    data: PathBuf,
}

impl Fx {
    fn new() -> Fx {
        let tmp = tempfile::tempdir().expect("tempdir");
        let data = dunce::canonicalize(tmp.path())
            .expect("canonical")
            .join("data");
        Fx { _tmp: tmp, data }
    }

    fn paths(&self) -> HealthPaths {
        HealthPaths::in_dir(&self.data)
    }

    fn run(&self, args: &[&str]) -> (i32, String, String) {
        let out = Command::new(BIN)
            .args(args)
            .env("LECTOR_DATA_DIR", &self.data)
            .env("LECTOR_CONFIG_DIR", self.data.join("config"))
            .env_remove("RUST_LOG")
            .env_remove("NO_COLOR")
            .output()
            .expect("binary runs");
        (
            out.status.code().expect("exit code"),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn json(&self, args: &[&str]) -> Value {
        let (code, stdout, stderr) = self.run(args);
        assert_eq!(code, 0, "{args:?}: {stderr}");
        serde_json::from_str(&stdout).unwrap_or_else(|e| panic!("not JSON ({e}): {stdout}"))
    }

    /// Three clean sessions, one that died without ending, and one crash report.
    fn seed(&self) {
        let p = self.paths();
        for _ in 0..3 {
            SessionHealth::begin_in(p.clone(), ProcessIdentity::current(), "0.7.0").end_clean();
        }
        let dead = ProcessIdentity {
            pid: 4_000_000_000,
            start_token: Some("1".into()),
            exe: "lector".into(),
        };
        SessionHealth::begin_in(p.clone(), dead, "0.7.0");
        let details = PanicDetails {
            message: "index out of bounds: the len is 3 but the index is 7".into(),
            location: Some("crates/lector-ui/src/view.rs:10:5".into()),
            thread: Some("main".into()),
            backtrace: "   0: lector::boom\n".into(),
        };
        let cx = ReportContext {
            version: "0.7.0".into(),
            time: 1_700_000_000,
            session: None,
            home: None,
        };
        write_report(&p.crashes, cx.time, &render_report(&details, &cx), 20).expect("report");
    }
}

fn is_path(v: &Value) -> bool {
    v.as_str().is_some_and(|s| Path::new(s).is_absolute())
}

#[test]
fn json_schema_on_an_empty_data_directory() {
    let fx = Fx::new();
    let v = fx.json(&["health", "--json"]);
    assert_eq!(v["schema"], 1);
    for key in [
        "sessions",
        "running",
        "clean",
        "abnormal",
        "panics",
        "crash_reports",
    ] {
        assert_eq!(v[key], 0, "{key}");
    }
    assert_eq!(v["crash_free_rate"], Value::Null);
    assert_eq!(v["by_version"], serde_json::json!([]));
    assert_eq!(v["last_crashes"], serde_json::json!([]));
    assert!(is_path(&v["crash_dir"]), "{}", v["crash_dir"]);
    // Reading the health log neither counts as a session nor creates anything.
    assert!(!fx.data.join("health").exists());
    assert!(!fx.data.join("crashes").exists());
}

#[test]
fn json_reports_sessions_rate_versions_and_crashes() {
    let fx = Fx::new();
    fx.seed();
    let v = fx.json(&["health", "--json"]);
    assert_eq!(v["schema"], 1);
    assert_eq!(v["sessions"], 4);
    assert_eq!(v["clean"], 3);
    assert_eq!(v["abnormal"], 1);
    assert_eq!(v["running"], 0);
    assert_eq!(v["crash_free_rate"], 0.75);
    let versions = v["by_version"].as_array().expect("by_version");
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0]["version"], "0.7.0");
    assert_eq!(versions[0]["sessions"], 4);
    assert_eq!(versions[0]["abnormal"], 1);
    assert_eq!(v["crash_reports"], 1);
    let crash = &v["last_crashes"][0];
    assert!(crash["file"].as_str().expect("file").starts_with("crash-"));
    assert!(is_path(&crash["path"]));
    assert_eq!(crash["time"], "2023-11-14T22:13:20Z");
    assert_eq!(crash["version"], "0.7.0");
    assert_eq!(crash["thread"], "main");
    assert_eq!(crash["location"], "crates/lector-ui/src/view.rs:10:5");
    assert_eq!(
        crash["message"],
        "index out of bounds: the len is 3 but the index is 7"
    );
    assert!(crash["os"]
        .as_str()
        .expect("os")
        .contains(std::env::consts::OS));
    // `--limit 0` lists none but still counts them.
    let v = fx.json(&["health", "--json", "-n", "0"]);
    assert_eq!(v["last_crashes"], serde_json::json!([]));
    assert_eq!(v["crash_reports"], 1);
    // Running it did not add a session.
    assert_eq!(fx.json(&["health", "--json"])["sessions"], 4);
}

#[test]
fn human_output_is_compact_and_plain_when_piped() {
    let fx = Fx::new();
    let (code, stdout, _) = fx.run(&["health"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("0 sessions"), "{stdout}");
    assert!(stdout.contains("no finished sessions yet"), "{stdout}");
    assert!(stdout.contains("No crash reports."), "{stdout}");

    fx.seed();
    let (code, stdout, _) = fx.run(&["health"]);
    assert_eq!(code, 0);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines[0], "4 sessions: 3 clean, 1 abnormal", "{stdout}");
    assert_eq!(lines[1], "crash-free 75.0%", "{stdout}");
    assert_eq!(lines[2], "panics: 0", "{stdout}");
    assert!(stdout.contains("index out of bounds"), "{stdout}");
    assert!(
        stdout.contains("crates/lector-ui/src/view.rs:10:5"),
        "{stdout}"
    );
    assert!(!stdout.contains('\x1b'), "no colors when piped");
}

#[test]
fn health_is_a_listed_subcommand() {
    assert!(lector_cli::SUBCOMMANDS.contains(&"health"));
    assert!(lector_cli::is_subcommand("health".as_ref()));
}
