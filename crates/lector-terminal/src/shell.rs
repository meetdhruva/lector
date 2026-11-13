//! Shell profiles, environment preparation and opt-in shell integration.
//!
//! # Shell integration
//!
//! The snippets ([`BASH_INTEGRATION`], [`ZSH_INTEGRATION`], [`FISH_INTEGRATION`],
//! [`POWERSHELL_INTEGRATION`]) emit OSC 7 (working directory) and OSC 133
//! (prompt/command marks). They never execute anything else and are idempotent.
//!
//! Injection at spawn (opt-in through [`SpawnOptions::shell_integration`])
//! never edits the user's dotfiles; files are written only to the directory the
//! caller passes (Lector's own data directory):
//!
//! | shell | mechanism | user files still loaded |
//! |---|---|---|
//! | zsh | `ZDOTDIR` shim (`.zshenv` restores the user's `ZDOTDIR`, sources their `.zshenv`, then the snippet) | all |
//! | fish | a `vendor_conf.d` entry found through `XDG_DATA_DIRS` (restored by the snippet) | all |
//! | bash | `--rcfile` pointing at a file that sources `/etc/bash.bashrc`, `~/.bashrc`, then the snippet | all of an interactive non-login shell; skipped for login shells or when the profile already has arguments |
//! | PowerShell, cmd, others | none (no clean mechanism) | — |
//!
//! For PowerShell users can dot-source [`POWERSHELL_INTEGRATION`] from their
//! profile themselves.

use std::path::{Path, PathBuf};

use crate::error::TerminalError;
use crate::geometry::TermSize;

/// A shell (or any program) to run in a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProfile {
    /// Display name ("zsh", "PowerShell", ...).
    pub name: String,
    /// Program to run (absolute, or looked up on `PATH`).
    pub program: PathBuf,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment for this profile.
    pub env: Vec<(String, String)>,
}

/// Recognised shell families (from the program's file name).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ShellKind {
    /// GNU bash.
    Bash,
    /// zsh.
    Zsh,
    /// fish.
    Fish,
    /// PowerShell (`pwsh` or Windows PowerShell).
    PowerShell,
    /// `cmd.exe`.
    Cmd,
    /// Another POSIX shell (`sh`, `dash`, `ksh`, ...).
    Posix,
    /// Anything else.
    Other,
}

