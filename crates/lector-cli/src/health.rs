//! `lector health`: the local crash log and session health (nothing leaves the machine).

use lector_app::health::{health_summary, HealthSummary};
use serde_json::{json, Value};

use crate::args::HealthArgs;
use crate::out::{print_json, truncate, Io};

/// The JSON document of `lector health --json` (`"schema"` is added by the printer).
///
/// Fields: `sessions`, `running`, `clean`, `abnormal`, `panics`, `crash_free_rate` (0..1 or
/// `null` until a session has finished), `by_version[]`, `crash_reports`, `crash_dir`,
/// `last_crashes[]` (`file`, `path`, `time`, `version`, `os`, `thread`, `location`, `message`).
pub fn summary_json(s: &HealthSummary, limit: usize) -> Value {
    json!({
        "sessions": s.sessions,
        "running": s.running,
        "clean": s.clean,
        "abnormal": s.abnormal,
        "panics": s.panics,
        "crash_free_rate": s.crash_free_rate,
        "by_version": s.by_version.iter().map(|v| json!({
            "version": v.version,
            "sessions": v.sessions,
            "clean": v.clean,
            "abnormal": v.abnormal,
            "panics": v.panics,
        })).collect::<Vec<_>>(),
        "crash_reports": s.crash_reports,
        "crash_dir": s.crash_dir.to_string_lossy(),
        "last_crashes": s.last_crashes.iter().take(limit).map(|c| json!({
            "file": c.file,
            "path": c.path.to_string_lossy(),
            "time": c.time,
            "version": c.version,
            "os": c.os,
            "thread": c.thread,
            "location": c.location,
            "message": c.message,
        })).collect::<Vec<_>>(),
    })
}

/// Runs `lector health`.
pub fn run(args: &HealthArgs, io: &mut Io) -> anyhow::Result<()> {
    let summary = health_summary();
    if args.json {
        return Ok(print_json(io.out, summary_json(&summary, args.limit))?);
    }
    let s = io.style;
    let mut line = format!(
        "{} sessions: {} clean, {} abnormal",
        summary.sessions, summary.clean, summary.abnormal
    );
    if summary.running > 0 {
        line.push_str(&format!(", {} running", summary.running));
    }
    writeln!(io.out, "{line}")?;
    match summary.crash_free_rate {
        Some(rate) => writeln!(
            io.out,
            "crash-free {}",
            s.bold(&format!("{:.1}%", rate * 100.0))
        )?,
        None => writeln!(
            io.out,
            "{}",
            s.dim("crash-free rate: no finished sessions yet")
        )?,
    }
    writeln!(io.out, "panics: {}", summary.panics)?;
    for v in &summary.by_version {
        writeln!(
            io.out,
            "  {} {} sessions, {} abnormal, {} panics",
            s.cyan(&format!("{:<10}", v.version)),
            v.sessions,
            v.abnormal,
            v.panics
        )?;
    }
    if summary.last_crashes.is_empty() {
        writeln!(io.out, "{}", s.dim("No crash reports."))?;
        return Ok(());
    }
    writeln!(
        io.out,
        "crash reports: {} in {}",
        summary.crash_reports,
        summary.crash_dir.display()
    )?;
    for c in summary.last_crashes.iter().take(args.limit) {
        let time = c
            .time
            .as_deref()
            .map(|t| t.replace('T', " ").replace('Z', ""))
            .unwrap_or_else(|| "?".to_string());
        writeln!(
            io.out,
            "  {} {} {} {}",
            s.dim(&time),
            s.cyan(c.version.as_deref().unwrap_or("?")),
            truncate(c.message.as_deref().unwrap_or("?"), 80),
            s.dim(&format!("[{}]", c.thread.as_deref().unwrap_or("?"))),
        )?;
        if let Some(location) = &c.location {
            writeln!(io.out, "      {}", s.dim(location))?;
        }
        writeln!(io.out, "      {}", s.dim(&c.path.to_string_lossy()))?;
    }
    let hidden = summary.last_crashes.len().saturating_sub(args.limit);
    if hidden > 0 {
        writeln!(
            io.out,
            "{}",
            s.dim(&format!("  ... {hidden} older (use --limit)"))
        )?;
    }
    // Reports are plain text: say how to send one, without sending anything ourselves.
    writeln!(
        io.out,
        "{}",
        s.dim("Reports never leave this computer; attach one to a bug report if you want to share it.")
    )?;
    Ok(())
}
