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
//! document commands in `documents` (Agent DOCS), the pull request fetch into Lector's shadow
//! repository in [`pull_request`], undo (`journal.undo`, `journal.undo_last`) in
//! [`journal_undo`]. [`agent_backup`] is not a command: it is the backup half of
//! the agent write path (ADR 0010, ADR 0011), here because it takes backups through the
//! `lector-git` write API like the commands do.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;

use crate::project::Project;
use lector_git::Changeset;
use lector_text::Point;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub use crate::core::AppCore;
pub use crate::journal::UndoRecord;

pub mod agent_backup;
pub mod documents;
pub mod git_sync;
pub mod git_write;
pub mod journal_undo;
pub mod pull_request;
pub mod read;
pub mod schema;

/// Registers every built-in command. Other modules add their `register` call here.
pub fn register_builtin(registry: &CommandRegistry) {
    read::register(registry);
    documents::register(registry);
    git_write::register(registry);
    git_sync::register(registry);
    pull_request::register(registry);
    journal_undo::register(registry);
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
    /// [`Outcome::undo`] is filled on success. Always equal to
    /// `undo == UndoPolicy::Undoable`; kept for existing readers of the spec.
    pub undoable: bool,
    /// The undo declaration (ADR 0002): every command that writes says whether the journal can
    /// undo it, and if not, why. Set with [`CommandSpec::undoable`] /
    /// [`CommandSpec::not_undoable`]; a write left [`UndoPolicy::Undeclared`] fails the
    /// `undo_roundtrip` test.
    pub undo: UndoPolicy,
    /// The GPUI action ("git::Stage") that dispatches this command, if any. The generic
    /// action bridge registers a handler that runs the command as is.
    pub ui_action: Option<&'static str>,
    /// The GPUI action whose handler in a UI crate runs this command through its own flow
    /// (selection, confirmation, progress, Undo toast). The bridge registers nothing for it
    /// and the palette lists that action's row instead of a second row for the command.
    pub ui_flow: Option<&'static str>,
    /// Who may run it, as declared with [`CommandSpec::ui_only`] (or set directly).
    /// [`CommandRegistry::invoke`] and [`CommandRegistry::list_for`] enforce
    /// [`CommandSpec::allowed_sources`], the stricter of this and what the command touches
    /// ([`CommandSpec::touches`]), so a command that declares network use cannot be reached by
    /// MCP clients even if this field says [`Sources::All`].
    pub sources: Sources,
    /// What the command reaches beyond Lector and the project's files (ADR 0010, item 3):
    /// declared with [`CommandSpec::uses_network`], [`CommandSpec::uses_credentials`],
    /// [`CommandSpec::steers_agent`] and [`CommandSpec::answers_for_user`]. It decides the
    /// minimum [`Sources`] ([`Touches::minimum_sources`]).
    pub touches: Touches,
    /// The command writes its own journal entry, so [`CommandRegistry::invoke`] records none for
    /// the invocation and leaves [`Outcome::journal_id`] as the command set it. Declared with
    /// [`CommandSpec::records_itself`]; only the undo commands do (`journal.undo`,
    /// `journal.undo_last`: the journal records an undo as the entry that carries its redo).
    pub records_itself: bool,
}

/// What a command reaches besides the project's files and Lector's own state
/// ([`CommandSpec::touches`]). Every flag raises the minimum [`Sources`]: a person at a keyboard
/// must have chosen to send something over the network, use the user's credentials or drive an
/// agent; answering an agent's question in the user's name happens only in Lector's window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Touches {
    /// Talks to another machine (git fetch / pull / push, a code host's API).
    pub network: bool,
    /// Uses the user's credentials (git credential helpers and askpass, forge tokens).
    pub credentials: bool,
    /// Starts, prompts or cancels an agent: makes another program act on the project.
    pub steers_agent: bool,
    /// Answers for the user (an agent's permission request).
    pub answers_for_user: bool,
}

impl Touches {
    /// The least restrictive [`Sources`] a command touching these may have: `UiOnly` when it
    /// answers for the user, `Interactive` when it uses the network or credentials or steers an
    /// agent, else `All`.
    pub fn minimum_sources(self) -> Sources {
        if self.answers_for_user {
            Sources::UiOnly
        } else if self.network || self.credentials || self.steers_agent {
            Sources::Interactive
        } else {
            Sources::All
        }
    }
}

