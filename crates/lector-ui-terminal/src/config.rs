//! Settings → spawn options: shell profile, starting folder, environment, fonts.
//!
//! Pure functions (no GPUI): the shell program, its arguments and environment come only from
//! the user's settings (`lector-config` strips them from project settings).

use std::path::{Path, PathBuf};

use lector_config::{NewTerminalCwd, TerminalSettings};
use lector_terminal::{ShellProfile, SpawnOptions, TermSize};

/// Line height used when `terminal.line_height` is unset.
pub const DEFAULT_LINE_HEIGHT: f32 = 1.2;

/// The profile "New Terminal" (or "New Terminal with Profile" for `name`) runs.
///
/// Order: the named profile, `default_profile`, `shell`, the platform default. Unknown names
/// fall through to the next step.
pub fn resolve_profile(settings: &TerminalSettings, name: Option<&str>) -> ShellProfile {
    let by_name = |n: &str| {
        settings
            .profiles
            .iter()
            .find(|p| p.name == n && !p.program.trim().is_empty())
    };
    let named = name
        .and_then(by_name)
        .or_else(|| settings.default_profile.as_deref().and_then(by_name));
    if let Some(p) = named {
        return ShellProfile {
            name: p.name.clone(),
            program: PathBuf::from(p.program.trim()),
            args: p.args.clone(),
            env: p.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        };
    }
    if let Some(shell) = settings
        .shell
        .as_ref()
        .filter(|s| !s.program.trim().is_empty())
    {
        let mut profile = ShellProfile::new(shell.program.trim());
        profile.args = shell.args.clone();
        return profile;
    }
    ShellProfile::default_for_platform()
}

/// Names of the configured profiles (for the profile picker), in settings order.
pub fn profile_names(settings: &TerminalSettings) -> Vec<String> {
    settings
        .profiles
        .iter()
        .filter(|p| !p.name.trim().is_empty() && !p.program.trim().is_empty())
        .map(|p| p.name.clone())
        .collect()
}

/// Where a new terminal starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CwdRequest {
    /// "Open in Terminal" on a folder or a file (its folder is used).
    Explicit(PathBuf),
    /// "New Terminal" without a context: `terminal.new_terminal_cwd`.
    Default,
}

/// What the starting folder is resolved against.
#[derive(Debug, Clone, Copy)]
pub struct CwdContext<'a> {
    /// The project root, when the window has a folder.
    pub project_root: Option<&'a Path>,
    /// The active file, if any.
    pub active_file: Option<&'a Path>,
    /// The user's home directory.
    pub home: Option<&'a Path>,
    /// `terminal.new_terminal_cwd`.
    pub setting: NewTerminalCwd,
}

/// Resolves the starting folder. `is_dir` checks the file system (injected for tests).
/// `None` only when nothing usable exists (the core then uses Lector's own cwd).
pub fn resolve_cwd(
    request: &CwdRequest,
    ctx: CwdContext<'_>,
    is_dir: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    let folder_of = |path: &Path| -> Option<PathBuf> {
        if is_dir(path) {
            Some(path.to_path_buf())
        } else {
            path.parent().filter(|p| is_dir(p)).map(Path::to_path_buf)
        }
    };
    if let CwdRequest::Explicit(path) = request {
        if let Some(dir) = folder_of(path) {
            return Some(dir);
        }
    }
    if ctx.setting == NewTerminalCwd::ActiveFile {
        if let Some(dir) = ctx.active_file.and_then(folder_of) {
            return Some(dir);
        }
    }
    ctx.project_root
        .filter(|p| is_dir(p))
        .or(ctx.home.filter(|h| is_dir(h)))
        .map(Path::to_path_buf)
}

/// Directory Lector installs its shell-integration snippets into (its own data dir).
pub fn shell_integration_dir() -> PathBuf {
    lector_config::paths::data_dir().join("shell-integration")
}

/// Spawn options for a new session.
pub fn spawn_options(
    settings: &TerminalSettings,
    profile: ShellProfile,
    cwd: Option<PathBuf>,
    size: TermSize,
) -> SpawnOptions {
    let mut options = SpawnOptions::new(profile);
    options.cwd = cwd;
    options.size = size;
    options.scrollback = settings.scrollback.clamp(100, 1_000_000);
    options.env = settings
        .env
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    options.shell_integration = settings.shell_integration.then(shell_integration_dir);
    options
}

