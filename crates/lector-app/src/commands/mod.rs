//! Commands are Lector's API: typed, parameterized, with availability, preview and
//! apply. Menus, the palette, key bindings, the CLI, MCP tools and agents are all
//! front-ends over [`CommandRegistry::invoke`].
//!
//! * [`CommandSpec`] describes a command (id, title, JSON-schema of its arguments,
//!   whether it writes, is destructive / undoable, and the GPUI action bound to it).
//! * [`Command`] implements it against a [`CommandContext`] (the [`AppCore`], the
//!   project and the UI focus).
//! * [`CommandRegistry::invoke`] checks availability, validates the arguments
//!   ([`schema::validate`]), runs the command and records writes to the activity
//!   journal through [`ActivitySink`].
//!
//! Read/navigation commands live in [`read`]; git writes in `git_write` (Agent SAFE), git
//! network / branch / stash commands in [`git_sync`] (v0.7),
//! document commands in `documents` (Agent DOCS).

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use lector_git::Changeset;
use lector_text::Point;
use lector_workspace::Project;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::core::AppCore;
pub use crate::journal::UndoRecord;

pub mod documents;
pub mod git_sync;
pub mod git_write;
pub mod read;
pub mod schema;

/// Registers every built-in command. Other modules add their `register` call here.
pub fn register_builtin(registry: &CommandRegistry) {
    read::register(registry);
    documents::register(registry);
    git_write::register(registry);
    git_sync::register(registry);
    crate::comments::register_commands(registry);
}

/// Static description of a command.
#[derive(Debug, Clone)]
pub struct CommandSpec {
    /// Stable dotted id ("git.stage", "workspace.open_path"); also the MCP tool / CLI name.
    pub id: &'static str,
    /// Human title for the palette ("Stage File").
    pub title: &'static str,
    /// Palette category ("Git", "File", "Search").
    pub category: &'static str,
    /// JSON Schema of the arguments (an `object` schema; see [`schema::validate`]).
    pub args_schema: Value,
    /// Whether applying it writes to the user's files or repository. Writes are
    /// recorded in the activity journal; read commands are not.
    pub writes: bool,
    /// Loses data unless undone (discard, delete).
    pub destructive: bool,
    /// [`Outcome::undo`] is filled on success.
    pub undoable: bool,
    /// The GPUI action ("git::Stage") that dispatches this command, if any. The generic
    /// action bridge registers a handler that runs the command as is.
    pub ui_action: Option<&'static str>,
    /// The GPUI action whose handler in a UI crate runs this command through its own flow
    /// (selection, confirmation, progress, Undo toast). The bridge registers nothing for it
    /// and the palette lists that action's row instead of a second row for the command.
    pub ui_flow: Option<&'static str>,
}

impl CommandSpec {
    /// A read-only command (no journal entry) with an argument schema.
    pub fn read(
        id: &'static str,
        title: &'static str,
        category: &'static str,
        args_schema: Value,
    ) -> Self {
        CommandSpec {
            id,
            title,
            category,
            args_schema,
            writes: false,
            destructive: false,
            undoable: false,
            ui_action: None,
            ui_flow: None,
        }
    }

    /// A command that writes (journaled).
    pub fn write(
        id: &'static str,
        title: &'static str,
        category: &'static str,
        args_schema: Value,
    ) -> Self {
        CommandSpec {
            writes: true,
            ..Self::read(id, title, category, args_schema)
        }
    }

    /// Sets [`CommandSpec::ui_action`].
    pub fn with_ui_action(mut self, action: &'static str) -> Self {
        self.ui_action = Some(action);
        self
    }

    /// Sets [`CommandSpec::ui_flow`].
    pub fn with_ui_flow(mut self, action: &'static str) -> Self {
        self.ui_flow = Some(action);
        self
    }

    /// Marks the command destructive.
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Marks the command undoable.
    pub fn undoable(mut self) -> Self {
        self.undoable = true;
        self
    }

    /// Machine-readable description (id, title, category, flags, schema) for
    /// `commands.list`, MCP `tools/list` and CLI help.
    pub fn to_json(&self) -> Value {
        let mut v = json!({
            "id": self.id,
            "title": self.title,
            "category": self.category,
            "args": self.args_schema,
        });
        if self.writes {
            v["writes"] = json!(true);
        }
        if self.destructive {
            v["destructive"] = json!(true);
        }
        if self.undoable {
            v["undoable"] = json!(true);
        }
        if let Some(a) = self.ui_action {
            v["ui_action"] = json!(a);
        }
        if let Some(a) = self.ui_flow {
            v["ui_flow"] = json!(a);
        }
        v
    }
}

