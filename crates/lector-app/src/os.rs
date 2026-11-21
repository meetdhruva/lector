//! Small operating-system integrations the UI asks for: reveal a path in the system file
//! manager, and read the OS "reduce motion" preference.
//!
//! These are **not** writes to the user's files or repository, so they are plain service
//! functions, not registry commands (ADR 0002 puts every process Lector starts in
//! `lector-app`; only writes must be commands). Processes go through `lector-process`, which
//! gives every child its own process tree and, on Windows, `CREATE_NO_WINDOW` so no console
//! window flashes. Nothing here blocks the caller's thread except
//! [`detect_os_reduce_motion`], which is documented as blocking.

use std::path::Path;
use std::time::Duration;

use lector_process::{OutputMode, ProcessCommand, ProcessKind, RunOptions, StdinMode};

use crate::core::AppCore;
use crate::scheduler::{JobKey, JobKind, Priority, Subject};

/// Menu label for [`reveal_in_file_manager`] on this platform.
pub const REVEAL_LABEL: &str = if cfg!(target_os = "macos") {
    "Reveal in Finder"
} else if cfg!(target_os = "windows") {
    "Reveal in File Explorer"
} else {
    "Reveal in File Manager"
};

/// The command that shows `path` in the file manager of this platform: `open -R` (macOS,
/// selects it), `explorer /select,` (Windows, selects it), else `xdg-open` on the folder
/// (the file's folder for a file).
fn reveal_command(path: &Path) -> ProcessCommand {
    let cmd = if cfg!(target_os = "macos") {
        ProcessCommand::new("open").arg("-R").arg(path)
    } else if cfg!(target_os = "windows") {
        ProcessCommand::new("explorer").arg(format!("/select,{}", path.display()))
    } else {
        let dir = if path.is_dir() {
            path
        } else {
            path.parent().unwrap_or(path)
        };
        ProcessCommand::new("xdg-open").arg(dir)
    };
    cmd.kind(ProcessKind::Helper)
        .label("reveal in file manager")
        .stdin(StdinMode::Null)
        .stdout(OutputMode::Null)
        .stderr(OutputMode::Null)
        // The file manager the opener hands off to must outlive it.
        .kill_tree_on_exit(false)
}

/// Shows `path` in the OS file manager (selects files where supported). Returns at once: the
/// opener runs on the core's scheduler; failures are logged, never fatal.
pub fn reveal_in_file_manager(core: &AppCore, path: &Path) {
    let cmd = reveal_command(path);
    let shown = path.display().to_string();
    // Waits on another program: the blocking lane, never the compute workers.
    let key = JobKey::new(JobKind::ExternalProgram, Subject::App);
    let _ = core.scheduler().spawn(key, Priority::Visible, move |_| {
        // Explorer exits non-zero even when it worked, so only a failure to start counts.
        if let Err(err) = lector_process::run(&cmd, &RunOptions::default()) {
            tracing::warn!("cannot reveal {shown}: {err}");
        }
    });
}

/// Reads the operating system's "reduce motion" preference. Blocking (runs a tiny CLI with a
/// time limit); call it from a background thread.
///
/// * `LECTOR_REDUCE_MOTION=1|0` overrides everything (also useful in containers).
/// * macOS: `defaults read com.apple.universalaccess reduceMotion`.
/// * Linux: GNOME/GTK `org.gnome.desktop.interface enable-animations` through `gsettings`
///   (animations disabled means reduced motion).
/// * Windows and other desktops: unknown (`None`).
pub fn detect_os_reduce_motion() -> Option<bool> {
    if let Ok(v) = std::env::var("LECTOR_REDUCE_MOTION") {
        match v.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => return Some(true),
            "0" | "false" | "no" | "off" => return Some(false),
            _ => {}
        }
    }
    let run = |program: &str, args: &[&str]| -> Option<String> {
        let cmd = ProcessCommand::new(program)
            .args(args)
            .kind(ProcessKind::Helper)
            .label(format!("{program} (reduce motion)"))
            .stdin(StdinMode::Null)
            .stderr(OutputMode::Null);
        let out = lector_process::run(
            &cmd,
            &RunOptions::default().with_timeout(Duration::from_secs(5)),
        )
        .ok()?;
        out.success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    };
    if cfg!(target_os = "macos") {
        let v = run(
            "defaults",
            &["read", "com.apple.universalaccess", "reduceMotion"],
        )?;
        return Some(v == "1");
    }
    if cfg!(target_os = "linux") {
        let v = run(
            "gsettings",
            &["get", "org.gnome.desktop.interface", "enable-animations"],
        )?;
        return match v.as_str() {
            "false" => Some(true),
            "true" => Some(false),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reveal_selects_files_where_the_platform_can() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let cmd = reveal_command(&file);
        let args: Vec<String> = cmd
            .get_args()
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        if cfg!(target_os = "macos") {
            assert_eq!(cmd.get_program(), "open");
            assert_eq!(args[0], "-R");
        } else if cfg!(target_os = "windows") {
            assert_eq!(cmd.get_program(), "explorer");
            assert!(args[0].starts_with("/select,"));
        } else {
            assert_eq!(cmd.get_program(), "xdg-open");
            assert_eq!(args, vec![dir.path().to_string_lossy().into_owned()]);
        }
        assert_eq!(cmd.get_kind(), ProcessKind::Helper);
    }
}
