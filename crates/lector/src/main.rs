//! Lector: a reading-first code viewer and git review tool.
#![forbid(unsafe_code)]

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

/// The project root for a file: the nearest ancestor with `.git`, else its folder.
fn root_for_file(file: &Path) -> PathBuf {
    let parent = file
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let mut dir = Some(parent.as_path());
    while let Some(d) = dir {
        if d.join(".git").exists() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    parent
}

fn build_request(cli: &Cli) -> anyhow::Result<OpenRequest> {
    let arg = cli.path.clone().unwrap_or_else(|| ".".into());
    let (path, line, col) = parse_target(&arg);
    let path =
        std::fs::canonicalize(&path).with_context(|| format!("cannot open {}", path.display()))?;
    let fixed_size = std::env::var_os("LECTOR_SCREENSHOT").map(|_| (1600., 1000.));
    if path.is_dir() {
        Ok(OpenRequest {
            root: path,
            file: None,
            fixed_size,
            project: None,
        })
    } else {
        Ok(OpenRequest {
            root: root_for_file(&path),
            file: Some((path, line, col)),
            fixed_size,
            project: None,
        })
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
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    let mut request = build_request(&cli)?;
    // Settings, the project (git discovery, LSP config) and themes load on a
    // background thread while GPUI starts up.
    let root = request.root.clone();
    let loader = std::thread::spawn(move || {
        let (settings, warnings) = lector_config::load_settings_for(Some(&root));
        let project = lector_workspace::Project::open(&root, settings.clone());
        (settings, warnings, project)
    });
    let (registry, mode) = load_themes(&cli);
    lector_ui::startup::log("themes loaded");

    Application::new()
        .with_assets(lector_ui::assets::Assets)
        .run(move |cx| {
            let Ok((settings, warnings, project)) = loader.join() else {
                eprintln!("lector: startup failed");
                cx.quit();
                return;
            };
            match project {
                Ok(project) => request.project = Some(project),
                Err(err) => {
                    eprintln!("lector: cannot open {}: {err}", request.root.display());
                    cx.quit();
                    return;
                }
            }
            lector_ui::startup::log("app ready, project open");
            lector_ui::init(cx, registry, mode, (settings, warnings));
            lector_ui_git::init(cx);
            lector_ui::startup::log("ui initialized");
            if let Err(err) = lector_ui::open_workspace_window(cx, request) {
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
}
