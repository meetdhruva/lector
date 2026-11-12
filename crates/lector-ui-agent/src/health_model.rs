//! The crash log as the UI shows it (v0.7): the summary text, the rows of recent crash reports
//! with their "Open" and "Copy details" payloads, and the quiet note after an abnormal exit.
//! UI-free and unit-tested. Nothing here sends anything anywhere.

use std::path::PathBuf;

use lector_app::health::{CrashFile, HealthSummary};

/// One recent crash report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrashRow {
    /// The report file ("Open" opens it read-only).
    pub path: PathBuf,
    /// "2026-11-13T10:22:33Z, 0.7.0".
    pub when: String,
    /// The panic message (one line).
    pub message: String,
    /// "src/foo.rs:12:5 on thread main", when known.
    pub location: String,
    /// What "Copy details" puts on the clipboard.
    pub details: String,
}

/// The text of the Crash Log tab.
#[derive(Debug, Clone, PartialEq)]
pub struct HealthModel {
    /// "Sessions: 12 (10 clean, 1 abnormal, 1 running)".
    pub sessions_line: String,
    /// "Crash-free: 90.9%" or "Crash-free: not enough sessions yet".
    pub rate_line: String,
    /// "Crash reports on disk: 3" and the directory.
    pub reports_line: String,
    /// Per-version lines ("0.7.0: 8 sessions, 1 abnormal, 0 panics").
    pub versions: Vec<String>,
    /// Recent reports, newest first.
    pub rows: Vec<CrashRow>,
    /// The folder with the reports.
    pub crash_dir: PathBuf,
    /// "Nothing is sent anywhere..." reminder.
    pub privacy_line: &'static str,
}

/// The privacy statement shown with the log.
pub const PRIVACY_LINE: &str =
    "These files stay on this computer. Nothing is sent anywhere; share a report only if you choose to.";

/// Builds the model from a summary.
pub fn health_model(summary: &HealthSummary) -> HealthModel {
    let sessions_line = format!(
        "Sessions: {} ({} clean, {} abnormal, {} running)",
        summary.sessions, summary.clean, summary.abnormal, summary.running
    );
    let rate_line = match summary.crash_free_rate {
        Some(r) => format!("Crash-free: {:.1}%", r * 100.0),
        None => "Crash-free: not enough finished sessions yet".to_string(),
    };
    let reports_line = format!(
        "Crash reports on disk: {} ({})",
        summary.crash_reports,
        summary.crash_dir.display()
    );
    let versions = summary
        .by_version
        .iter()
        .map(|v| {
            format!(
                "{}: {} session{}, {} abnormal, {} panic{}",
                v.version,
                v.sessions,
                plural(v.sessions),
                v.abnormal,
                v.panics,
                plural(v.panics)
            )
        })
        .collect();
    HealthModel {
        sessions_line,
        rate_line,
        reports_line,
        versions,
        rows: summary.last_crashes.iter().map(crash_row).collect(),
        crash_dir: summary.crash_dir.clone(),
        privacy_line: PRIVACY_LINE,
    }
}

fn plural(n: u32) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

fn crash_row(c: &CrashFile) -> CrashRow {
    let when = match (&c.time, &c.version) {
        (Some(t), Some(v)) => format!("{t}, {v}"),
        (Some(t), None) => t.clone(),
        (None, Some(v)) => v.clone(),
        (None, None) => c.file.clone(),
    };
    let message = c
        .message
        .clone()
        .unwrap_or_else(|| "(no message)".to_string());
    let location = match (&c.location, &c.thread) {
        (Some(l), Some(t)) => format!("{l} on thread {t}"),
        (Some(l), None) => l.clone(),
        (None, Some(t)) => format!("thread {t}"),
        (None, None) => String::new(),
    };
    let mut details = format!("Lector crash report {}\n", c.file);
    for (label, value) in [
        ("Time", &c.time),
        ("Version", &c.version),
        ("OS", &c.os),
        ("Thread", &c.thread),
        ("Location", &c.location),
    ] {
        if let Some(v) = value {
            details.push_str(&format!("{label}: {v}\n"));
        }
    }
    details.push_str(&format!("Message: {message}\n"));
    CrashRow {
        path: c.path.clone(),
        when,
        message,
        location,
        details,
    }
}

