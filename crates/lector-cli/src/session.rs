//! Opening the application core and a project without a window.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _};
use lector_app::commands::Source;
use lector_app::{AppCore, CommandContext, CoreOptions};
use lector_workspace::Project;

/// The headless application core, with every command that exists without a window.
///
/// This is the one place where the CLI and the MCP server assemble the registry: commands
/// of other crates that should be available headlessly are registered here (the built-in
/// read, document and git-write commands come with [`AppCore::open`]).
pub fn open_core() -> AppCore {
    let core = AppCore::open(CoreOptions::default());
    // Agent sessions (`agent.*`) are part of the headless API too: scripts and MCP
    // clients can start a session, prompt, list turns and undo a turn.
    lector_agent::commands::register(&core);
    core
}

/// The directory a command works in: `dir`, or the current directory. A file path means
/// its parent folder.
pub fn resolve_dir(dir: Option<&Path>) -> anyhow::Result<PathBuf> {
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    let dir = dunce::canonicalize(&dir).map_err(|e| anyhow!("{}: {e}", dir.display()))?;
    if dir.is_file() {
        return dir
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| anyhow!("{} has no parent folder", dir.display()));
    }
    Ok(dir)
}

/// Opens a project at `dir` (git repository discovery included) without language servers.
pub fn open_project(dir: &Path) -> anyhow::Result<Project> {
    let (mut settings, warnings) = lector_config::load_settings_for(Some(dir));
    for w in warnings {
        tracing::warn!("settings: {w}");
    }
    // Headless: never start language servers.
    settings.lsp.enabled = false;
    Project::open(dir, settings).map_err(|e| anyhow!("cannot open {}: {e}", dir.display()))
}

/// Like [`open_project`] but the folder must be inside a git repository.
pub fn open_repo_project(dir: &Path) -> anyhow::Result<Project> {
    let project = open_project(dir)?;
    if project.repo().is_none() {
        bail!("{} is not inside a git repository", dir.display());
    }
    Ok(project)
}

/// A command context for the CLI.
pub fn context(core: &AppCore, project: Option<Project>) -> CommandContext {
    CommandContext::new(core.clone(), project, Source::Cli)
}
