//! Job identity: the closed list of job kinds and the subjects they belong to (ADR 0003).
//!
//! A [`JobKey`] is a [`JobKind`] plus a [`Subject`]. Every scheduled job has one:
//! * a newer job with the same key cancels the older ones when the kind
//!   [replaces](JobKind::replaces) ("latest wins", e.g. ranking for the newest query);
//! * [`Scheduler::cancel_subject`](super::Scheduler::cancel_subject) cancels every job of a
//!   subject (a document, a repository, a view...) when it closes, except kinds that
//!   [finish on their own](JobKind::cancelled_with_subject) (writes the user asked for);
//! * the kind picks the [`Lane`]: short, CPU or local-disk work runs on the compute workers;
//!   work whose duration depends on somebody else (the network, a language server, an agent,
//!   the user answering a prompt, an external program) runs on the bounded blocking lane so it
//!   can never occupy every compute worker.
//!
//! The priority of each kind is chosen by the caller; only the kinds listed in
//! [`JobKind::may_be_interactive`] may use [`Priority::Interactive`](super::Priority), the
//! others are run as `Visible` (see `docs/ARCHITECTURE.md`, "Background work").

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Where a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The fixed compute pool (CPU work, local disk and local `git`, bounded by input size).
    Compute,
    /// The bounded blocking lane: waits whose length nobody here controls.
    Blocking,
}