/// The text of the Crash Log tab as plain lines (the tab shows these, then the rows).
pub fn health_text(model: &HealthModel) -> String {
    let mut out = String::new();
    out.push_str(&model.sessions_line);
    out.push('\n');
    out.push_str(&model.rate_line);
    out.push('\n');
    out.push_str(&model.reports_line);
    out.push('\n');
    for v in &model.versions {
        out.push_str("  ");
        out.push_str(v);
        out.push('\n');
    }
    out.push('\n');
    out.push_str(model.privacy_line);
    out.push('\n');
    out
}

/// The quiet note after an abnormal previous exit.
pub const ABNORMAL_EXIT_NOTE: &str = "Lector didn't shut down cleanly last time";
/// Its button.
pub const ABNORMAL_EXIT_BUTTON: &str = "Show details";

#[cfg(test)]
mod tests {
    use super::*;
    use lector_app::health::VersionHealth;

    fn summary() -> HealthSummary {
        HealthSummary {
            sessions: 12,
            running: 1,
            clean: 10,
            abnormal: 1,
            panics: 2,
            crash_free_rate: Some(10.0 / 11.0),
            by_version: vec![
                VersionHealth {
                    version: "0.7.0".into(),
                    sessions: 8,
                    clean: 7,
                    abnormal: 1,
                    panics: 2,
                },
                VersionHealth {
                    version: "0.6.0".into(),
                    sessions: 1,
                    clean: 1,
                    abnormal: 0,
                    panics: 0,
                },
            ],
            last_crashes: vec![CrashFile {
                path: "/data/crashes/a.txt".into(),
                file: "a.txt".into(),
                time: Some("2026-11-13T10:22:33Z".into()),
                version: Some("0.7.0".into()),
                os: Some("linux".into()),
                thread: Some("main".into()),
                location: Some("src/foo.rs:12:5".into()),
                message: Some("index out of bounds".into()),
            }],
            crash_reports: 3,
            crash_dir: "/data/crashes".into(),
        }
    }

    #[test]
    fn formats_the_summary() {
        let m = health_model(&summary());
        assert_eq!(
            m.sessions_line,
            "Sessions: 12 (10 clean, 1 abnormal, 1 running)"
        );
        assert_eq!(m.rate_line, "Crash-free: 90.9%");
        assert_eq!(m.reports_line, "Crash reports on disk: 3 (/data/crashes)");
        assert_eq!(m.versions[0], "0.7.0: 8 sessions, 1 abnormal, 2 panics");
        assert_eq!(m.versions[1], "0.6.0: 1 session, 0 abnormal, 0 panics");
        let text = health_text(&m);
        assert!(text.contains("Nothing is sent anywhere"));
        assert!(text.starts_with("Sessions: 12"));
    }

    #[test]
    fn a_crash_row_has_open_and_copy_payloads() {
        let m = health_model(&summary());
        let row = &m.rows[0];
        assert_eq!(row.path, PathBuf::from("/data/crashes/a.txt"));
        assert_eq!(row.when, "2026-11-13T10:22:33Z, 0.7.0");
        assert_eq!(row.message, "index out of bounds");
        assert_eq!(row.location, "src/foo.rs:12:5 on thread main");
        assert!(row.details.contains("Message: index out of bounds"));
        assert!(row.details.contains("Version: 0.7.0"));
        assert!(row.details.starts_with("Lector crash report a.txt"));
    }

    #[test]
    fn an_empty_log_reads_plainly() {
        let mut s = summary();
        s.sessions = 0;
        s.clean = 0;
        s.abnormal = 0;
        s.running = 0;
        s.crash_free_rate = None;
        s.by_version.clear();
        s.last_crashes.clear();
        s.crash_reports = 0;
        let m = health_model(&s);
        assert_eq!(m.rate_line, "Crash-free: not enough finished sessions yet");
        assert!(m.rows.is_empty() && m.versions.is_empty());
        assert_eq!(
            m.sessions_line,
            "Sessions: 0 (0 clean, 0 abnormal, 0 running)"
        );
    }

    #[test]
    fn rows_cope_with_missing_fields() {
        let mut s = summary();
        s.last_crashes = vec![CrashFile {
            path: "/c/b.txt".into(),
            file: "b.txt".into(),
            time: None,
            version: None,
            os: None,
            thread: None,
            location: None,
            message: None,
        }];
        let row = &health_model(&s).rows[0];
        assert_eq!(row.when, "b.txt");
        assert_eq!(row.message, "(no message)");
        assert_eq!(row.location, "");
    }
}