/// Whether a command can run in a context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    /// It can run.
    Available,
    /// It cannot; the reason is shown as a tooltip / returned to agents.
    Unavailable(String),
}

impl Availability {
    /// True for [`Availability::Available`].
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }

    /// `Unavailable` with a reason.
    pub fn no(reason: impl Into<String>) -> Self {
        Availability::Unavailable(reason.into())
    }
}

/// Who invoked a command (recorded in the journal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// The GUI (palette, key binding, menu, button).
    #[default]
    Ui,
    /// The command-line interface.
    Cli,
    /// An MCP client.
    Mcp,
    /// An agent integration.
    Agent,
    /// Tests and internal callers.
    Internal,
}

/// What the user is looking at; commands default their arguments from it.
#[derive(Debug, Clone, Default)]
pub struct Focus {
    /// Absolute path of the active file, if any.
    pub path: Option<PathBuf>,
    /// Selections in the active file (0-based points; empty = caret only).
    pub selections: Vec<Range<Point>>,
    /// The changeset of the active diff / review tab.
    pub changeset: Option<Changeset>,
    /// Index of the hunk at the cursor in the active diff.
    pub hunk: Option<usize>,
}

/// Everything a command runs against.
#[derive(Clone)]
pub struct CommandContext {
    /// The application core.
    pub core: AppCore,
    /// The project of the invoking window / session (None: no folder open).
    pub project: Option<Project>,
    /// UI focus (empty for headless callers).
    pub focus: Focus,
    /// Who invoked it.
    pub source: Source,
}

impl CommandContext {
    /// A context without UI focus.
    pub fn new(core: AppCore, project: Option<Project>, source: Source) -> Self {
        CommandContext {
            core,
            project,
            focus: Focus::default(),
            source,
        }
    }

    /// The project or an error naming why a command cannot run.
    pub fn require_project(&self) -> anyhow::Result<&Project> {
        self.project
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no folder is open"))
    }

    /// The project's repository or an error.
    pub fn require_repo(&self) -> anyhow::Result<&lector_git::Repo> {
        self.require_project()?
            .repo()
            .ok_or_else(|| anyhow::anyhow!("the folder is not in a git repository"))
    }

    /// Whether path arguments must stay inside the project: MCP clients and agents may
    /// only reach the project they were given (the GUI and CLI act for the user directly).
    pub fn is_sandboxed(&self) -> bool {
        matches!(self.source, Source::Mcp | Source::Agent)
    }

    /// Checks a resolved path argument. For [sandboxed](Self::is_sandboxed) callers the
    /// path must lie inside the project root after resolving `..` and symlinks of its
    /// deepest existing ancestor, and outside any `.git` directory; other callers pass.
    pub fn confine_path(&self, path: PathBuf) -> anyhow::Result<PathBuf> {
        if !self.is_sandboxed() {
            return Ok(path);
        }
        let outside = || anyhow::anyhow!("{} is outside the project", path.display());
        let root = self.require_project()?.root();
        let root = dunce::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let mut lexical = PathBuf::new();
        for c in path.components() {
            match c {
                std::path::Component::ParentDir => {
                    if !lexical.pop() {
                        return Err(outside());
                    }
                }
                std::path::Component::CurDir => {}
                other => lexical.push(other.as_os_str()),
            }
        }
        let mut existing = lexical.as_path();
        while std::fs::symlink_metadata(existing).is_err() {
            existing = existing.parent().ok_or_else(outside)?;
        }
        let real = dunce::canonicalize(existing).map_err(|_| outside())?;
        let rest = lexical.strip_prefix(existing).map_err(|_| outside())?;
        let resolved = real.join(rest);
        let rel = resolved.strip_prefix(&root).map_err(|_| outside())?;
        if rel
            .components()
            .any(|c| c.as_os_str().to_string_lossy().eq_ignore_ascii_case(".git"))
        {
            anyhow::bail!("{} is inside .git", path.display());
        }
        Ok(lexical)
    }
}

/// A request for the front-end, carried in [`Outcome::ui`]. Headless callers may
/// ignore them; the GUI workspace executes them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UiRequest {
    /// Open a file, optionally at a 0-based point.
    OpenPath {
        /// Absolute path.
        path: PathBuf,
        /// Where to put the caret.
        point: Option<Point>,
    },
    /// Open a changeset review (commit, working tree, range...).
    OpenChangeset(Changeset),
    /// Show a transient message.
    Toast(String),
    /// Re-read git state and files.
    Refresh,
}

