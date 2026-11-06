//! Lector: a reading-first code viewer and git review tool.
#![forbid(unsafe_code)]
// Release builds on Windows are GUI apps: no console window when launched from Explorer.
#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod logging;

use std::path::{Path, PathBuf};

use anyhow::Context as _;
use clap::Parser;
use lector_theme::ThemeRegistry;
use lector_ui::gpui::Application;
use lector_ui::{OpenRequest, ThemeMode};

/// Command-line interface.
#[derive(Parser, Debug)]
#[command(
    name = "lector",
    version,
    about = "A fast, reading-first code viewer and git review tool."
)]
struct Cli {
    /// Folder or file to open, optionally with a position: PATH[:LINE[:COL]].
    path: Option<String>,

    /// Theme name (e.g. "Lector Light"). Defaults to following the OS appearance.
    #[arg(long)]
    theme: Option<String>,

    /// Open a window without a folder (the welcome screen), ignoring PATH.
    #[arg(long)]
    welcome: bool,
}

/// Splits `PATH[:LINE[:COL]]`, preferring an existing path verbatim.
fn parse_target(arg: &str) -> (PathBuf, Option<u32>, Option<u32>) {
    if Path::new(arg).exists() {
        return (PathBuf::from(arg), None, None);
    }
    let mut parts: Vec<&str> = arg.rsplitn(3, ':').collect();
    parts.reverse();
    match parts.as_slice() {
        [path, line, col] if line.parse::<u32>().is_ok() && col.parse::<u32>().is_ok() => {
            (PathBuf::from(path), line.parse().ok(), col.parse().ok())
        }
        [.., line, col] if col.parse::<u32>().is_ok() && line.parse::<u32>().is_err() => {
            let path = &arg[..arg.len() - col.len() - 1];
            (PathBuf::from(path), col.parse().ok(), None)
        }
        [path, line] if line.parse::<u32>().is_ok() => {
            (PathBuf::from(path), line.parse().ok(), None)
        }
        _ => (PathBuf::from(arg), None, None),
    }
}

/// Whether `dir` is a reasonable project to open implicitly (when no path was given).
///
/// Launchers (Finder, the Start menu, desktop entries) start apps in `/`, the home
/// directory or a system directory; opening those would scan the whole disk.
fn is_sensible_default_dir(dir: &Path) -> bool {
    if dir.parent().is_none() {
        return false;
    }
    if dirs::home_dir().is_some_and(|home| home == dir) {
        return false;
    }
    let lower = dir.to_string_lossy().to_lowercase().replace('\\', "/");
    let system_prefixes = [
        "/system",
        "/usr",
        "/bin",
        "/sbin",
        "/etc",
        "/var",
        "/proc",
        "/dev",
        "/applications",
        "/library",
        "c:/windows",
        "c:/program files",
    ];
    !system_prefixes
        .iter()
        .any(|prefix| lower == *prefix || lower.starts_with(&format!("{prefix}/")))
}

/// The target to open when no path was given on the command line: the
/// current directory when it is a sensible project, else the most recent
/// project that still exists. `None` opens a window with the welcome view.
fn default_target() -> Option<String> {
    if let Ok(cwd) = std::env::current_dir() {
        if is_sensible_default_dir(&cwd) {
            return Some(cwd.to_string_lossy().into_owned());
        }
    }
    lector_ui::commands_bridge::recent_projects()
        .most_recent_existing()
        .map(|recent| recent.to_string_lossy().into_owned())
}

/// The window to open at startup; `None` = a window without a folder.
fn build_request(cli: &Cli) -> anyhow::Result<Option<OpenRequest>> {
    if cli.welcome {
        return Ok(None);
    }
    match cli.path.clone().or_else(default_target) {
        Some(arg) => request_for_target(&arg).map(Some),
        None => Ok(None),
    }
}

/// Fixed window size for headless screenshots (`LECTOR_SCREENSHOT`).
fn screenshot_size() -> Option<(f32, f32)> {
    std::env::var_os("LECTOR_SCREENSHOT").map(|_| (1600., 1000.))
}

