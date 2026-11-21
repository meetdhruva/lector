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
//!   can never occupy every compute worker; work that other jobs or a person wait on runs on
//!   the guaranteed lane, which never queues behind a bound; git writes run on the serial lane,
//!   one per repository at a time (see [`Lane`]);
//! * the kind says whether it [writes](JobKind::is_write); everything about a write follows
//!   from that one predicate: never replaced, not cancelled with its subject or by dropping its
//!   task, runs once queued, drained at quit ([`AtQuit`]); the rest is cancelled.
//!
//! The priority of each kind is chosen by the caller; only the kinds listed in
//! [`JobKind::may_be_interactive`] may use [`Priority::Interactive`](super::Priority), the
//! others are run as `Visible`. The priorities call sites use today ("C" compute lane, "B"
//! blocking lane, "S" serial lane, "G" guaranteed lane):
//!
//! | Priority | Kinds | Why |
//! |---|---|---|
//! | Interactive (C, reserved worker) | DocumentParse, PickerRank, FindInFile | answer a keystroke; short; newest wins |
//! | Visible (C) | DocumentHighlight, DocumentStructure, DocumentOpen, DocumentReload, DocumentSave, FileRead, ImageDecode, MarkdownRender, ReplaceInFile, ProjectSearch, ProjectSymbols, PickerFeed, GitChangeset, GitFileSummary, GitBlame, GitLog, GitRefs, GitIdentity, GitCommitMessage, GitLookup, GitConfigRead, ConflictParse, JournalList, JournalPreview, ProvenanceExplain, ReviewAnalysis (review queue), AgentContext (attach a diff), TerminalSpawn, TerminalLink, TerminalSessions (restore) | on screen, or what the user just asked for |
//! | Visible (B) | LspHover, LspDefinitions, LspReferences, ForgeRead, ForgeWrite, AgentStart, NetworkCommand (sync operations), Command / CommandRead (`invoke_async`), StartupLoad, ExternalProgram (reveal) | the user waits, but the duration is somebody else's |
//! | Visible (S) | GitWrite (subject: the repository) | runs other people's programs (hooks, signing); one per repository, in order |
//! | Visible (G) | AskpassPrompt, AgentRequest, AgentControl, AgentUndo, AgentTurn | other jobs or the user wait on them |
//! | Background (C) | DocumentGutter, DocumentBlame, DocumentLsp, GitRefresh, GitSyncStatus, WatchRegister, FileRescan, Reindex, ReviewFingerprint, ReviewAnalysis (working-tree hint), AgentContext (turn summaries), GitLookup (the Changes panel's default base) | needed soon, not on the critical path |
//! | Background (B) | LspSync, LspLaunch, UpdateCheck | waits on a server or the network; nobody waits for it |
//! | Background (S) | TerminalSessionsSave | a write; a newer save never overtakes an older one |
//! | Background (G) | TerminalKill | a dying process tree; must run even when the blocking lane is full |
//! | Idle | SymbolIndex (C), ExternalProgram (B; the OS reduce-motion probe) | indexing and probes that can wait |
//!
//! Writes the user asked for (DocumentSave, GitWrite, Command, TerminalSessionsSave,
//! TerminalKill, AgentUndo: [`Writes::Local`]; NetworkCommand, ForgeWrite, AgentControl:
//! [`Writes::Remote`]) follow [`JobKind::is_write`]. DocumentSave stays on the compute lane: it
//! is bounded by the file's size, and format on save waits for the language server on the
//! main thread (an async request with a timeout, `lector_ui::lsp::format`) before the save
//! job is submitted, so no worker waits on a server.
//! ReplaceInFile only computes replacements (the edit is applied on the main thread), so it is
//! not a write.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Where a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lane {
    /// The fixed compute pool (CPU work, local disk and local `git`, bounded by input size).
    Compute,
    /// The bounded blocking lane: "leaf" waits whose length nobody here controls (network,
    /// language servers, external programs). Beyond its bound, jobs queue by priority.
    Blocking,
    /// One job per key at a time, in submission order, each key's jobs on a thread of their
    /// own (no bound across keys): git writes, one repository at a time, which git requires
    /// anyway. Not bounded by the compute workers nor counted against the blocking lane, so
    /// slow hooks in two repositories delay neither. Its threads are bounded by the keys with a
    /// write in flight (repositories, terminal panels). A serial job must never wait for a job
    /// with its own key (it would wait for itself); its kinds are all local writes.
    Serial,
    /// Waits that other jobs or a person wait on (a credential prompt a git command waits for,
    /// an agent's request, cancelling or undoing a turn, a terminal's kill escalation) and agent
    /// turns (at most one per session). Always start at once: no bound, so a full blocking
    /// lane of waiters can never keep the job they wait for from starting. Their number is
    /// bounded by their owners (prompt slots, requests in flight, sessions).
    Guaranteed,
}

