//! Which agents can be started: the user's `[[agent.servers]]` plus known ACP agents
//! found on `PATH`.

use std::path::{Path, PathBuf};

use lector_config::AgentSettings;
use serde::{Deserialize, Serialize};

/// How to start one agent (settings `[[agent.servers]]`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Name shown in the picker.
    pub name: String,
    /// Program (a name looked up on `PATH`, or a path).
    pub command: String,
    /// Arguments.
    pub args: Vec<String>,
    /// Extra environment variables.
    pub env: Vec<(String, String)>,
}

impl AgentConfig {
    /// A config without environment variables.
    pub fn new(name: impl Into<String>, command: impl Into<String>, args: &[&str]) -> Self {
        AgentConfig {
            name: name.into(),
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            env: Vec::new(),
        }
    }

    /// The command line as one string (for display).
    pub fn command_line(&self) -> String {
        std::iter::once(self.command.as_str())
            .chain(self.args.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }
}

/// Known ACP agents: (name, program, args).
pub const KNOWN_AGENTS: &[(&str, &str, &[&str])] = &[
    ("Gemini CLI", "gemini", &["--experimental-acp"]),
    ("Claude Code", "claude-code-acp", &[]),
    ("Codex", "codex-acp", &[]),
];

/// Agents to offer: the user's servers first, then known agents found on `PATH` whose
/// program is not configured already. Pass the *user* settings (`load_settings_for(None)`)
/// — a project's settings must not be able to add commands.
pub fn available_configs(settings: &AgentSettings) -> Vec<AgentConfig> {
    available_configs_with(settings, std::env::var_os("PATH").as_deref())
}

/// [`available_configs`] with an explicit `PATH` value (tests).
pub fn available_configs_with(
    settings: &AgentSettings,
    path_var: Option<&std::ffi::OsStr>,
) -> Vec<AgentConfig> {
    let mut out: Vec<AgentConfig> = settings
        .servers
        .iter()
        .filter(|s| !s.command.trim().is_empty())
        .map(|s| AgentConfig {
            name: if s.name.trim().is_empty() {
                s.command.clone()
            } else {
                s.name.clone()
            },
            command: s.command.clone(),
            args: s.args.clone(),
            env: s.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        })
        .collect();
    for (name, program, args) in KNOWN_AGENTS {
        let configured = out.iter().any(|c| {
            Path::new(&c.command)
                .file_stem()
                .is_some_and(|stem| stem == *program)
        });
        if configured {
            continue;
        }
        if let Some(found) = find_on_path(program, path_var) {
            out.push(AgentConfig {
                name: (*name).into(),
                command: found.to_string_lossy().into_owned(),
                args: args.iter().map(|s| s.to_string()).collect(),
                env: Vec::new(),
            });
        }
    }
    out
}

/// Finds an executable on `PATH` (with `PATHEXT` extensions on Windows).
pub fn find_on_path(program: &str, path_var: Option<&std::ffi::OsStr>) -> Option<PathBuf> {
    let path_var = path_var?;
    let exts: Vec<String> = if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".EXE;.CMD;.BAT;.COM".into())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.to_ascii_lowercase())
            .collect()
    } else {
        vec![String::new()]
    };
    for dir in std::env::split_paths(path_var) {
        for ext in &exts {
            let candidate = dir.join(format!("{program}{ext}"));
            if is_executable(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lector_config::AgentServer;

    #[test]
    fn user_servers_first_then_known_agents_on_path() {
        let dir = tempfile::tempdir().unwrap();
        let gemini = dir.path().join(if cfg!(windows) {
            "gemini.exe"
        } else {
            "gemini"
        });
        std::fs::write(&gemini, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&gemini, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut settings = AgentSettings::default();
        settings.servers.push(AgentServer {
            name: "Mine".into(),
            command: "my-agent".into(),
            args: vec!["--acp".into()],
            env: [("K".to_string(), "V".to_string())].into_iter().collect(),
        });
        let path = std::env::join_paths([dir.path()]).unwrap();
        let configs = available_configs_with(&settings, Some(&path));
        assert_eq!(configs.len(), 2, "{configs:?}");
        assert_eq!(configs[0].name, "Mine");
        assert_eq!(configs[0].env, vec![("K".into(), "V".into())]);
        assert_eq!(configs[1].name, "Gemini CLI");
        assert_eq!(configs[1].args, vec!["--experimental-acp".to_string()]);
        // A configured gemini replaces the detected one.
        settings.servers.push(AgentServer {
            name: "G".into(),
            command: "gemini".into(),
            ..Default::default()
        });
        assert_eq!(available_configs_with(&settings, Some(&path)).len(), 2);
        assert!(available_configs_with(&AgentSettings::default(), None).is_empty());
    }
}