/// Builds an open request for `PATH[:LINE[:COL]]`.
fn request_for_target(arg: &str) -> anyhow::Result<OpenRequest> {
    let (path, line, col) = parse_target(arg);
    let path =
        dunce::canonicalize(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let fixed_size = screenshot_size();
    if path.is_dir() {
        Ok(OpenRequest {
            root: path,
            file: None,
            fixed_size,
            project: None,
        })
    } else {
        Ok(OpenRequest {
            root: lector_ui::root_for_file(&path),
            file: Some((path, line, col)),
            fixed_size,
            project: None,
        })
    }
}

/// Converts a URL delivered by the OS ("Open With", drag onto the dock icon) to a local path.
fn path_from_open_url(raw: &str) -> Option<PathBuf> {
    let url = url::Url::parse(raw).ok()?;
    if url.scheme() == "file" {
        url.to_file_path().ok()
    } else {
        None
    }
}

/// Records `root` as a recent project.
fn record_recent(root: &Path) {
    if let Err(err) = lector_ui::commands_bridge::recent_projects().record(root) {
        tracing::warn!("cannot record recent project: {err}");
    }
}

/// Opens a window for an OS-delivered path, reporting failures on stderr.
fn open_target_window(cx: &mut lector_ui::gpui::App, target: &str) {
    match request_for_target(target) {
        Ok(request) => {
            record_recent(&request.root);
            if let Err(err) = lector_ui::open_workspace_window(cx, request) {
                eprintln!("lector: failed to open window: {err:#}");
            }
        }
        Err(err) => eprintln!("lector: {err:#}"),
    }
}

fn load_themes(cli: &Cli) -> (ThemeRegistry, ThemeMode) {
    let mut registry = ThemeRegistry::new();
    let dir = std::env::var_os("LECTOR_THEMES_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(lector_config::paths::themes_dir);
    let (_, errors) = registry.load_dir(&dir);
    for err in errors {
        tracing::warn!("theme: {err}");
    }
    let mode = match &cli.theme {
        Some(name) => match registry.get(name) {
            Some(theme) => ThemeMode::Named(theme.name.clone().into()),
            None => {
                eprintln!(
                    "lector: unknown theme {name:?}; available: {}",
                    registry.names().join(", ")
                );
                ThemeMode::System
            }
        },
        None => ThemeMode::System,
    };
    (registry, mode)
}

fn main() -> anyhow::Result<()> {
    lector_ui::startup::mark_process_start();
    let cli = Cli::parse();
    logging::init();
    let mut request = build_request(&cli)?;
    // Settings, the project (git discovery, LSP config) and themes load on a
    // background thread while GPUI starts up.
    let root = request.as_ref().map(|r| r.root.clone());
    let loader = std::thread::spawn(move || {
        let (settings, warnings) = lector_config::load_settings_for(root.as_deref());
        let project = root
            .as_ref()
            .map(|root| lector_workspace::Project::open(root, settings.clone()));
        (settings, warnings, project)
    });
    let (registry, mode) = load_themes(&cli);
    lector_ui::startup::log("themes loaded");

    // Paths the OS asks us to open (macOS "Open With", dropping onto the dock icon).
    let (open_tx, mut open_rx) = futures::channel::mpsc::unbounded::<Vec<String>>();
    let app = Application::new().with_assets(lector_ui::assets::Assets);
    app.on_open_urls(move |urls| {
        let _ = open_tx.unbounded_send(urls);
    });
    app.on_reopen(|cx| {
        // Dock icon clicked with no windows open: reopen the default project.
        if cx.windows().is_empty() {
            match default_target() {
                Some(target) => open_target_window(cx, &target),
                None => {
                    if let Err(err) = lector_ui::open_empty_window(cx, screenshot_size()) {
                        eprintln!("lector: failed to open window: {err:#}");
                    }
                }
            }
        }
    });
    app.run(move |cx| {
        cx.spawn(async move |cx| {
            use futures::StreamExt as _;
            while let Some(urls) = open_rx.next().await {
                for url in urls {
                    if let Some(path) = path_from_open_url(&url) {
                        let target = path.to_string_lossy().into_owned();
                        let _ = cx.update(|cx| open_target_window(cx, &target));
                    }
                }
            }
        })
        .detach();
        let Ok((settings, warnings, project)) = loader.join() else {
            eprintln!("lector: startup failed");
            cx.quit();
            return;
        };
        if let (Some(request), Some(project)) = (request.as_mut(), project) {
            match project {
                Ok(project) => request.project = Some(project),
                Err(err) => {
                    eprintln!("lector: cannot open {}: {err}", request.root.display());
                    cx.quit();
                    return;
                }
            }
            record_recent(&request.root);
        }
        lector_ui::startup::log("app ready, project open");
        lector_ui::init(cx, registry, mode, (settings, warnings));
        lector_ui_git::init(cx);
        lector_ui::startup::log("ui initialized");
        let opened = match request {
            Some(request) => lector_ui::open_workspace_window(cx, request),
            None => lector_ui::open_empty_window(cx, screenshot_size()),
        };
        if let Err(err) = opened {
            eprintln!("lector: failed to open window: {err:#}");
            cx.quit();
        }
        lector_ui::startup::log("window opened");
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets() {
        assert_eq!(
            parse_target("src/x.rs:12"),
            (PathBuf::from("src/x.rs"), Some(12), None)
        );
        assert_eq!(
            parse_target("src/x.rs:12:4"),
            (PathBuf::from("src/x.rs"), Some(12), Some(4))
        );
        assert_eq!(
            parse_target("src/x.rs"),
            (PathBuf::from("src/x.rs"), None, None)
        );
        assert_eq!(parse_target("C:dir"), (PathBuf::from("C:dir"), None, None));
    }

    #[test]
    fn open_urls() {
        #[cfg(unix)]
        assert_eq!(
            path_from_open_url("file:///tmp/a%20b/c.rs"),
            Some(PathBuf::from("/tmp/a b/c.rs"))
        );
        assert_eq!(path_from_open_url("https://example.com"), None);
        assert_eq!(path_from_open_url("not a url"), None);
    }

    #[test]
    fn default_dir_heuristics() {
        assert!(!is_sensible_default_dir(Path::new("/")));
        assert!(!is_sensible_default_dir(Path::new("/usr/local")));
        assert!(!is_sensible_default_dir(Path::new("/Applications")));
        assert!(!is_sensible_default_dir(Path::new("C:\\Windows\\System32")));
        if let Some(home) = dirs::home_dir() {
            assert!(!is_sensible_default_dir(&home));
            assert!(is_sensible_default_dir(&home.join("src/project")));
        }
        assert!(is_sensible_default_dir(Path::new("/tmp/project")));
    }
}