/// What happens to a job of a kind when the app quits (`lector_app::shutdown`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AtQuit {
    /// Cancelled at once (reads, computations): its result has no one to go to.
    Cancel,
    /// A local write: queued ones run, running ones finish, within the drain bound; its child
    /// processes are stopped only after the drain.
    Drain,
    /// A write that waits on the network (push, forge comments) or on an agent: drained for a
    /// short grace only, since it can hang; then cancelled and reported.
    DrainBriefly,
}

/// Whether a kind writes something the user asked for, and how long quit waits for it. This
/// one column decides every write policy (see [`JobKind::is_write`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Writes {
    /// Reads or computes: replaced, cancelled with its subject or a dropped task, cancelled at
    /// quit.
    No,
    /// A local write: drained at quit up to the drain bound ([`AtQuit::Drain`]).
    Local,
    /// A write that waits on the network or an agent: drained for a short grace
    /// ([`AtQuit::DrainBriefly`]).
    Remote,
}

macro_rules! job_kinds {
    ($( $(#[$doc:meta])* $name:ident = ($lane:ident, $replaces:expr, $interactive:expr, $writes:ident) ),* $(,)?) => {
        /// The closed set of background job kinds. Adding one is a one-line change here; add it
        /// to the priority table in the module documentation too.
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

            /// Whether this kind writes something the user asked for, and how quit treats it.
            pub const fn writes(self) -> Writes {
                match self { $( JobKind::$name => Writes::$writes, )* }
            }

            /// The `replaces` column of the table (only honoured for kinds that do not write).
            const fn declared_replaces(self) -> bool {
                match self { $( JobKind::$name => $replaces, )* }
            }

            /// Whether this kind may run at `Priority::Interactive` (it directly answers a
            /// keystroke and is short). Other kinds asking for it run at `Visible`.
            pub const fn may_be_interactive(self) -> bool {
                match self { $( JobKind::$name => $interactive, )* }
            }
        }
    };
}

// (lane, replaces, may_be_interactive, writes)
//
// Everything about a write follows from the `writes` column (see `JobKind::is_write`).
// `replaces` only where one subject runs one such job at a time and the newest answer is the
// only one wanted (a query, a reload). Where one view runs several independent jobs of a kind
// (two menus, a list refresh and opening an item) the kind accumulates; a superseded job held in
// a field is still cancelled when its bridge task is dropped.
job_kinds! {
    // ---- documents (lector-app document store) ----
    /// Reparse after an edit.
    DocumentParse = (Compute, false, true, No),
    /// Syntax highlights of rows on screen.
    DocumentHighlight = (Compute, false, false, No),
    /// Outline, folds and structure queries.
    DocumentStructure = (Compute, true, false, No),
    /// Git gutter of a document.
    DocumentGutter = (Compute, true, false, No),
    /// Blame mapping of a document.
    DocumentBlame = (Compute, true, false, No),
    /// Syncing a document to its language server.
    DocumentLsp = (Compute, true, false, No),
    /// Opening (reading, decoding) a document for an editor.
    DocumentOpen = (Compute, true, false, No),
    /// Re-reading a document from disk / checking it against disk.
    DocumentReload = (Compute, true, false, No),
    /// Saving a document (a write the user asked for).
    DocumentSave = (Compute, false, false, Local),
    /// Reading a file for a view (not through the document store).
    FileRead = (Compute, false, false, No),
    /// Image metadata or an SVG rasterization.
    ImageDecode = (Compute, true, false, No),
    /// Markdown parsing for a preview or hover.
    MarkdownRender = (Compute, true, false, No),

    // ---- search and pickers ----
    /// Ranking a picker's rows for the newest query.
    PickerRank = (Compute, true, true, No),
    /// Feeding a picker its candidate list.
    PickerFeed = (Compute, true, false, No),
    /// Find in the current file as the user types.
    FindInFile = (Compute, true, true, No),
    /// Computing a replace-all.
    ReplaceInFile = (Compute, true, false, No),
    /// Project-wide text search.
    ProjectSearch = (Compute, true, false, No),
    /// Listing the project's symbols for the symbol picker.
    ProjectSymbols = (Compute, true, false, No),

    // ---- project ----
    /// Rescanning the project's file list.
    FileRescan = (Compute, true, false, No),
    /// Building the project's symbol index.
    SymbolIndex = (Compute, true, false, No),
    /// Re-indexing changed files.
    Reindex = (Compute, false, false, No),
    /// Registering recursive file watches (walks the tree once).
    WatchRegister = (Compute, true, false, No),
    /// Loading settings, the project and themes at startup.
    StartupLoad = (Blocking, true, false, No),
    /// Running a helper program of the OS (reveal in the file manager, preference probes).
    ExternalProgram = (Blocking, false, false, No),

    // ---- language servers ----
    /// Hover from a language server.
    LspHover = (Blocking, true, false, No),
    /// Go to definition.
    LspDefinitions = (Blocking, true, false, No),
    /// Find references.
    LspReferences = (Blocking, true, false, No),
    /// Opening a document on a language server from the UI.
    LspSync = (Blocking, true, false, No),
    /// Launching or restarting a language server (waits for `initialize`). Several servers
    /// may launch at once: launches accumulate.
    LspLaunch = (Blocking, false, false, No),

    // ---- git (local) ----
    /// Re-reading HEAD, status and conflicts of a repository.
    GitRefresh = (Compute, true, false, No),
    /// Sync status (ahead/behind, upstream).
    GitSyncStatus = (Compute, true, false, No),
    /// Branches, stashes, refs, unreachable commits: lists for a menu or panel.
    GitRefs = (Compute, false, false, No),
    /// The committer identity.
    GitIdentity = (Compute, true, false, No),
    /// The last commit message (amend).
    GitCommitMessage = (Compute, true, false, No),
    /// Commit history.
    GitLog = (Compute, true, false, No),
    /// Blame of a file.
    GitBlame = (Compute, true, false, No),
    /// A changeset (diff) for a panel, a tab or a diff view.
    GitChangeset = (Compute, true, false, No),
    /// One file's diff summary.
    GitFileSummary = (Compute, true, false, No),
    /// A permalink or a revision lookup.
    GitLookup = (Compute, false, false, No),
    /// Credential helpers git is configured with.
    GitConfigRead = (Compute, true, false, No),
    /// Conflict parsing for inline conflicts or the merge editor.
    ConflictParse = (Compute, true, false, No),
    /// A write command the user asked for (stage, commit, discard, undo...). Its subject is
    /// the repository ([`Subject::repo`]): one git write per repository at a time, in
    /// submission order, on threads of its own (hooks, signing and pinentry can take minutes).
    GitWrite = (Serial, false, false, Local),
    /// A writing registry command run off the caller's thread (`CommandRegistry::invoke_async`,
    /// palette actions): it may run git, so the blocking lane; never cancelled with a subject.
    Command = (Blocking, false, false, Local),
    /// A reading registry command run off the caller's thread.
    CommandRead = (Blocking, false, false, No),
    /// A git network operation (fetch, pull, push, sync flows): waits on the remote.
    NetworkCommand = (Blocking, false, false, Remote),

    // ---- review ----
    /// Review analysis of changes (risk, structure, queue).
    ReviewAnalysis = (Compute, true, false, No),
    /// Fingerprint of the working tree changes.
    ReviewFingerprint = (Compute, true, false, No),
    /// Provenance explanation for a hover.
    ProvenanceExplain = (Compute, true, false, No),
    /// The activity journal list.
    JournalList = (Compute, true, false, No),
    /// A preview of what undoing a journal entry restores.
    JournalPreview = (Compute, true, false, No),

    // ---- network ----
    /// A forge request that only reads (pull requests, opening a review).
    ForgeRead = (Blocking, false, false, No),
    /// A forge request that writes (reply, resolve, submit a review).
    ForgeWrite = (Blocking, false, false, Remote),
    /// Checking for an update.
    UpdateCheck = (Blocking, true, false, No),
    /// One credential prompt connection (waits for the user).
    AskpassPrompt = (Guaranteed, false, false, No),

    // ---- agents ----
    /// Starting or restarting an agent session (spawns and initializes the agent).
    AgentStart = (Blocking, true, false, No),
    /// One agent turn.
    AgentTurn = (Guaranteed, false, false, No),
    /// One request from the agent (may wait for the user's permission).
    AgentRequest = (Guaranteed, false, false, No),
    /// Cancelling or stopping a turn (talks to the agent; a short grace at quit).
    AgentControl = (Guaranteed, false, false, Remote),
    /// Undoing a turn: restores the turn's files locally, so quit waits for it to finish
    /// (a restore cut off halfway would leave files partly restored).
    AgentUndo = (Guaranteed, false, false, Local),
    /// Computing a turn summary or the diff context for the agent.
    AgentContext = (Compute, false, false, No),

    // ---- terminal ----
    /// Starting a terminal session.
    TerminalSpawn = (Compute, true, false, No),
    /// Loading the terminal tabs of a project.
    TerminalSessions = (Compute, false, false, No),
    /// Saving the terminal tabs of a project (a write). Serial per panel, so a newer save
    /// never lands before an older one that is still queued.
    TerminalSessionsSave = (Serial, false, false, Local),
    /// Resolving a clicked link in a terminal.
    TerminalLink = (Compute, true, false, No),
    /// Killing a terminal's process tree after its grace period.
    TerminalKill = (Guaranteed, false, false, Local),
}

impl JobKind {
    /// Whether this kind writes something the user asked for. The one predicate behind every
    /// write policy, so they cannot drift apart:
    /// * a write is never [replaced](JobKind::replaces) by a newer job with its key;
    /// * it is not [cancelled with its subject](JobKind::cancelled_with_subject);
    /// * dropping its bridge task [detaches it](JobKind::detaches_on_drop) instead of cancelling;
    /// * it is drained at quit ([`JobKind::at_quit`]);
    /// * once queued it [runs](JobKind::runs_once_queued) even if its token was cancelled before
    ///   it started; only [`Scheduler::cancel_key`](super::Scheduler::cancel_key) (before it
    ///   starts), a refusal while quitting or shutdown keep it from running.
    pub const fn is_write(self) -> bool {
        !matches!(self.writes(), Writes::No)
    }

    /// Whether a newer job with the same key cancels the older ones. Kinds that do not
    /// replace accumulate (several highlight batches of one document, a write per click).
    /// Never for a write.
    pub const fn replaces(self) -> bool {
        !self.is_write() && self.declared_replaces()
    }

    /// Whether [`Scheduler::cancel_subject`](super::Scheduler::cancel_subject) cancels
    /// jobs of this kind. `false` for writes the user asked for: closing the window
    /// right after "Commit" must not lose the commit.
    pub const fn cancelled_with_subject(self) -> bool {
        !self.is_write()
    }

    /// Whether dropping the bridge's task (`lector_ui::background::BackgroundTask`) lets the
    /// job finish unobserved instead of cancelling it: true for writes.
    pub const fn detaches_on_drop(self) -> bool {
        self.is_write()
    }

    /// Whether a queued job runs even when its token was cancelled before it started (its
    /// result is then dropped): true for writes. Other kinds are skipped.
    pub const fn runs_once_queued(self) -> bool {
        self.is_write()
    }

    /// What happens to jobs of this kind at quit: reads are cancelled, local writes drained,
    /// network writes drained briefly.
    pub const fn at_quit(self) -> AtQuit {
        match self.writes() {
            Writes::No => AtQuit::Cancel,
            Writes::Local => AtQuit::Drain,
            Writes::Remote => AtQuit::DrainBriefly,
        }
    }

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
            JobKind::AgentUndo,
            JobKind::Command,
            JobKind::NetworkCommand,
            JobKind::TerminalSessionsSave,
            JobKind::TerminalKill,
        ] {
            assert!(kind.is_write(), "{kind:?}");
            assert!(!kind.replaces(), "{kind:?}");
            assert!(!kind.cancelled_with_subject(), "{kind:?}");
        }
    }

    #[test]
    fn no_write_row_declares_replaces() {
        for &kind in JobKind::ALL {
            assert!(!(kind.is_write() && kind.declared_replaces()), "{kind:?}");
        }
    }

    /// The quit drain relies on it: it never drops a serial-lane job out of order.
    #[test]
    fn serial_kinds_are_local_writes() {
        for &kind in JobKind::ALL {
            if kind.lane() == Lane::Serial {
                assert_eq!(kind.at_quit(), AtQuit::Drain, "{kind:?}");
            }
        }
    }

    #[test]
    fn git_writes_are_serial_and_undo_is_drained_fully() {
        assert_eq!(JobKind::GitWrite.lane(), Lane::Serial);
        assert_eq!(JobKind::DocumentSave.lane(), Lane::Compute);
        assert_eq!(JobKind::AgentUndo.at_quit(), AtQuit::Drain);
        assert_eq!(JobKind::AgentControl.at_quit(), AtQuit::DrainBriefly);
    }

    #[test]
    fn waits_on_others_run_on_the_blocking_lane() {
        for kind in [
            JobKind::LspHover,
            JobKind::LspLaunch,
            JobKind::ForgeRead,
            JobKind::UpdateCheck,
            JobKind::NetworkCommand,
            JobKind::ExternalProgram,
        ] {
            assert_eq!(kind.lane(), Lane::Blocking, "{kind:?}");
        }
    }

    #[test]
    fn what_others_wait_on_has_a_guaranteed_start() {
        for kind in [
            JobKind::AskpassPrompt,
            JobKind::AgentRequest,
            JobKind::AgentControl,
            JobKind::AgentUndo,
            JobKind::AgentTurn,
            JobKind::TerminalKill,
        ] {
            assert_eq!(kind.lane(), Lane::Guaranteed, "{kind:?}");
        }
    }

    #[test]
    fn every_kind_that_survives_its_subject_is_drained_at_quit() {
        for &kind in JobKind::ALL {
            assert_eq!(
                kind.cancelled_with_subject(),
                kind.at_quit() == AtQuit::Cancel,
                "{kind:?}"
            );
        }
    }
}