/// Who may run a command ([`CommandSpec::allowed_sources`]), from widest to narrowest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum Sources {
    /// Every front-end: the GUI, the CLI, MCP clients, agents, internal callers.
    #[default]
    All,
    /// A person at a keyboard chose it: the GUI ([`Source::Ui`]), the CLI ([`Source::Cli`],
    /// `lector run` in a terminal or a script the user started) and Lector itself
    /// ([`Source::Internal`]). MCP clients and agents are refused, with or without
    /// `--allow-writes`: network, credentials and agent control (ADR 0010).
    Interactive,
    /// Only the GUI ([`Source::Ui`]): its view fronts it (the Pull Requests panel fetching a
    /// pull request, answering an agent's permission request); the CLI, MCP and agents are
    /// refused.
    UiOnly,
}

impl Sources {
    /// Whether a caller of `source` may run the command.
    pub fn allows(self, source: Source) -> bool {
        match self {
            Sources::All => true,
            Sources::Interactive => {
                matches!(source, Source::Ui | Source::Cli | Source::Internal)
            }
            Sources::UiOnly => source == Source::Ui,
        }
    }

    /// The narrower of two.
    pub fn stricter(self, other: Sources) -> Sources {
        self.max(other)
    }

    /// Stable name for `--json` / `commands.list` (`all`, `interactive`, `ui_only`).
    pub fn as_str(self) -> &'static str {
        match self {
            Sources::All => "all",
            Sources::Interactive => "interactive",
            Sources::UiOnly => "ui_only",
        }
    }
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
            undo: UndoPolicy::NothingToUndo,
            ui_action: None,
            ui_flow: None,
            sources: Sources::All,
            touches: Touches::default(),
            records_itself: false,
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
            undo: UndoPolicy::Undeclared,
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

    /// Restricts the command to the GUI ([`Sources::UiOnly`]).
    pub fn ui_only(mut self) -> Self {
        self.sources = Sources::UiOnly;
        self
    }

    /// Declares that it talks to another machine ([`Touches::network`]): at least
    /// [`Sources::Interactive`].
    pub fn uses_network(mut self) -> Self {
        self.touches.network = true;
        self
    }

    /// Declares that it uses the user's credentials ([`Touches::credentials`]): at least
    /// [`Sources::Interactive`].
    pub fn uses_credentials(mut self) -> Self {
        self.touches.credentials = true;
        self
    }

    /// Declares that it starts, prompts or cancels an agent ([`Touches::steers_agent`]): at
    /// least [`Sources::Interactive`].
    pub fn steers_agent(mut self) -> Self {
        self.touches.steers_agent = true;
        self
    }

    /// Declares that it answers for the user ([`Touches::answers_for_user`]):
    /// [`Sources::UiOnly`].
    pub fn answers_for_user(mut self) -> Self {
        self.touches.answers_for_user = true;
        self
    }

    /// Who may run it: the stricter of [`CommandSpec::sources`] and
    /// [`Touches::minimum_sources`]. The one value [`CommandRegistry::invoke`],
    /// [`CommandRegistry::list_for`] and the palette use.
    pub fn allowed_sources(&self) -> Sources {
        self.sources.stricter(self.touches.minimum_sources())
    }

    /// Declares that the command writes its own journal entry ([`CommandSpec::records_itself`]).
    pub fn records_itself(mut self) -> Self {
        self.records_itself = true;
        self
    }

    /// Marks the command destructive.
    pub fn destructive(mut self) -> Self {
        self.destructive = true;
        self
    }

    /// Marks the command undoable: [`Outcome::undo`] is filled on success (unless there was
    /// nothing to do).
    pub fn undoable(mut self) -> Self {
        self.undoable = true;
        self.undo = UndoPolicy::Undoable;
        self
    }

    /// Declares that the journal cannot undo the command, and why (shown to agents and in
    /// `commands.list`).
    pub fn not_undoable(mut self, reason: &'static str) -> Self {
        self.undoable = false;
        self.undo = UndoPolicy::NotUndoable(reason);
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
        if let UndoPolicy::NotUndoable(reason) = self.undo {
            v["not_undoable"] = json!(reason);
        }
        if let Some(a) = self.ui_action {
            v["ui_action"] = json!(a);
        }
        if let Some(a) = self.ui_flow {
            v["ui_flow"] = json!(a);
        }
        let sources = self.allowed_sources();
        if sources == Sources::UiOnly {
            v["ui_only"] = json!(true);
        }
        v["sources"] = json!(sources.as_str());
        if self.touches.network {
            v["network"] = json!(true);
        }
        if self.touches.credentials {
            v["credentials"] = json!(true);
        }
        if self.touches.steers_agent {
            v["steers_agent"] = json!(true);
        }
        if self.touches.answers_for_user {
            v["answers_for_user"] = json!(true);
        }
        v
    }
}