/// Effective terminal font settings: (family override, size override, line height).
pub fn font_overrides(settings: &TerminalSettings) -> (Option<String>, Option<f32>, f32) {
    let family = settings
        .font_family
        .as_ref()
        .map(|f| f.trim().to_string())
        .filter(|f| !f.is_empty());
    let size = settings
        .font_size
        .filter(|s| s.is_finite() && *s > 0.)
        .map(|s| s.clamp(6., 48.));
    let line_height = settings
        .line_height
        .filter(|l| l.is_finite())
        .map(|l| l.clamp(1.0, 3.0))
        .unwrap_or(DEFAULT_LINE_HEIGHT);
    (family, size, line_height)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_config::{TerminalProfile, TerminalShell};

    #[test]
    fn profile_order() {
        let mut s = TerminalSettings::default();
        assert_eq!(
            resolve_profile(&s, None).program,
            ShellProfile::default_for_platform().program
        );
        s.shell = Some(TerminalShell {
            program: "/bin/zsh".into(),
            args: vec!["-l".into()],
        });
        let p = resolve_profile(&s, None);
        assert_eq!(p.program, PathBuf::from("/bin/zsh"));
        assert_eq!(p.args, vec!["-l".to_string()]);
        s.profiles.push(TerminalProfile {
            name: "fish".into(),
            program: "fish".into(),
            args: vec![],
            env: [("A".to_string(), "1".to_string())].into_iter().collect(),
        });
        assert_eq!(resolve_profile(&s, Some("fish")).name, "fish");
        assert_eq!(
            resolve_profile(&s, Some("fish")).env,
            vec![("A".into(), "1".into())]
        );
        assert_eq!(
            resolve_profile(&s, Some("nope")).program,
            PathBuf::from("/bin/zsh")
        );
        s.default_profile = Some("fish".into());
        assert_eq!(resolve_profile(&s, None).name, "fish");
        assert_eq!(profile_names(&s), vec!["fish".to_string()]);
    }

    fn dirs(path: &Path) -> bool {
        let s = path.to_string_lossy();
        matches!(
            s.as_ref(),
            "/proj" | "/proj/src" | "/home/u" | "/outside" | "/proj/src/deep"
        )
    }

    #[test]
    fn cwd_for_each_entry_point() {
        let ctx = CwdContext {
            project_root: Some(Path::new("/proj")),
            active_file: Some(Path::new("/proj/src/main.rs")),
            home: Some(Path::new("/home/u")),
            setting: NewTerminalCwd::Project,
        };
        // File tree: folder → that folder; file → its parent.
        let folder = CwdRequest::Explicit("/proj/src/deep".into());
        assert_eq!(
            resolve_cwd(&folder, ctx, dirs),
            Some("/proj/src/deep".into())
        );
        let file = CwdRequest::Explicit("/proj/src/lib.rs".into());
        assert_eq!(resolve_cwd(&file, ctx, dirs), Some("/proj/src".into()));
        // Editor tab: the file's parent.
        let tab = CwdRequest::Explicit("/proj/src/main.rs".into());
        assert_eq!(resolve_cwd(&tab, ctx, dirs), Some("/proj/src".into()));
        // Project root / empty tree area.
        let root = CwdRequest::Explicit("/proj".into());
        assert_eq!(resolve_cwd(&root, ctx, dirs), Some("/proj".into()));
        // A deleted folder falls back to the default.
        let gone = CwdRequest::Explicit("/gone/x/y".into());
        assert_eq!(resolve_cwd(&gone, ctx, dirs), Some("/proj".into()));
        // New Terminal: project root by default, the active file's folder when asked.
        assert_eq!(
            resolve_cwd(&CwdRequest::Default, ctx, dirs),
            Some("/proj".into())
        );
        let active = CwdContext {
            setting: NewTerminalCwd::ActiveFile,
            ..ctx
        };
        assert_eq!(
            resolve_cwd(&CwdRequest::Default, active, dirs),
            Some("/proj/src".into())
        );
        let no_file = CwdContext {
            active_file: None,
            ..active
        };
        assert_eq!(
            resolve_cwd(&CwdRequest::Default, no_file, dirs),
            Some("/proj".into())
        );
        // No project: home.
        let no_project = CwdContext {
            project_root: None,
            active_file: None,
            ..ctx
        };
        assert_eq!(
            resolve_cwd(&CwdRequest::Default, no_project, dirs),
            Some("/home/u".into())
        );
    }

    #[test]
    fn spawn_options_from_settings() {
        let mut s = TerminalSettings::default();
        s.env.insert("EDITOR".into(), "vim".into());
        s.scrollback = 5;
        let o = spawn_options(
            &s,
            ShellProfile::new("/bin/sh"),
            Some("/tmp".into()),
            TermSize::new(80, 24),
        );
        assert_eq!(o.scrollback, 100, "clamped");
        assert_eq!(o.env, vec![("EDITOR".into(), "vim".into())]);
        assert!(o.shell_integration.is_some());
        s.shell_integration = false;
        let o = spawn_options(
            &s,
            ShellProfile::new("/bin/sh"),
            None,
            TermSize::new(80, 24),
        );
        assert!(o.shell_integration.is_none());
        let (family, size, lh) = font_overrides(&s);
        assert_eq!((family, size, lh), (None, None, DEFAULT_LINE_HEIGHT));
        s.font_size = Some(100.);
        s.font_family = Some("  ".into());
        assert_eq!(font_overrides(&s).1, Some(48.));
        assert_eq!(font_overrides(&s).0, None);
    }
}
