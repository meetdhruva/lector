use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

/// What a supervised process is, for listings and policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ProcessKind {
    /// A `git` invocation (network operations, reads, writes).
    Git,
    /// A language server.
    LanguageServer,
    /// An agent integration.
    Agent,
    /// An interactive terminal.
    Terminal,
    /// A short-lived helper (askpass, formatters, ...).
    Helper,
    /// Anything else.
    Other,
}

impl ProcessKind {
    /// Whether children of this kind lose the askpass variables
    /// ([`crate::askpass::SCRUB_ENV`]) inherited from Lector's environment: language
    /// servers, agents and terminals are not git network operations. Variables set
    /// explicitly with [`ProcessCommand::env`] are kept.
    pub fn scrubs_askpass_env(self) -> bool {
        matches!(
            self,
            ProcessKind::LanguageServer | ProcessKind::Agent | ProcessKind::Terminal
        )
    }

    /// Stable lowercase name (`git`, `language_server`, ...).
    pub fn as_str(self) -> &'static str {
        match self {
            ProcessKind::Git => "git",
            ProcessKind::LanguageServer => "language_server",
            ProcessKind::Agent => "agent",
            ProcessKind::Terminal => "terminal",
            ProcessKind::Helper => "helper",
            ProcessKind::Other => "other",
        }
    }
}

/// Standard input of a spawned process.
#[derive(Clone, Default)]
pub enum StdinMode {
    /// `/dev/null` (the default): the process sees EOF at once.
    #[default]
    Null,
    /// A pipe the caller writes to ([`ChildHandle::take_stdin`](crate::ChildHandle::take_stdin)).
    Piped,
    /// These bytes are written on a helper thread, then stdin is closed.
    Bytes(Vec<u8>),
}

impl std::fmt::Debug for StdinMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StdinMode::Null => f.write_str("Null"),
            StdinMode::Piped => f.write_str("Piped"),
            // May carry secrets (patches, messages); only the length is shown.
            StdinMode::Bytes(b) => write!(f, "Bytes({} bytes)", b.len()),
        }
    }
}

/// Standard output or error of a spawned process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OutputMode {
    /// A pipe (the default); read it raw or through line events.
    #[default]
    Piped,
    /// Discarded.
    Null,
    /// Inherited from Lector (logs, debugging).
    Inherit,
}

/// A command to run under supervision: program, arguments, working directory,
/// environment changes, stdio and how its process tree is handled.
///
/// Arguments are passed as separate argv entries, never through a shell. `Debug` output
/// shows environment variable *names* only (values may be tokens).
#[derive(Clone)]
pub struct ProcessCommand {
    pub(crate) program: OsString,
    pub(crate) args: Vec<OsString>,
    pub(crate) cwd: Option<PathBuf>,
    pub(crate) env: Vec<(OsString, OsString)>,
    pub(crate) env_remove: Vec<OsString>,
    pub(crate) env_clear: bool,
    pub(crate) stdin: StdinMode,
    pub(crate) stdout: OutputMode,
    pub(crate) stderr: OutputMode,
    pub(crate) label: String,
    pub(crate) kind: ProcessKind,
    pub(crate) new_session: bool,
    pub(crate) kill_tree_on_exit: bool,
    pub(crate) console_window: bool,
}