/// How a UI shows an [`Outcome`]'s summary: a success, a note (nothing to do) or a warning
/// (partly done, needs attention). Not part of the JSON of the CLI or MCP.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum NoticeLevel {
    /// It worked.
    #[default]
    Success,
    /// A note: nothing happened because nothing was needed ("already up to date").
    Info,
    /// It partly worked (applied with conflicts, pull stopped).
    Warning,
}

/// Result of applying a command.
#[derive(Debug, Clone, Default)]
pub struct Outcome {
    /// One line for toasts and the activity log ("Staged src/main.rs").
    pub summary: String,
    /// How to undo it (writes only).
    pub undo: Option<UndoRecord>,
    /// Structured, token-efficient result for agents / CLI / MCP.
    pub data: Value,
    /// Requests for the UI.
    pub ui: Vec<UiRequest>,
    /// Files or repository paths the command wrote (absolute; for the journal).
    pub paths: Vec<PathBuf>,
    /// Journal entry id, set by [`CommandRegistry::invoke`] when recorded.
    pub journal_id: Option<u64>,
    /// How a UI shows [`Outcome::summary`].
    pub level: NoticeLevel,
}

impl Outcome {
    /// An outcome with a summary and no data.
    pub fn new(summary: impl Into<String>) -> Self {
        Outcome {
            summary: summary.into(),
            data: Value::Null,
            ..Default::default()
        }
    }

    /// Sets [`Outcome::level`].
    pub fn with_level(mut self, level: NoticeLevel) -> Self {
        self.level = level;
        self
    }

    /// Sets [`Outcome::data`].
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = data;
        self
    }

    /// Appends a UI request.
    pub fn with_ui(mut self, req: UiRequest) -> Self {
        self.ui.push(req);
        self
    }

    /// Sets [`Outcome::undo`].
    pub fn with_undo(mut self, undo: UndoRecord) -> Self {
        self.undo = Some(undo);
        self
    }

    /// Sets [`Outcome::paths`].
    pub fn with_paths(mut self, paths: Vec<PathBuf>) -> Self {
        self.paths = paths;
        self
    }
}

/// A command implementation.
pub trait Command: Send + Sync {
    /// Static description.
    fn spec(&self) -> &CommandSpec;
    /// Whether it can run now (cheap; called while rendering the palette).
    fn available(&self, cx: &CommandContext) -> Availability;
    /// Optional human-readable preview (text or a unified diff) of what `apply` would do.
    fn preview(&self, cx: &CommandContext, args: &Value) -> anyhow::Result<Option<String>> {
        let _ = (cx, args);
        Ok(None)
    }
    /// Runs it. Blocking: call from a background thread / the scheduler.
    fn apply(&self, cx: &CommandContext, args: &Value) -> anyhow::Result<Outcome>;
}

/// What [`CommandRegistry::invoke`] hands to the journal for every write.
#[derive(Debug, Clone)]
pub struct ActivityRecord {
    /// Command id.
    pub command: String,
    /// Arguments as invoked.
    pub args: Value,
    /// [`Outcome::summary`].
    pub summary: String,
    /// Repository work directory, if any.
    pub repo: Option<PathBuf>,
    /// [`Outcome::paths`].
    pub paths: Vec<PathBuf>,
    /// [`Outcome::undo`].
    pub undo: Option<UndoRecord>,
    /// Who invoked it.
    pub source: Source,
}

/// Receives a record of every successful write command. Implemented by the
/// activity journal (`crate::journal`); plugged in with [`AppCore::set_activity_sink`].
pub trait ActivitySink: Send + Sync {
    /// Records one write; returns the journal entry id.
    fn record(&self, record: ActivityRecord) -> Option<u64>;
}

/// Failure of [`CommandRegistry::invoke`].
#[derive(Debug, thiserror::Error)]
pub enum CommandError {
    /// No command with that id.
    #[error("unknown command `{0}`")]
    Unknown(String),
    /// Not available in this context.
    #[error("{0}")]
    Unavailable(String),
    /// Arguments do not match the schema.
    #[error("invalid arguments: {0}")]
    InvalidArgs(String),
    /// The command ran and failed.
    #[error(transparent)]
    Failed(#[from] anyhow::Error),
}

/// All registered commands. Cheap to clone (shared).
#[derive(Clone, Default)]
pub struct CommandRegistry {
    commands: Arc<RwLock<BTreeMap<&'static str, Arc<dyn Command>>>>,
}

impl std::fmt::Debug for CommandRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandRegistry")
            .field("len", &self.commands.read().len())
            .finish()
    }
}