impl ShellProfile {
    /// A profile running `program` with no arguments, named after the file.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        let program = program.into();
        let name = program
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| program.to_string_lossy().into_owned());
        Self {
            name,
            program,
            args: Vec::new(),
            env: Vec::new(),
        }
    }

    /// The platform default shell.
    ///
    /// Unix: `$SHELL`, then `/bin/bash`, then `/bin/sh` (as a login shell on
    /// macOS, like Terminal.app). Windows: `pwsh.exe`, `powershell.exe`, then
    /// `cmd.exe`, looked up on `PATH` (falling back to `%ComSpec%`).
    pub fn default_for_platform() -> Self {
        #[cfg(windows)]
        {
            for (exe, name) in [
                ("pwsh.exe", "PowerShell"),
                ("powershell.exe", "Windows PowerShell"),
            ] {
                if let Some(p) = find_on_path(exe) {
                    let mut profile = ShellProfile::new(p);
                    profile.name = name.to_owned();
                    profile.args = vec!["-NoLogo".to_owned()];
                    return profile;
                }
            }
            let cmd = find_on_path("cmd.exe")
                .or_else(|| std::env::var_os("ComSpec").map(PathBuf::from))
                .unwrap_or_else(|| PathBuf::from(r"C:\Windows\System32\cmd.exe"));
            ShellProfile::new(cmd)
        }
        #[cfg(not(windows))]
        {
            let from_env = std::env::var_os("SHELL")
                .map(PathBuf::from)
                .filter(|p| p.is_absolute() && is_executable(p));
            let program = from_env
                .or_else(|| Some(PathBuf::from("/bin/bash")).filter(|p| is_executable(p)))
                .unwrap_or_else(|| PathBuf::from("/bin/sh"));
            let mut profile = ShellProfile::new(program);
            if cfg!(target_os = "macos") {
                profile.args.push("-l".to_owned());
            }
            profile
        }
    }

    /// The shell family, from the program's file name.
    pub fn kind(&self) -> ShellKind {
        // Split on both separators so Windows paths classify on any host.
        let program = self.program.to_string_lossy().to_ascii_lowercase();
        let file = program.rsplit(['/', '\\']).next().unwrap_or(&program);
        let stem = file.strip_suffix(".exe").unwrap_or(file);
        match stem.trim_start_matches('-') {
            "bash" => ShellKind::Bash,
            "zsh" => ShellKind::Zsh,
            "fish" => ShellKind::Fish,
            "pwsh" | "powershell" | "pwsh-preview" => ShellKind::PowerShell,
            "cmd" => ShellKind::Cmd,
            "sh" | "dash" | "ksh" | "mksh" | "ash" | "yash" => ShellKind::Posix,
            _ => ShellKind::Other,
        }
    }
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Looks `name` up on `PATH` (and `PATHEXT` on Windows when `name` has no extension).
pub fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    let exts: Vec<String> = if cfg!(windows) && Path::new(name).extension().is_none() {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned())
            .split(';')
            .map(str::to_owned)
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(&path) {
        for ext in &exts {
            let candidate = dir.join(format!("{name}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

/// Options for spawning a terminal session.
#[derive(Debug, Clone)]
pub struct SpawnOptions {
    /// What to run.
    pub profile: ShellProfile,
    /// Starting directory (default: the current directory of Lector).
    pub cwd: Option<PathBuf>,
    /// Extra environment, applied after the profile's.
    pub env: Vec<(String, String)>,
    /// Initial size.
    pub size: TermSize,
    /// Scrollback lines kept (default 10 000).
    pub scrollback: usize,
    /// Directory to install and inject shell integration from (opt-in; `None` = off).
    pub shell_integration: Option<PathBuf>,
    /// Variables removed from the inherited environment (see [`DEFAULT_ENV_SCRUB`]).
    pub env_scrub: Vec<String>,
    /// Scroll the view to the bottom when input is sent (default true).
    pub scroll_to_bottom_on_input: bool,
}

impl SpawnOptions {
    /// Options for `profile` with defaults for everything else.
    pub fn new(profile: ShellProfile) -> Self {
        Self {
            profile,
            cwd: None,
            env: Vec::new(),
            size: TermSize::default(),
            scrollback: crate::DEFAULT_SCROLLBACK,
            shell_integration: None,
            env_scrub: DEFAULT_ENV_SCRUB.iter().map(|s| (*s).to_owned()).collect(),
            scroll_to_bottom_on_input: true,
        }
    }
}

impl Default for SpawnOptions {
    fn default() -> Self {
        Self::new(ShellProfile::default_for_platform())
    }
}

/// Inherited variables removed before spawning. A trailing `*` matches a prefix.
///
/// Lector's own `LECTOR_*` settings (this includes the askpass endpoint, token and
/// operation id), the askpass hooks git and ssh would otherwise use instead of prompting on
/// the terminal, stale size hints, and the identity of whatever terminal Lector itself was
/// started from (which would mislead programs about the terminal they run in).
///
/// Invariant: every name in `lector_process::askpass::SCRUB_ENV` is matched here (this crate
/// does not depend on `lector-process`; `tests/scrub.rs` checks it).
pub const DEFAULT_ENV_SCRUB: &[&str] = &[
    "LECTOR_*",
    "GIT_ASKPASS",
    "SSH_ASKPASS",
    "SSH_ASKPASS_REQUIRE",
    "GIT_TERMINAL_PROMPT",
    "COLUMNS",
    "LINES",
    "TERMCAP",
    "TERM_SESSION_ID",
    "ITERM_SESSION_ID",
    "ITERM_PROFILE",
    "KITTY_WINDOW_ID",
    "KITTY_PID",
    "KITTY_INSTALLATION_DIR",
    "ALACRITTY_SOCKET",
    "ALACRITTY_LOG",
    "ALACRITTY_WINDOW_ID",
    "WEZTERM_*",
    "VTE_VERSION",
    "WT_SESSION",
    "WT_PROFILE_ID",
    "GHOSTTY_*",
    "KONSOLE_*",
    "VSCODE_*",
];

/// Whether `name` matches one of the scrub `patterns` (trailing `*` = prefix).
pub fn env_is_scrubbed(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| match p.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None if cfg!(windows) => name.eq_ignore_ascii_case(p),
        None => name == p,
    })
}

/// Variables every session gets.
pub fn terminal_env() -> Vec<(String, String)> {
    vec![
        ("TERM".to_owned(), "xterm-256color".to_owned()),
        ("COLORTERM".to_owned(), "truecolor".to_owned()),
        ("TERM_PROGRAM".to_owned(), "Lector".to_owned()),
        (
            "TERM_PROGRAM_VERSION".to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]
}

/// bash integration snippet (OSC 7 + OSC 133 A/B/C/D).
pub const BASH_INTEGRATION: &str = r#"# Lector shell integration for bash: reports the working directory (OSC 7)
# and prompt/command boundaries (OSC 133). Safe to source more than once.
if [[ $- == *i* && -z "${__lector_si_loaded:-}" ]]; then
__lector_si_loaded=1
__lector_at_prompt=0
__lector_ran=0
__lector_urlencode() {
  local LC_ALL=C s="$1" out="" c i
  for (( i = 0; i < ${#s}; i++ )); do
    c="${s:i:1}"
    case "$c" in
      [a-zA-Z0-9/._~-]) out+="$c" ;;
      *) printf -v c '%%%02X' "'$c"; out+="$c" ;;
    esac
  done
  printf '%s' "$out"
}
__lector_precmd() {
  local st=$?
  if [[ $__lector_ran == 1 ]]; then printf '\e]133;D;%s\a' "$st"; fi
  __lector_ran=0
  printf '\e]7;file://%s%s\a' "${HOSTNAME:-}" "$(__lector_urlencode "$PWD")"
  return $st
}
__lector_postcmd() {
  local st=$?
  if [[ $PS1 != *'133;A'* ]]; then PS1='\[\e]133;A\a\]'"$PS1"; fi
  if [[ $PS1 != *'133;B'* ]]; then PS1="$PS1"'\[\e]133;B\a\]'; fi
  __lector_at_prompt=1
  return $st
}
__lector_preexec() {
  if [[ $__lector_at_prompt == 1 ]]; then
    __lector_at_prompt=0
    __lector_ran=1
    printf '\e]133;C\a'
  fi
}
PROMPT_COMMAND="__lector_precmd${PROMPT_COMMAND:+;$PROMPT_COMMAND};__lector_postcmd"
if [[ -z "$(trap -p DEBUG)" ]]; then trap '__lector_preexec' DEBUG; fi
fi
"#;

/// zsh integration snippet (OSC 7 + OSC 133 A/B/C/D).
pub const ZSH_INTEGRATION: &str = r#"# Lector shell integration for zsh: reports the working directory (OSC 7)
# and prompt/command boundaries (OSC 133). Safe to source more than once.
if [[ -o interactive && -z "${__lector_si_loaded:-}" ]]; then
typeset -g __lector_si_loaded=1 __lector_ran=0
__lector_urlencode() {
  emulate -L zsh
  local LC_ALL=C s="$1" out="" c i
  for (( i = 1; i <= ${#s}; i++ )); do
    c="${s[i]}"
    if [[ "$c" == [a-zA-Z0-9/._~-] ]]; then out+="$c"; else out+="$(printf '%%%02X' "'$c")"; fi
  done
  print -rn -- "$out"
}
__lector_precmd() {
  local st=$?
  (( __lector_ran )) && printf '\e]133;D;%s\a' "$st"
  __lector_ran=0
  printf '\e]7;file://%s%s\a' "${HOST:-}" "$(__lector_urlencode "$PWD")"
  [[ $PS1 != *'133;A'* ]] && PS1=$'%{\e]133;A\a%}'"$PS1"
  [[ $PS1 != *'133;B'* ]] && PS1="$PS1"$'%{\e]133;B\a%}'
  return $st
}
__lector_preexec() {
  __lector_ran=1
  printf '\e]133;C\a'
}
typeset -ga precmd_functions preexec_functions
precmd_functions=(__lector_precmd $precmd_functions)
preexec_functions+=(__lector_preexec)
fi
"#;

/// fish integration snippet (OSC 7 + OSC 133 A/C/D).
pub const FISH_INTEGRATION: &str = r#"# Lector shell integration for fish: reports the working directory (OSC 7)
# and prompt/command boundaries (OSC 133). Safe to source more than once.
if set -q __LECTOR_FISH_XDG
    # Undo the XDG_DATA_DIRS entry Lector used to load this file.
    set -l dirs (string split : -- "$XDG_DATA_DIRS")
    set -l keep
    for d in $dirs
        test "$d" = "$__LECTOR_FISH_XDG"; or set -a keep $d
    end
    if test (count $keep) -gt 0
        set -gx XDG_DATA_DIRS (string join : -- $keep)
    else
        set -e XDG_DATA_DIRS
    end
    set -e __LECTOR_FISH_XDG
end
if status is-interactive; and not set -q __lector_si_loaded
    set -g __lector_si_loaded 1
    function __lector_osc7 --on-variable PWD
        printf '\e]7;file://%s%s\a' (hostname) (string escape --style=url -- "$PWD")
    end
    function __lector_prompt --on-event fish_prompt
        printf '\e]133;A\a'
    end
    function __lector_preexec --on-event fish_preexec
        printf '\e]133;C\a'
    end
    function __lector_postexec --on-event fish_postexec
        printf '\e]133;D;%s\a' $status
    end
    __lector_osc7
end
"#;

/// PowerShell integration snippet (not injected automatically; dot-source it from `$PROFILE`).
pub const POWERSHELL_INTEGRATION: &str = r#"# Lector shell integration for PowerShell: reports the working directory
# (OSC 7) and prompt/command boundaries (OSC 133). Dot-source from $PROFILE.
if (-not $global:__LectorSiLoaded) {
    $global:__LectorSiLoaded = $true
    $global:__LectorRan = $false
    $global:__LectorOrigPrompt = $function:prompt
    function global:prompt {
        $ok = $?
        $code = $global:LASTEXITCODE
        $e = [char]27; $b = [char]7
        $out = ""
        if ($global:__LectorRan) {
            $ec = if ($ok) { 0 } elseif ($code) { $code } else { 1 }
            $out += "$e]133;D;$ec$b"
        }
        $global:__LectorRan = $false
        $loc = $executionContext.SessionState.Path.CurrentLocation
        if ($loc.Provider.Name -eq 'FileSystem') {
            $p = [uri]::EscapeDataString(($loc.ProviderPath -replace '\\', '/'))
            $out += "$e]7;file://$env:COMPUTERNAME/$p$b"
        }
        $out += "$e]133;A$b"
        $out += & $global:__LectorOrigPrompt
        $out += "$e]133;B$b"
        $global:LASTEXITCODE = $code
        return $out
    }
    if (Get-Module PSReadLine) {
        Set-PSReadLineKeyHandler -Chord Enter -ScriptBlock {
            [Microsoft.PowerShell.PSConsoleReadLine]::AcceptLine()
            $global:__LectorRan = $true
            [Console]::Write("$([char]27)]133;C$([char]7)")
        }
    }
}
"#;

const BASH_RCFILE: &str = r#"# Lector bash startup: loads the usual interactive startup files, then Lector's
# shell integration. Generated by Lector; your own files are not modified.
if [ -f /etc/bash.bashrc ]; then . /etc/bash.bashrc; fi
if [ -f /etc/bashrc ]; then . /etc/bashrc; fi
if [ -f "$HOME/.bashrc" ]; then . "$HOME/.bashrc"; fi
__lector_dir="${BASH_SOURCE[0]%/*}"
if [ -f "$__lector_dir/lector.bash" ]; then . "$__lector_dir/lector.bash"; fi
unset __lector_dir
"#;

const ZSH_ZSHENV: &str = r#"# Lector zsh shim: restores your ZDOTDIR, loads your .zshenv, then Lector's
# shell integration. Generated by Lector; your own files are not modified.
__lector_dir="${${(%):-%x}:A:h}"
if [[ -n "${__LECTOR_ZDOTDIR+x}" ]]; then
  export ZDOTDIR="$__LECTOR_ZDOTDIR"
else
  unset ZDOTDIR
fi
unset __LECTOR_ZDOTDIR
if [[ -f "${ZDOTDIR:-$HOME}/.zshenv" ]]; then builtin source "${ZDOTDIR:-$HOME}/.zshenv"; fi
if [[ -o interactive && -f "$__lector_dir/lector.zsh" ]]; then builtin source "$__lector_dir/lector.zsh"; fi
unset __lector_dir
"#;

fn write_if_changed(path: &Path, content: &str) -> Result<(), TerminalError> {
    if std::fs::read(path).ok().as_deref() == Some(content.as_bytes()) {
        return Ok(());
    }
    let err = |source| TerminalError::Integration {
        path: path.to_path_buf(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(err)?;
    }
    std::fs::write(path, content).map_err(err)
}

/// Writes the integration files under `dir` (only files whose content differs).
pub fn install_shell_integration(dir: &Path) -> Result<(), TerminalError> {
    write_if_changed(&dir.join("bash").join("lector.bash"), BASH_INTEGRATION)?;
    write_if_changed(&dir.join("bash").join("rcfile"), BASH_RCFILE)?;
    write_if_changed(&dir.join("zsh").join("lector.zsh"), ZSH_INTEGRATION)?;
    write_if_changed(&dir.join("zsh").join(".zshenv"), ZSH_ZSHENV)?;
    write_if_changed(
        &dir.join("fish").join("vendor_conf.d").join("lector.fish"),
        FISH_INTEGRATION,
    )?;
    write_if_changed(
        &dir.join("powershell").join("lector.ps1"),
        POWERSHELL_INTEGRATION,
    )?;
    Ok(())
}

/// How a spawn is changed to inject integration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Injection {
    /// Arguments appended to the profile's.
    pub args: Vec<String>,
    /// Variables to set.
    pub env: Vec<(String, String)>,
}

/// Plans the least invasive injection for `profile` from files installed in `dir`.
/// `None` when the shell has no clean mechanism.
pub(crate) fn plan_injection(profile: &ShellProfile, dir: &Path) -> Option<Injection> {
    match profile.kind() {
        ShellKind::Zsh => {
            let mut env = vec![(
                "ZDOTDIR".to_owned(),
                dir.join("zsh").to_string_lossy().into_owned(),
            )];
            if let Ok(user) = std::env::var("ZDOTDIR") {
                env.push(("__LECTOR_ZDOTDIR".to_owned(), user));
            }
            Some(Injection {
                args: Vec::new(),
                env,
            })
        }
        ShellKind::Fish => {
            let ours = dir.to_string_lossy().into_owned();
            let existing = std::env::var("XDG_DATA_DIRS")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
            Some(Injection {
                args: Vec::new(),
                env: vec![
                    ("XDG_DATA_DIRS".to_owned(), format!("{ours}:{existing}")),
                    ("__LECTOR_FISH_XDG".to_owned(), ours),
                ],
            })
        }
        ShellKind::Bash if profile.args.is_empty() => Some(Injection {
            args: vec![
                "--rcfile".to_owned(),
                dir.join("bash")
                    .join("rcfile")
                    .to_string_lossy()
                    .into_owned(),
            ],
            env: Vec::new(),
        }),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(ShellProfile::new("/bin/zsh").kind(), ShellKind::Zsh);
        assert_eq!(
            ShellProfile::new("/usr/local/bin/fish").kind(),
            ShellKind::Fish
        );
        assert_eq!(
            ShellProfile::new(r"C:\x\pwsh.exe").kind(),
            ShellKind::PowerShell
        );
        assert_eq!(ShellProfile::new("cmd.exe").kind(), ShellKind::Cmd);
        assert_eq!(ShellProfile::new("/bin/dash").kind(), ShellKind::Posix);
        assert_eq!(
            ShellProfile::new("/usr/bin/python3").kind(),
            ShellKind::Other
        );
        assert_eq!(ShellProfile::new("/bin/bash").name, "bash");
    }

    #[test]
    fn default_profile_exists() {
        let p = ShellProfile::default_for_platform();
        assert!(!p.name.is_empty());
        #[cfg(unix)]
        assert!(p.program.is_absolute());
    }

    #[test]
    fn scrub_patterns() {
        let pats: Vec<String> = DEFAULT_ENV_SCRUB.iter().map(|s| (*s).to_owned()).collect();
        assert!(env_is_scrubbed("LECTOR_DATA_DIR", &pats));
        assert!(env_is_scrubbed("COLUMNS", &pats));
        assert!(env_is_scrubbed("WEZTERM_PANE", &pats));
        for askpass in [
            "GIT_ASKPASS",
            "SSH_ASKPASS",
            "SSH_ASKPASS_REQUIRE",
            "GIT_TERMINAL_PROMPT",
            "LECTOR_ASKPASS_TOKEN",
        ] {
            assert!(env_is_scrubbed(askpass, &pats), "{askpass}");
        }
        assert!(!env_is_scrubbed("HOME", &pats));
        assert!(!env_is_scrubbed("PATH", &pats));
    }

    #[test]
    fn install_and_plan() {
        let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
        install_shell_integration(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        // Idempotent.
        install_shell_integration(dir.path()).unwrap_or_else(|e| panic!("{e}"));
        assert!(dir.path().join("zsh/.zshenv").is_file());
        assert!(dir.path().join("fish/vendor_conf.d/lector.fish").is_file());
        let bash = plan_injection(&ShellProfile::new("/bin/bash"), dir.path());
        assert_eq!(bash.map(|i| i.args.len()), Some(2));
        let mut login = ShellProfile::new("/bin/bash");
        login.args.push("-l".into());
        assert_eq!(plan_injection(&login, dir.path()), None);
        let zsh = plan_injection(&ShellProfile::new("/bin/zsh"), dir.path());
        assert!(zsh.is_some_and(|i| i.env.iter().any(|(k, _)| k == "ZDOTDIR")));
        assert_eq!(plan_injection(&ShellProfile::new("pwsh"), dir.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn snippets_parse() {
        // Syntax-check the snippets with the shells that are installed.
        for (shell, flag, snippet) in [
            ("bash", "-n", BASH_INTEGRATION),
            ("bash", "-n", BASH_RCFILE),
            ("zsh", "-n", ZSH_INTEGRATION),
            ("zsh", "-n", ZSH_ZSHENV),
            ("fish", "--no-execute", FISH_INTEGRATION),
        ] {
            let Some(path) = find_on_path(shell) else {
                continue;
            };
            let dir = tempfile::tempdir().unwrap_or_else(|e| panic!("{e}"));
            let file = dir.path().join("snippet");
            std::fs::write(&file, snippet).unwrap_or_else(|e| panic!("{e}"));
            let out = std::process::Command::new(path)
                .arg(flag)
                .arg(&file)
                .output();
            let out = out.unwrap_or_else(|e| panic!("{shell}: {e}"));
            assert!(
                out.status.success(),
                "{shell}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}