/// Whether the activity journal can undo a command ([`CommandSpec::undo`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoPolicy {
    /// Its [`Outcome::undo`] is filled on success; the journal's Undo reverses it.
    Undoable,
    /// The journal cannot undo it; the text says why (pushes are public, a commit is taken
    /// back with `git reset`, ...).
    NotUndoable(&'static str),
    /// A read: it changes nothing, so there is nothing to undo.
    NothingToUndo,
    /// A write that did not declare its undo yet (the `undo_roundtrip` test rejects it).
    Undeclared,
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

    /// Checks a path argument the command will read. For [sandboxed](Self::is_sandboxed)
    /// callers the path goes through [`lector_fs::Confinement::read`] ([`confinement`]): it
    /// must resolve (symlinks included) inside the project, outside every git directory and
    /// outside Lector's own data and settings; the resolved path is returned. Other callers
    /// get the path back unchanged.
    pub fn confine_path(&self, path: PathBuf) -> anyhow::Result<PathBuf> {
        self.confine_as(path, lector_fs::Access::Read)
    }

    /// [`CommandContext::confine_path`] for a path the command will write: a final-component
    /// symlink is refused too ([`lector_fs::Confinement::write`]). Commands that write such a
    /// path pass [`CommandContext::save_confinement`] on to the save so the write itself does
    /// not follow a link that appears after this check.
    pub fn confine_write_path(&self, path: PathBuf) -> anyhow::Result<PathBuf> {
        self.confine_as(path, lector_fs::Access::Write)
    }

    /// The confinement a write from this context must stay in (`Some` for sandboxed callers),
    /// for [`crate::documents::SaveRequest::confine`].
    pub fn save_confinement(&self) -> anyhow::Result<Option<lector_fs::Confinement>> {
        if !self.is_sandboxed() {
            return Ok(None);
        }
        Ok(Some(confinement(
            &self.core,
            self.require_project()?.root(),
        )?))
    }

    fn confine_as(&self, path: PathBuf, access: lector_fs::Access) -> anyhow::Result<PathBuf> {
        if !self.is_sandboxed() {
            return Ok(path);
        }
        let root = self.require_project()?.root();
        Ok(confinement(&self.core, root)?.check(&path, access)?.path())
    }
}