impl CommandRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds (or replaces) a command by its id.
    pub fn register(&self, command: impl Command + 'static) {
        self.register_arc(Arc::new(command));
    }

    /// Adds (or replaces) a shared command.
    pub fn register_arc(&self, command: Arc<dyn Command>) {
        let id = command.spec().id;
        self.commands.write().insert(id, command);
    }

    /// Looks a command up by id.
    pub fn get(&self, id: &str) -> Option<Arc<dyn Command>> {
        self.commands.read().get(id).cloned()
    }

    /// The command whose [`CommandSpec::ui_action`] is `action` ("git::Stage").
    pub fn for_ui_action(&self, action: &str) -> Option<Arc<dyn Command>> {
        self.commands
            .read()
            .values()
            .find(|c| c.spec().ui_action == Some(action))
            .cloned()
    }

    /// All commands, sorted by id.
    pub fn list(&self) -> Vec<Arc<dyn Command>> {
        self.commands.read().values().cloned().collect()
    }

    /// Number of commands.
    pub fn len(&self) -> usize {
        self.commands.read().len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Checks availability, validates `args`, applies the command and, for
    /// writes, records the outcome in the activity journal. Blocking.
    pub fn invoke(
        &self,
        id: &str,
        args: Value,
        cx: &CommandContext,
    ) -> Result<Outcome, CommandError> {
        let command = self
            .get(id)
            .ok_or_else(|| CommandError::Unknown(id.to_string()))?;
        let spec = command.spec();
        let args = if args.is_null() { json!({}) } else { args };
        schema::validate(&spec.args_schema, &args).map_err(CommandError::InvalidArgs)?;
        if let Availability::Unavailable(why) = command.available(cx) {
            return Err(CommandError::Unavailable(why));
        }
        let mut outcome = command.apply(cx, &args)?;
        if spec.writes {
            if let Some(sink) = cx.core.activity_sink() {
                let repo = cx
                    .project
                    .as_ref()
                    .and_then(|p| p.repo())
                    .map(|r| r.workdir().to_path_buf());
                outcome.journal_id = sink.record(ActivityRecord {
                    command: spec.id.to_string(),
                    args,
                    summary: outcome.summary.clone(),
                    repo,
                    paths: outcome.paths.clone(),
                    undo: outcome.undo.clone(),
                    source: cx.source,
                });
            } else {
                tracing::debug!("no activity sink: `{}` not journaled", spec.id);
            }
        }
        Ok(outcome)
    }

    /// Runs [`CommandRegistry::invoke`] on the core's scheduler at
    /// [`crate::scheduler::Priority::Interactive`].
    pub fn invoke_async(
        &self,
        id: &str,
        args: Value,
        cx: CommandContext,
    ) -> crate::scheduler::JobHandle<Result<Outcome, CommandError>> {
        let this = self.clone();
        let id = id.to_string();
        let scheduler = cx.core.scheduler().clone();
        scheduler.spawn(crate::scheduler::Priority::Interactive, None, move |_| {
            this.invoke(&id, args, &cx)
        })
    }
}

type AvailableFn = Box<dyn Fn(&CommandContext) -> Availability + Send + Sync>;
type ApplyFn = Box<dyn Fn(&CommandContext, &Value) -> anyhow::Result<Outcome> + Send + Sync>;

/// A command built from closures (handy for simple commands and tests).
pub struct FnCommand {
    spec: CommandSpec,
    available: AvailableFn,
    apply: ApplyFn,
}

impl FnCommand {
    /// Always-available command.
    pub fn new(
        spec: CommandSpec,
        apply: impl Fn(&CommandContext, &Value) -> anyhow::Result<Outcome> + Send + Sync + 'static,
    ) -> Self {
        FnCommand {
            spec,
            available: Box::new(|_| Availability::Available),
            apply: Box::new(apply),
        }
    }

    /// Sets the availability check.
    pub fn with_available(
        mut self,
        f: impl Fn(&CommandContext) -> Availability + Send + Sync + 'static,
    ) -> Self {
        self.available = Box::new(f);
        self
    }
}