impl std::fmt::Debug for ProcessCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessCommand")
            .field("program", &self.program)
            .field("args", &self.args)
            .field("cwd", &self.cwd)
            .field("env", &self.env.iter().map(|(k, _)| k).collect::<Vec<_>>())
            .field("env_remove", &self.env_remove)
            .field("stdin", &self.stdin)
            .field("label", &self.label)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl ProcessCommand {
    /// A command for `program` (looked up on `PATH` unless it is a path). Defaults: no
    /// arguments, Lector's working directory and environment, stdin `/dev/null`, stdout
    /// and stderr piped, [`ProcessKind::Other`], a new session / job, the whole tree is
    /// killed when the main process exits, no console window on Windows.
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        let program = program.as_ref().to_os_string();
        ProcessCommand {
            label: program.to_string_lossy().into_owned(),
            program,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            env_clear: false,
            stdin: StdinMode::Null,
            stdout: OutputMode::Piped,
            stderr: OutputMode::Piped,
            kind: ProcessKind::Other,
            new_session: true,
            kill_tree_on_exit: true,
            console_window: false,
        }
    }

    /// Appends one argument.
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.args.push(arg.as_ref().to_os_string());
        self
    }

    /// Appends arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.args
            .extend(args.into_iter().map(|a| a.as_ref().to_os_string()));
        self
    }

    /// Working directory.
    pub fn cwd(mut self, dir: impl AsRef<Path>) -> Self {
        self.cwd = Some(dir.as_ref().to_path_buf());
        self
    }

    /// Sets an environment variable (later calls win).
    pub fn env(mut self, key: impl AsRef<OsStr>, value: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        self.env_remove.retain(|k| *k != key);
        self.env.push((key, value.as_ref().to_os_string()));
        self
    }

    /// Sets several environment variables.
    pub fn envs<I, K, V>(mut self, vars: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (k, v) in vars {
            self = self.env(k, v);
        }
        self
    }

    /// Removes an environment variable inherited from Lector.
    pub fn env_remove(mut self, key: impl AsRef<OsStr>) -> Self {
        let key = key.as_ref().to_os_string();
        self.env.retain(|(k, _)| *k != key);
        self.env_remove.push(key);
        self
    }

    /// Starts from an empty environment (then [`env`](Self::env) adds to it).
    pub fn env_clear(mut self) -> Self {
        self.env_clear = true;
        self
    }

    /// Standard input.
    pub fn stdin(mut self, mode: StdinMode) -> Self {
        self.stdin = mode;
        self
    }

    /// Standard output (runs always pipe it).
    pub fn stdout(mut self, mode: OutputMode) -> Self {
        self.stdout = mode;
        self
    }

    /// Standard error (runs always pipe it).
    pub fn stderr(mut self, mode: OutputMode) -> Self {
        self.stderr = mode;
        self
    }

    /// Human label for listings ("git fetch origin", "rust-analyzer").
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = label.into();
        self
    }

    /// What the process is.
    pub fn kind(mut self, kind: ProcessKind) -> Self {
        self.kind = kind;
        self
    }

    /// Unix: `true` (default) starts the process in a new session (`setsid`): it has no
    /// controlling terminal, so ssh/git cannot prompt on the terminal Lector was started
    /// from and use askpass instead. `false` only starts a new process group (keeps the
    /// terminal, e.g. for a tty pinentry). Either way the whole tree can be signalled.
    /// Ignored on Windows (a Job Object is always used).
    pub fn new_session(mut self, yes: bool) -> Self {
        self.new_session = yes;
        self
    }

    /// `true` (default): when the main process exits, whatever it left running in its
    /// process group / job is killed. `false` lets intentional daemons survive (git's
    /// credential-cache daemon, ssh `ControlPersist` masters); they are still killed when
    /// the main process is cancelled, times out or its handle is dropped while it runs.
    pub fn kill_tree_on_exit(mut self, yes: bool) -> Self {
        self.kill_tree_on_exit = yes;
        self
    }

    /// Windows: allow a console window (interactive programs). Default `false`:
    /// `CREATE_NO_WINDOW`, so non-interactive children never flash a console. Ignored elsewhere.
    pub fn console_window(mut self, yes: bool) -> Self {
        self.console_window = yes;
        self
    }

    /// The program.
    pub fn get_program(&self) -> &OsStr {
        &self.program
    }

    /// The arguments.
    pub fn get_args(&self) -> &[OsString] {
        &self.args
    }

    /// The label.
    pub fn get_label(&self) -> &str {
        &self.label
    }

    /// The kind.
    pub fn get_kind(&self) -> ProcessKind {
        self.kind
    }

    /// The configured value of an environment variable set with [`env`](Self::env).
    pub fn get_env(&self, key: impl AsRef<OsStr>) -> Option<&OsStr> {
        let key = key.as_ref();
        self.env
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_os_str())
    }

    /// The std command (stdio is configured by the spawner).
    pub(crate) fn to_std(&self) -> std::process::Command {
        let mut cmd = std::process::Command::new(&self.program);
        cmd.args(&self.args);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        if self.env_clear {
            cmd.env_clear();
        }
        for key in &self.env_remove {
            cmd.env_remove(key);
        }
        if self.kind.scrubs_askpass_env() {
            for key in crate::askpass::SCRUB_ENV {
                cmd.env_remove(key);
            }
        }
        for (k, v) in &self.env {
            cmd.env(k, v);
        }
        cmd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_hides_env_values_and_stdin_bytes() {
        let cmd = ProcessCommand::new("git")
            .args(["fetch", "origin"])
            .env("GIT_ASKPASS_TOKEN", "s3cret")
            .stdin(StdinMode::Bytes(b"password".to_vec()));
        let text = format!("{cmd:?}");
        assert!(text.contains("GIT_ASKPASS_TOKEN"));
        assert!(!text.contains("s3cret"));
        assert!(!text.contains("password\""));
        assert!(text.contains("8 bytes"));
    }

    #[test]
    fn env_and_env_remove_override_each_other() {
        let cmd = ProcessCommand::new("x")
            .env("A", "1")
            .env_remove("A")
            .env("B", "1")
            .env("B", "2");
        assert_eq!(cmd.get_env("A"), None);
        assert_eq!(cmd.get_env("B"), Some(OsStr::new("2")));
        assert_eq!(cmd.env_remove, vec![OsString::from("A")]);
        let cmd = cmd.env("A", "3");
        assert!(cmd.env_remove.is_empty());
        assert_eq!(cmd.get_env("A"), Some(OsStr::new("3")));
    }

    #[test]
    fn non_git_kinds_drop_inherited_askpass_variables() {
        let removed = |kind: ProcessKind| {
            let std = ProcessCommand::new("x")
                .kind(kind)
                .env("LECTOR_ASKPASS_TOKEN", "explicit")
                .to_std();
            let envs: Vec<(OsString, Option<OsString>)> = std
                .get_envs()
                .map(|(k, v)| (k.to_os_string(), v.map(OsStr::to_os_string)))
                .collect();
            let gone = |name: &str| {
                envs.iter()
                    .any(|(k, v)| k == OsStr::new(name) && v.is_none())
            };
            let explicit = envs.iter().any(|(k, v)| {
                k == "LECTOR_ASKPASS_TOKEN" && v.as_deref() == Some(OsStr::new("explicit"))
            });
            (
                gone("GIT_ASKPASS") && gone("SSH_ASKPASS") && gone("LECTOR_ASKPASS_ENDPOINT"),
                explicit,
            )
        };
        for kind in [
            ProcessKind::Terminal,
            ProcessKind::LanguageServer,
            ProcessKind::Agent,
        ] {
            assert_eq!(removed(kind), (true, true), "{kind:?}");
        }
        for kind in [ProcessKind::Git, ProcessKind::Helper, ProcessKind::Other] {
            assert_eq!(removed(kind), (false, true), "{kind:?}");
        }
    }
}