macro_rules! job_kinds {
    ($( $(#[$doc:meta])* $name:ident = ($lane:ident, $replaces:expr, $with_subject:expr, $interactive:expr) ),* $(,)?) => {
        /// The closed set of background job kinds. Adding one is a one-line change here; the
        /// table in `docs/ARCHITECTURE.md` lists them with their priority.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum JobKind {
            $( $(#[$doc])* $name, )*
        }

        impl JobKind {
            /// Every kind, in declaration order.
            pub const ALL: &'static [JobKind] = &[ $( JobKind::$name, )* ];

            /// The lane jobs of this kind run on.
            pub const fn lane(self) -> Lane {
                match self { $( JobKind::$name => Lane::$lane, )* }
            }

            /// Whether a newer job with the same key cancels the older ones. Kinds that do not
            /// replace accumulate (several highlight batches of one document, a write per click).
            pub const fn replaces(self) -> bool {
                match self { $( JobKind::$name => $replaces, )* }
            }

            /// Whether [`Scheduler::cancel_subject`](super::Scheduler::cancel_subject) cancels
            /// jobs of this kind. `false` for writes the user asked for: closing the window
            /// right after "Commit" must not lose the commit.
            pub const fn cancelled_with_subject(self) -> bool {
                match self { $( JobKind::$name => $with_subject, )* }
            }

            /// Whether this kind may run at `Priority::Interactive` (it directly answers a
            /// keystroke and is short). Other kinds asking for it run at `Visible`.
            pub const fn may_be_interactive(self) -> bool {
                match self { $( JobKind::$name => $interactive, )* }
            }
        }
    };
}

// (lane, replaces, cancelled_with_subject, may_be_interactive)
job_kinds! {
    // ---- documents (lector-app document store) ----
    /// Reparse after an edit.
    DocumentParse = (Compute, false, true, true),
    /// Syntax highlights of rows on screen.
    DocumentHighlight = (Compute, false, true, false),
    /// Outline, folds and structure queries.
    DocumentStructure = (Compute, true, true, false),
    /// Git gutter of a document.
    DocumentGutter = (Compute, true, true, false),
    /// Blame mapping of a document.
    DocumentBlame = (Compute, true, true, false),
    /// Syncing a document to its language server.
    DocumentLsp = (Compute, true, true, false),
    /// Opening (reading, decoding) a document for an editor.
    DocumentOpen = (Compute, true, true, false),
    /// Re-reading a document from disk / checking it against disk.
    DocumentReload = (Compute, true, true, false),
    /// Saving a document (a write the user asked for).
    DocumentSave = (Compute, false, false, false),
    /// Reading a file for a view (not through the document store).
    FileRead = (Compute, true, true, false),
    /// Image metadata or an SVG rasterization.
    ImageDecode = (Compute, true, true, false),
    /// Markdown parsing for a preview or hover.
    MarkdownRender = (Compute, true, true, false),

    // ---- search and pickers ----
    /// Ranking a picker's rows for the newest query.
    PickerRank = (Compute, true, true, true),
    /// Feeding a picker its candidate list.
    PickerFeed = (Compute, true, true, false),
    /// Find in the current file as the user types.
    FindInFile = (Compute, true, true, true),
    /// Computing a replace-all.
    ReplaceInFile = (Compute, true, true, false),
    /// Project-wide text search.
    ProjectSearch = (Compute, true, true, false),
    /// Listing the project's symbols for the symbol picker.
    ProjectSymbols = (Compute, true, true, false),

    // ---- project ----
    /// Rescanning the project's file list.
    FileRescan = (Compute, true, true, false),
    /// Building the project's symbol index.
    SymbolIndex = (Compute, true, true, false),
    /// Re-indexing changed files.
    Reindex = (Compute, false, true, false),
    /// Registering recursive file watches (walks the tree once).
    WatchRegister = (Compute, true, true, false),
    /// Loading settings, the project and themes at startup.
    StartupLoad = (Blocking, true, true, false),
    /// Running a helper program of the OS (reveal in the file manager, preference probes).
    ExternalProgram = (Blocking, false, true, false),

    // ---- language servers ----
    /// Hover from a language server.
    LspHover = (Blocking, true, true, false),
    /// Go to definition.
    LspDefinitions = (Blocking, true, true, false),
    /// Find references.
    LspReferences = (Blocking, true, true, false),
    /// Opening a document on a language server from the UI.
    LspSync = (Blocking, true, true, false),
    /// Launching or restarting a language server (waits for `initialize`).
    LspLaunch = (Blocking, true, true, false),

    // ---- git (local) ----
    /// Re-reading HEAD, status and conflicts of a repository.
    GitRefresh = (Compute, true, true, false),
    /// Sync status (ahead/behind, upstream).
    GitSyncStatus = (Compute, true, true, false),
    /// Branches, stashes, refs, unreachable commits: lists for a menu or panel.
    GitRefs = (Compute, true, true, false),
    /// The committer identity.
    GitIdentity = (Compute, true, true, false),
    /// The last commit message (amend).
    GitCommitMessage = (Compute, true, true, false),
    /// Commit history.
    GitLog = (Compute, true, true, false),
    /// Blame of a file.
    GitBlame = (Compute, true, true, false),
    /// A changeset (diff) for a panel, a tab or a diff view.
    GitChangeset = (Compute, true, true, false),
    /// One file's diff summary.
    GitFileSummary = (Compute, true, true, false),
    /// A permalink or a revision lookup.
    GitLookup = (Compute, true, true, false),
    /// Credential helpers git is configured with.
    GitConfigRead = (Compute, true, true, false),
    /// Conflict parsing for inline conflicts or the merge editor.
    ConflictParse = (Compute, true, true, false),
    /// A write command the user asked for (stage, commit, discard, undo...).
    GitWrite = (Compute, false, false, false),
    /// A registry command run off the caller's thread (`CommandRegistry::invoke_async`): any
    /// command, network sync included, so the blocking lane; it may write, so never cancelled
    /// with a subject.
    Command = (Blocking, false, false, false),

    // ---- review ----
    /// Review analysis of changes (risk, structure, queue).
    ReviewAnalysis = (Compute, true, true, false),
    /// Fingerprint of the working tree changes.
    ReviewFingerprint = (Compute, true, true, false),
    /// Provenance explanation for a hover.
    ProvenanceExplain = (Compute, true, true, false),
    /// The activity journal list.
    JournalList = (Compute, true, true, false),
    /// A preview of what undoing a journal entry restores.
    JournalPreview = (Compute, true, true, false),

    // ---- network ----
    /// A forge request that only reads (pull requests, opening a review).
    ForgeRead = (Blocking, true, true, false),
    /// A forge request that writes (reply, resolve, submit a review).
    ForgeWrite = (Blocking, false, false, false),
    /// Checking for an update.
    UpdateCheck = (Blocking, true, true, false),
    /// One credential prompt connection (waits for the user).
    AskpassPrompt = (Blocking, false, true, false),

    // ---- agents ----
    /// Starting or restarting an agent session (spawns and initializes the agent).
    AgentStart = (Blocking, true, true, false),
    /// One agent turn.
    AgentTurn = (Blocking, false, true, false),
    /// One request from the agent (may wait for the user's permission).
    AgentRequest = (Blocking, false, true, false),
    /// Cancelling a turn or undoing one (talks to the agent / writes).
    AgentControl = (Blocking, false, false, false),
    /// Computing a turn summary or the diff context for the agent.
    AgentContext = (Compute, false, true, false),

    // ---- terminal ----
    /// Starting a terminal session.
    TerminalSpawn = (Compute, true, true, false),
    /// Loading or saving the terminal tabs of a project.
    TerminalSessions = (Compute, true, true, false),
    /// Resolving a clicked link in a terminal.
    TerminalLink = (Compute, true, true, false),
    /// Killing a terminal's process tree after its grace period.
    TerminalKill = (Blocking, false, false, false),
}

impl JobKind {
    /// The kind's name for diagnostics (`DocumentParse` -> `"DocumentParse"`).
    pub fn name(self) -> String {
        format!("{self:?}")
    }
}

/// What a job is about: cancelling a subject cancels every job that belongs to it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Subject {
    /// Process-wide work that belongs to nothing that closes.
    App,
    /// A window.
    Window(u64),
    /// A document of the document store (its id).
    Document(u64),
    /// A repository, by work directory.
    Repo(Arc<Path>),
    /// A file or directory, by path.
    Path(Arc<Path>),
    /// One view or model of the UI (a GPUI entity id): a panel, a picker, an editor.
    View(u64),
    /// An agent session (its session id).
    Session(u64),
    /// An owner without an id of its own (a picker's ranker...): see [`Subject::unique`].
    Local(u64),
}

impl Subject {
    /// A new subject no other owner uses (process-unique).
    pub fn unique() -> Subject {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Subject::Local(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    /// A repository subject.
    pub fn repo(workdir: impl Into<PathBuf>) -> Subject {
        Subject::Repo(Arc::from(workdir.into()))
    }

    /// A path subject.
    pub fn path(path: impl Into<PathBuf>) -> Subject {
        Subject::Path(Arc::from(path.into()))
    }
}

/// Identifies a job: newer jobs with the same key replace older ones (for kinds that
/// [replace](JobKind::replaces)); every job of a subject can be cancelled at once.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey {
    /// What kind of work.
    pub kind: JobKind,
    /// What it belongs to.
    pub subject: Subject,
}

impl JobKey {
    /// A key.
    pub fn new(kind: JobKind, subject: Subject) -> JobKey {
        JobKey { kind, subject }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interactive_kinds_are_few_and_short() {
        let interactive: Vec<JobKind> = JobKind::ALL
            .iter()
            .copied()
            .filter(|k| k.may_be_interactive())
            .collect();
        assert_eq!(
            interactive,
            [
                JobKind::DocumentParse,
                JobKind::PickerRank,
                JobKind::FindInFile
            ]
        );
        assert!(interactive.iter().all(|k| k.lane() == Lane::Compute));
    }

    #[test]
    fn writes_are_never_replaced_or_cancelled_with_their_subject() {
        for kind in [
            JobKind::DocumentSave,
            JobKind::GitWrite,
            JobKind::ForgeWrite,
            JobKind::AgentControl,
        ] {
            assert!(!kind.replaces(), "{kind:?}");
            assert!(!kind.cancelled_with_subject(), "{kind:?}");
        }
    }

    #[test]
    fn waits_on_others_run_on_the_blocking_lane() {
        for kind in [
            JobKind::LspHover,
            JobKind::LspLaunch,
            JobKind::ForgeRead,
            JobKind::UpdateCheck,
            JobKind::AskpassPrompt,
            JobKind::AgentTurn,
            JobKind::AgentRequest,
            JobKind::ExternalProgram,
        ] {
            assert_eq!(kind.lane(), Lane::Blocking, "{kind:?}");
        }
    }
}