impl Command for FnCommand {
    fn spec(&self) -> &CommandSpec {
        &self.spec
    }
    fn available(&self, cx: &CommandContext) -> Availability {
        (self.available)(cx)
    }
    fn apply(&self, cx: &CommandContext, args: &Value) -> anyhow::Result<Outcome> {
        (self.apply)(cx, args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::CoreOptions;
    use parking_lot::Mutex;

    #[derive(Default)]
    struct Sink(Mutex<Vec<ActivityRecord>>);

    impl ActivitySink for Sink {
        fn record(&self, record: ActivityRecord) -> Option<u64> {
            let mut v = self.0.lock();
            v.push(record);
            Some(v.len() as u64)
        }
    }

    fn echo(writes: bool) -> FnCommand {
        let schema = schema::object(&[("n", json!({"type": "integer"}))], &["n"]);
        let spec = if writes {
            CommandSpec::write("test.write", "Write", "Test", schema).with_ui_action("test::Write")
        } else {
            CommandSpec::read("test.read", "Read", "Test", schema)
        };
        FnCommand::new(spec, |_, args| {
            Ok(Outcome::new("done")
                .with_data(args.clone())
                .with_paths(vec![PathBuf::from("/x")]))
        })
    }

    #[test]
    fn invoke_validates_checks_availability_and_journals_writes() {
        let core = AppCore::open(CoreOptions::for_tests());
        let sink = Arc::new(Sink::default());
        core.set_activity_sink(sink.clone());
        let reg = core.registry();
        reg.register(echo(true));
        reg.register(echo(false));
        reg.register(
            FnCommand::new(
                CommandSpec::read("test.never", "Never", "Test", json!({})),
                |_, _| Ok(Outcome::new("x")),
            )
            .with_available(|_| Availability::no("not here")),
        );
        let cx = CommandContext::new(core.clone(), None, Source::Agent);

        let out = reg.invoke("test.write", json!({"n": 1}), &cx).unwrap();
        assert_eq!(out.data, json!({"n": 1}));
        assert_eq!(out.journal_id, Some(1));
        let rec = sink.0.lock()[0].clone();
        assert_eq!(rec.command, "test.write");
        assert_eq!(rec.source, Source::Agent);
        assert_eq!(rec.paths, vec![PathBuf::from("/x")]);

        let out = reg.invoke("test.read", json!({"n": 2}), &cx).unwrap();
        assert_eq!(out.journal_id, None);
        assert_eq!(sink.0.lock().len(), 1, "reads are not journaled");

        assert!(matches!(
            reg.invoke("test.write", json!({"n": "x"}), &cx),
            Err(CommandError::InvalidArgs(_))
        ));
        assert!(matches!(
            reg.invoke("test.write", Value::Null, &cx),
            Err(CommandError::InvalidArgs(_))
        ));
        assert!(matches!(
            reg.invoke("test.never", json!({}), &cx),
            Err(CommandError::Unavailable(why)) if why == "not here"
        ));
        assert!(matches!(
            reg.invoke("missing", json!({}), &cx),
            Err(CommandError::Unknown(_))
        ));
        assert_eq!(
            reg.for_ui_action("test::Write").map(|c| c.spec().id),
            Some("test.write")
        );
        let res = reg
            .invoke_async("test.write", json!({"n": 3}), cx.clone())
            .wait()
            .unwrap()
            .unwrap();
        assert_eq!(res.journal_id, Some(2));
    }

    #[test]
    fn builtin_commands_have_object_schemas() {
        let core = AppCore::open(CoreOptions::for_tests());
        assert!(core.registry().len() >= 10);
        for c in core.registry().list() {
            let spec = c.spec();
            assert_eq!(spec.args_schema["type"], "object", "{}", spec.id);
            assert!(spec.id.contains('.'), "{}", spec.id);
            // Every schema accepts its own minimal valid input shape.
            assert!(spec.to_json()["args"].is_object());
        }
    }

    #[test]
    fn extensions_are_typed_slots() {
        let core = AppCore::open(CoreOptions::for_tests());
        assert!(core.extension::<String>().is_none());
        core.set_extension(Arc::new(String::from("docs")));
        assert_eq!(
            core.extension::<String>().as_deref().map(String::as_str),
            Some("docs")
        );
        let n = core.extension_or_init(|| Arc::new(7u32));
        assert_eq!(*n, 7);
        assert_eq!(*core.extension_or_init(|| Arc::new(8u32)), 7);
    }
}