/// The one confinement of sandboxed callers (agents, MCP clients) to a project `root`
/// (ADR 0010): [`lector_fs::Confinement`] for the root, also forbidding Lector's data directory
/// (the core's shadow directory and `lector_config::paths::data_dir`) and its settings
/// directory when the project contains them (portable mode, overrides), so an agent cannot
/// reach backups, logs or `settings.toml` (which chooses programs to run).
pub fn confinement(
    core: &AppCore,
    root: &std::path::Path,
) -> Result<lector_fs::Confinement, lector_fs::ConfineError> {
    Ok(lector_fs::Confinement::new(root)?
        .forbid(&pull_request::shadow_data_dir(core))
        .forbid(&lector_config::paths::data_dir())
        .forbid(&lector_config::paths::config_dir()))
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

    /// All commands, sorted by id (UI-only ones included; front-ends list
    /// [`CommandRegistry::list_for`] instead).
    pub fn list(&self) -> Vec<Arc<dyn Command>> {
        self.commands.read().values().cloned().collect()
    }

    /// The commands a caller of `source` may run ([`CommandSpec::sources`]), sorted by id:
    /// what MCP `tools/list`, `lector commands` and `commands.list` show.
    pub fn list_for(&self, source: Source) -> Vec<Arc<dyn Command>> {
        self.commands
            .read()
            .values()
            .filter(|c| c.spec().allowed_sources().allows(source))
            .cloned()
            .collect()
    }

    /// Number of commands.
    pub fn len(&self) -> usize {
        self.commands.read().len()
    }

    /// True when empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Checks that the caller's [`Source`] may run it ([`CommandSpec::sources`]: the one
    /// place UI-only commands are enforced), validates `args`, checks availability, applies
    /// the command and, for writes, records the outcome in the activity journal (unless the
    /// command [records itself](CommandSpec::records_itself)). Blocking.
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
        match spec.allowed_sources() {
            sources if sources.allows(cx.source) => {}
            Sources::Interactive => {
                return Err(CommandError::Unavailable(format!(
                    "`{}` runs only when a person starts it (Lector's window or `lector run`), not from MCP clients or agents: it uses the network, the user's credentials or controls an agent",
                    spec.id
                )))
            }
            _ => {
                return Err(CommandError::Unavailable(format!(
                    "`{}` runs only from Lector's window, not from the CLI, MCP clients or agents",
                    spec.id
                )))
            }
        }
        let args = if args.is_null() { json!({}) } else { args };
        schema::validate(&spec.args_schema, &args).map_err(CommandError::InvalidArgs)?;
        if let Availability::Unavailable(why) = command.available(cx) {
            return Err(CommandError::Unavailable(why));
        }
        let mut outcome = command.apply(cx, &args)?;
        if spec.writes && !spec.records_itself {
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

    /// Runs [`CommandRegistry::invoke`] on the core's scheduler (blocking lane, `Visible`; job
    /// kind [`crate::scheduler::JobKind::Command`] for a write, which is drained at quit, else
    /// `CommandRead`).
    pub fn invoke_async(
        &self,
        id: &str,
        args: Value,
        cx: CommandContext,
    ) -> crate::scheduler::JobHandle<Result<Outcome, CommandError>> {
        let this = self.clone();
        let id = id.to_string();
        let scheduler = cx.core.scheduler().clone();
        let writes = self.get(&id).is_none_or(|c| c.spec().writes);
        let kind = if writes {
            crate::scheduler::JobKind::Command
        } else {
            crate::scheduler::JobKind::CommandRead
        };
        let key = crate::scheduler::JobKey::new(kind, crate::scheduler::Subject::App);
        scheduler.spawn(key, crate::scheduler::Priority::Visible, move |_| {
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

    /// ADR 0010, item 3: what a command touches sets its minimum sources; the declared
    /// `sources` can narrow but never widen it, and `invoke` enforces the result.
    #[test]
    fn touches_decide_the_minimum_sources() {
        let net =
            CommandSpec::write("t.net", "Net", "Test", json!({"type": "object"})).uses_network();
        assert_eq!(net.allowed_sources(), Sources::Interactive);
        let mut widened = net.clone();
        widened.sources = Sources::All;
        assert_eq!(widened.allowed_sources(), Sources::Interactive);
        let answer = CommandSpec::write("t.ans", "Ans", "Test", json!({"type": "object"}))
            .steers_agent()
            .answers_for_user();
        assert_eq!(answer.allowed_sources(), Sources::UiOnly);
        let creds = CommandSpec::read("t.cred", "Cred", "Test", json!({"type": "object"}))
            .uses_credentials()
            .ui_only();
        assert_eq!(creds.allowed_sources(), Sources::UiOnly);
        for (sources, allowed) in [
            (Sources::All, [true, true, true, true, true]),
            (Sources::Interactive, [true, true, false, false, true]),
            (Sources::UiOnly, [true, false, false, false, false]),
        ] {
            let got = [
                Source::Ui,
                Source::Cli,
                Source::Mcp,
                Source::Agent,
                Source::Internal,
            ]
            .map(|s| sources.allows(s));
            assert_eq!(got, allowed, "{sources:?}");
        }
        assert_eq!(net.to_json()["sources"], "interactive");
        assert_eq!(net.to_json()["network"], true);
        assert!(net.to_json().get("ui_only").is_none());

        let core = AppCore::open(CoreOptions::for_tests());
        let r = core.registry();
        r.register(FnCommand::new(widened, |_, _| Ok(Outcome::new("ran"))));
        let mcp = CommandContext::new(core.clone(), None, Source::Mcp);
        assert!(matches!(
            r.invoke("t.net", json!({}), &mcp),
            Err(CommandError::Unavailable(_))
        ));
        assert!(!r
            .list_for(Source::Mcp)
            .iter()
            .any(|c| c.spec().id == "t.net"));
        let cli = CommandContext::new(core.clone(), None, Source::Cli);
        assert!(r.invoke("t.net", json!({}), &cli).is_ok());
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
