//! UI-free logic of "sync with the remote" (v0.7): the state of the current branch
//! relative to its upstream and what to offer for it, the commands each sync action runs,
//! the gate in front of dangerous ones (force with lease), the next step offered for each
//! structured failure, branch-name validation as you type, and the classification of
//! credential prompts into dialogs. Everything here is plain data so it is unit-tested
//! without a window; [`crate::sync`] renders it and runs the commands.

use std::time::{Duration, SystemTime};

use lector_app::commands::git_sync::askpass::{self, PromptKind};
use lector_git::{PullMode, SyncError, SyncErrorKind, UpstreamStatus};
use serde_json::{json, Value};

/// A configured remote (URL with credentials removed).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    /// Remote name (`origin`).
    pub name: String,
    /// Fetch URL, credentials redacted.
    pub url: String,
}

/// What the sync control knows about the current branch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SyncStatus {
    /// `false` until the first load finished.
    pub loaded: bool,
    /// Current branch, `None` when detached.
    pub branch: Option<String>,
    /// `HEAD` is detached.
    pub detached: bool,
    /// Short sha of `HEAD` (detached label).
    pub head_short: Option<String>,
    /// Upstream (`origin/main`) if configured and present.
    pub upstream: Option<String>,
    /// An upstream is configured but its ref is gone.
    pub upstream_gone: bool,
    /// The remote a push goes to by default.
    pub push_remote: Option<String>,
    /// Commits to push.
    pub ahead: u32,
    /// Commits to pull.
    pub behind: u32,
    /// Configured remotes.
    pub remotes: Vec<RemoteInfo>,
    /// When `FETCH_HEAD` was last written (last fetch / pull).
    pub last_fetch: Option<SystemTime>,
}

impl SyncStatus {
    /// Builds the status from [`UpstreamStatus`] and the remotes.
    pub fn from_upstream(
        st: &UpstreamStatus,
        head_short: Option<String>,
        remotes: Vec<RemoteInfo>,
        last_fetch: Option<SystemTime>,
    ) -> SyncStatus {
        SyncStatus {
            loaded: true,
            branch: st.branch.clone().filter(|_| !st.detached),
            detached: st.detached,
            head_short,
            upstream: st.upstream.clone(),
            upstream_gone: st.upstream_gone,
            push_remote: st.push_remote.clone(),
            ahead: st.ahead,
            behind: st.behind,
            remotes,
            last_fetch,
        }
    }

    /// The sync state.
    pub fn state(&self) -> SyncState {
        if !self.loaded {
            return SyncState::Loading;
        }
        if self.detached || self.branch.is_none() {
            return SyncState::Detached;
        }
        if self.remotes.is_empty() {
            return SyncState::NoRemote;
        }
        if self.upstream_gone {
            return SyncState::UpstreamGone;
        }
        if self.upstream.is_none() {
            return SyncState::NoUpstream;
        }
        match (self.ahead, self.behind) {
            (0, 0) => SyncState::UpToDate,
            (_, 0) => SyncState::NeedsPush,
            (0, _) => SyncState::NeedsPull,
            _ => SyncState::Diverged,
        }
    }

    /// Where a push of the current branch goes: `origin/main` (the upstream, or the push
    /// remote and the branch name).
    pub fn push_target(&self) -> Option<String> {
        let branch = self.branch.as_deref()?;
        match (&self.upstream, &self.push_remote) {
            (Some(up), Some(remote)) if up.starts_with(&format!("{remote}/")) => Some(up.clone()),
            (_, Some(remote)) => Some(format!("{remote}/{branch}")),
            (Some(up), None) => Some(up.clone()),
            (None, None) => None,
        }
    }

    /// The URL of the remote a push / fetch talks to (redacted), for prompts and errors.
    pub fn remote_url(&self) -> Option<&str> {
        let name = self
            .push_remote
            .as_deref()
            .or_else(|| self.upstream.as_deref().and_then(|u| u.split('/').next()))?;
        self.remotes
            .iter()
            .find(|r| r.name == name)
            .or_else(|| self.remotes.first())
            .map(|r| r.url.as_str())
    }
}

/// The state of the current branch relative to its upstream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyncState {
    /// Not loaded yet.
    Loading,
    /// Same commits as the upstream (as of the last fetch).
    UpToDate,
    /// Local commits to push.
    NeedsPush,
    /// Upstream commits to pull.
    NeedsPull,
    /// Both: pull (rebase) first.
    Diverged,
    /// No upstream: the branch was never pushed ("Publish branch").
    NoUpstream,
    /// The upstream was deleted on the remote.
    UpstreamGone,
    /// `HEAD` is detached: nothing to sync.
    Detached,
    /// The repository has no remote.
    NoRemote,
}

/// A network sync operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyncAction {
    /// `git fetch --all`.
    Fetch,
    /// `git pull --ff-only`.
    Pull,
    /// `git pull --rebase`.
    PullRebase,
    /// `git push` (sets the upstream when there is none).
    Push,
    /// `git push --force-with-lease=<ref>:<expected>`; always confirmed first.
    ForcePushWithLease,
    /// Push and set the upstream.
    Publish,
}

impl SyncAction {
    /// Every action (tests, menus).
    pub const ALL: [SyncAction; 6] = [
        SyncAction::Fetch,
        SyncAction::Pull,
        SyncAction::PullRebase,
        SyncAction::Push,
        SyncAction::ForcePushWithLease,
        SyncAction::Publish,
    ];

    /// Menu label.
    pub fn label(self) -> &'static str {
        match self {
            SyncAction::Fetch => "Fetch",
            SyncAction::Pull => "Pull",
            SyncAction::PullRebase => "Pull (Rebase)",
            SyncAction::Push => "Push",
            SyncAction::ForcePushWithLease => "Force Push with Lease…",
            SyncAction::Publish => "Publish Branch",
        }
    }

    /// Progressive verb for the running label ("Pushing").
    pub fn running_verb(self) -> &'static str {
        match self {
            SyncAction::Fetch => "Fetching",
            SyncAction::Pull | SyncAction::PullRebase => "Pulling",
            SyncAction::Push | SyncAction::ForcePushWithLease => "Pushing",
            SyncAction::Publish => "Publishing",
        }
    }

    /// The `git::*` action that runs it from menus, palette and keys.
    pub fn action_name(self) -> &'static str {
        match self {
            SyncAction::Fetch => "git::Fetch",
            SyncAction::Pull => "git::Pull",
            SyncAction::PullRebase => "git::PullRebase",
            SyncAction::Push => "git::Push",
            SyncAction::ForcePushWithLease => "git::ForcePushWithLease",
            SyncAction::Publish => "git::PublishBranch",
        }
    }

    /// The registry command and its arguments.
    pub fn command(self) -> (&'static str, Value) {
        match self {
            SyncAction::Fetch => ("git.fetch", json!({})),
            SyncAction::Pull => ("git.pull", json!({ "mode": "ff_only" })),
            SyncAction::PullRebase => ("git.pull", json!({ "mode": "rebase" })),
            SyncAction::Push => ("git.push", json!({})),
            SyncAction::ForcePushWithLease => ("git.push", json!({ "force_with_lease": true })),
            SyncAction::Publish => ("git.push", json!({ "set_upstream": true })),
        }
    }

    /// The pull mode of a pull action.
    pub fn pull_mode(self) -> Option<PullMode> {
        match self {
            SyncAction::Pull => Some(PullMode::FastForwardOnly),
            SyncAction::PullRebase => Some(PullMode::Rebase),
            _ => None,
        }
    }
}

/// What a click on the status bar control does, by state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryAction {
    /// Run a sync action.
    Sync(SyncAction),
    /// Create a branch (detached `HEAD`).
    NewBranch,
    /// Nothing to run; the tooltip explains (no remote, loading).
    None,
}

impl PrimaryAction {
    /// The `git::*` action name it dispatches.
    pub fn action_name(self) -> Option<&'static str> {
        match self {
            PrimaryAction::Sync(a) => Some(a.action_name()),
            PrimaryAction::NewBranch => Some("git::NewBranch"),
            PrimaryAction::None => None,
        }
    }
}

/// The status bar control for a state: branch label, counts as text, the state word, the
/// click action and the tooltip. Color is never the only signal: the state word and the
/// `↑`/`↓` counts are text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlView {
    /// Branch name or `detached abc1234`.
    pub branch: String,
    /// `↑2 ↓1` (empty when both are zero or there is no upstream).
    pub counts: String,
    /// `up to date`, `push`, `pull`, `diverged`, `publish`, `detached`, `no remote`, ...
    pub state_word: &'static str,
    /// What a click does.
    pub primary: PrimaryAction,
    /// Tooltip title.
    pub tooltip: String,
}

/// Formats `↑a ↓b`, leaving out zero counts.
pub fn counts_label(ahead: u32, behind: u32) -> String {
    let mut parts = Vec::new();
    if ahead > 0 {
        parts.push(format!("↑{ahead}"));
    }
    if behind > 0 {
        parts.push(format!("↓{behind}"));
    }
    parts.join(" ")
}

fn commits(n: u32) -> String {
    if n == 1 {
        "1 commit".into()
    } else {
        format!("{n} commits")
    }
}

/// The control for `status`.
pub fn control_view(status: &SyncStatus) -> ControlView {
    let state = status.state();
    let branch = match (&status.branch, &status.head_short) {
        (Some(b), _) if !status.detached => b.clone(),
        (_, Some(short)) => format!("detached {short}"),
        _ => "detached".into(),
    };
    let up = status.upstream.clone().unwrap_or_default();
    let target = status.push_target().unwrap_or_else(|| "the remote".into());
    let (state_word, primary, tooltip) = match state {
        SyncState::Loading => ("", PrimaryAction::None, "Reading the branch state…".into()),
        SyncState::UpToDate => (
            "up to date",
            PrimaryAction::Sync(SyncAction::Fetch),
            format!("{branch} is up to date with {up} (as of the last fetch). Click to fetch."),
        ),
        SyncState::NeedsPush => (
            "push",
            PrimaryAction::Sync(SyncAction::Push),
            format!("{} to push to {up}. Click to push.", commits(status.ahead)),
        ),
        SyncState::NeedsPull => (
            "pull",
            PrimaryAction::Sync(SyncAction::Pull),
            format!(
                "{} to pull from {up}. Click to pull (fast-forward).",
                commits(status.behind)
            ),
        ),
        SyncState::Diverged => (
            "diverged",
            PrimaryAction::Sync(SyncAction::PullRebase),
            format!(
                "{} to push and {} to pull: {branch} and {up} have diverged. Click to pull with \
                 rebase (replays your commits on top of {up}; undoable), then push.",
                commits(status.ahead),
                commits(status.behind)
            ),
        ),
        SyncState::NoUpstream => (
            "publish",
            PrimaryAction::Sync(SyncAction::Publish),
            format!("{branch} has no upstream. Click to publish it to {target}."),
        ),
        SyncState::UpstreamGone => (
            "upstream gone",
            PrimaryAction::Sync(SyncAction::Publish),
            format!(
                "The upstream of {branch} was deleted on the remote. Click to publish it again \
                 to {target}."
            ),
        ),
        SyncState::Detached => (
            "detached",
            PrimaryAction::NewBranch,
            "HEAD is detached: create a branch to push your work.".into(),
        ),
        SyncState::NoRemote => (
            "no remote",
            PrimaryAction::None,
            "This repository has no remote. Add one in a terminal: git remote add origin <url>"
                .into(),
        ),
    };
    let counts = if status.upstream.is_some() {
        counts_label(status.ahead, status.behind)
    } else {
        String::new()
    };
    ControlView {
        branch,
        counts,
        state_word,
        primary,
        tooltip,
    }
}

/// "Last fetched 12 min ago" (`None` = never fetched in this clone).
pub fn last_fetched_label(last: Option<SystemTime>, now: SystemTime) -> String {
    let Some(last) = last else {
        return "Never fetched".into();
    };
    let secs = now.duration_since(last).unwrap_or(Duration::ZERO).as_secs();
    let ago = match secs {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86_399 => {
            let h = secs / 3600;
            format!("{h} hour{} ago", if h == 1 { "" } else { "s" })
        }
        _ => {
            let d = secs / 86_400;
            format!("{d} day{} ago", if d == 1 { "" } else { "s" })
        }
    };
    format!("Last fetched {ago}")
}

// ---------------------------------------------------------------------------------------------
// Plans: what running an action means
// ---------------------------------------------------------------------------------------------

/// What the force-push confirmation says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForceConfirm {
    /// Dialog title.
    pub title: String,
    /// Explanation (names the remote branch, says commits there may be discarded).
    pub message: Vec<String>,
    /// Destructive button label.
    pub confirm_label: String,
}

/// What happens when the user asks for an action.
#[derive(Clone, Debug, PartialEq)]
pub enum Plan {
    /// Run these commands in order.
    Run(Vec<(&'static str, Value)>),
    /// Ask first; run the commands only after an explicit confirmation.
    Confirm(ForceConfirm, Vec<(&'static str, Value)>),
    /// Not possible now; show why.
    Blocked(String),
}

/// The plan for `action` given the branch state and whether an operation already runs in
/// this repository (`busy` = its label). Force push is only ever confirmed, never run
/// directly; plain `--force` does not exist.
pub fn plan(action: SyncAction, status: &SyncStatus, busy: Option<&str>) -> Plan {
    if let Some(what) = busy {
        return Plan::Blocked(format!(
            "{what} is still running in this repository. Wait for it to finish or cancel it."
        ));
    }
    let state = status.state();
    let needs_branch = !matches!(action, SyncAction::Fetch);
    if needs_branch && state == SyncState::Detached {
        return Plan::Blocked("HEAD is detached: create a branch first.".into());
    }
    if state == SyncState::NoRemote {
        return Plan::Blocked(
            "This repository has no remote. Add one in a terminal: git remote add origin <url>"
                .into(),
        );
    }
    if action.pull_mode().is_some() && status.upstream.is_none() && status.loaded {
        return Plan::Blocked(format!(
            "{} has no upstream to pull from. Publish it or set an upstream first.",
            status.branch.as_deref().unwrap_or("The branch")
        ));
    }
    let cmd = action.command();
    match action {
        SyncAction::ForcePushWithLease => {
            let target = status
                .push_target()
                .unwrap_or_else(|| "the remote branch".into());
            Plan::Confirm(
                ForceConfirm {
                    title: format!("Force push to {target}?"),
                    message: vec![
                        format!(
                            "This replaces {target} with your local {}. Commits on {target} that \
                             are not in your branch may be discarded for everyone.",
                            status.branch.as_deref().unwrap_or("branch")
                        ),
                        format!(
                            "Lector uses --force-with-lease: the push is refused if {target} \
                             moved since your last fetch. A push cannot be undone."
                        ),
                    ],
                    confirm_label: "Force Push".into(),
                },
                vec![cmd],
            )
        }
        _ => Plan::Run(vec![cmd]),
    }
}

// ---------------------------------------------------------------------------------------------
// Failures and next steps
// ---------------------------------------------------------------------------------------------

/// The operation that failed (for the message and the next steps).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OpKind {
    /// A network sync action.
    Sync(SyncAction),
    /// Stash, then pull.
    StashAndPull(SyncAction),
    /// Switch to a branch.
    Switch(String),
    /// Create a branch.
    CreateBranch(String),
    /// Rename a branch.
    RenameBranch(String),
    /// Delete a branch.
    DeleteBranch(String),
    /// Set the upstream.
    SetUpstream,
    /// Stash changes.
    Stash,
    /// Apply / pop / drop a stash.
    StashApply(&'static str),
}

impl OpKind {
    /// "Push", "Switch to feature", ...
    pub fn title(&self) -> String {
        match self {
            OpKind::Sync(a) => a.label().trim_end_matches('…').to_string(),
            OpKind::StashAndPull(_) => "Stash and pull".into(),
            OpKind::Switch(b) => format!("Switch to {b}"),
            OpKind::CreateBranch(b) => format!("Create branch {b}"),
            OpKind::RenameBranch(b) => format!("Rename {b}"),
            OpKind::DeleteBranch(b) => format!("Delete {b}"),
            OpKind::SetUpstream => "Set upstream".into(),
            OpKind::Stash => "Stash".into(),
            OpKind::StashApply(verb) => format!("{verb} stash"),
        }
    }

    /// Whether this is a network operation (progress, cancel, credential prompts).
    pub fn is_network(&self) -> bool {
        matches!(self, OpKind::Sync(_) | OpKind::StashAndPull(_))
    }
}

/// A button offered after a failure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NextStep {
    /// Pull (fast-forward), then offer to push again.
    PullFirst,
    /// Pull with rebase.
    PullRebase,
    /// Push and set the upstream.
    Publish,
    /// Choose an upstream.
    SetUpstream,
    /// Fetch (then decide).
    Fetch,
    /// Stash local changes (including untracked), then pull with this action.
    StashAndPull(SyncAction),
    /// Stash local changes, then switch to the branch.
    StashAndSwitch(String),
    /// Run the same operation again (after fixing credentials, network, the hook...).
    Retry,
    /// Create a branch (detached `HEAD`).
    NewBranch,
    /// Show the Changes panel with the conflicts and the operation banner.
    ShowConflicts,
}

impl NextStep {
    /// Button label.
    pub fn label(&self) -> String {
        match self {
            NextStep::PullFirst => "Pull First".into(),
            NextStep::PullRebase => "Pull (Rebase)".into(),
            NextStep::Publish => "Publish Branch".into(),
            NextStep::SetUpstream => "Set Upstream…".into(),
            NextStep::Fetch => "Fetch".into(),
            NextStep::StashAndPull(_) => "Stash and Pull".into(),
            NextStep::StashAndSwitch(_) => "Stash Changes and Switch".into(),
            NextStep::Retry => "Retry".into(),
            NextStep::NewBranch => "New Branch…".into(),
            NextStep::ShowConflicts => "Show Conflicts".into(),
        }
    }
}

/// What the failure dialog shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FailureView {
    /// The structured kind (`None` for unclassified errors).
    pub kind: Option<SyncErrorKind>,
    /// "Push failed".
    pub title: String,
    /// What happened and what to do, in words.
    pub message: Vec<String>,
    /// Files involved (local changes that would be overwritten).
    pub paths: Vec<String>,
    /// Hook output or git's output, shown in the monospace output box.
    pub details: Option<String>,
    /// The details are the output of a hook that declined.
    pub details_are_hook_output: bool,
    /// Buttons, most useful first.
    pub steps: Vec<NextStep>,
}

/// What failure messages can say about credentials (filled in on auth failures).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FailureContext {
    /// Redacted URL of the remote.
    pub remote_url: Option<String>,
    /// `credential.helper` values git is configured with.
    pub credential_helpers: Vec<String>,
    /// Whether Lector showed a credential prompt during the operation, and whether it was
    /// answered (`Some(true)`) or cancelled (`Some(false)`).
    pub prompt_answered: Option<bool>,
    /// An ssh agent is reachable (`SSH_AUTH_SOCK` set).
    pub ssh_agent: bool,
}

fn is_ssh_url(url: &str) -> bool {
    url.starts_with("ssh://")
        || url.starts_with("git@")
        || (!url.contains("://") && url.contains('@') && url.contains(':'))
}

/// Which credential sources git tried, in words (auth failures).
pub fn credential_sources(ctx: &FailureContext) -> String {
    let url = ctx.remote_url.as_deref().unwrap_or("");
    let prompt = match ctx.prompt_answered {
        Some(true) => "Lector asked you and the server refused that answer.",
        Some(false) => "Lector's credential prompt was cancelled.",
        None => "Lector was not asked for credentials.",
    };
    if is_ssh_url(url) {
        let agent = if ctx.ssh_agent {
            "your ssh-agent (SSH_AUTH_SOCK)"
        } else {
            "no ssh-agent (SSH_AUTH_SOCK is not set)"
        };
        format!(
            "This remote uses SSH. ssh tried {agent} and your key files (~/.ssh/id_*, \
             ~/.ssh/config); a key passphrase is asked through Lector. {prompt}"
        )
    } else {
        let helper = if ctx.credential_helpers.is_empty() {
            "No git credential helper is configured, so nothing was stored.".to_string()
        } else {
            format!(
                "git asked its credential helper first ({}); it may have offered a stored \
                 password or token that is no longer valid.",
                ctx.credential_helpers.join(", ")
            )
        };
        format!("{helper} {prompt} For HTTPS remotes use a personal access token as the password.")
    }
}

/// The failure dialog for `err` from `op`, with the next steps that make sense.
pub fn failure_view(op: &OpKind, err: &SyncError, ctx: &FailureContext) -> FailureView {
    let title = format!("{} failed", op.title());
    let mut message: Vec<String> = Vec::new();
    let mut steps: Vec<NextStep> = Vec::new();
    let mut details = None;
    let mut hook = false;
    let first = |s: &str| {
        let mut c = s.chars();
        c.next()
            .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
            .unwrap_or_default()
    };
    let pull_action = match op {
        OpKind::Sync(a) | OpKind::StashAndPull(a) if a.pull_mode().is_some() => Some(*a),
        _ => None,
    };
    match err.kind {
        SyncErrorKind::NoUpstream => {
            message.push("The branch has no upstream on the remote yet.".into());
            steps.push(NextStep::Publish);
            steps.push(NextStep::SetUpstream);
        }
        SyncErrorKind::NoRemote => {
            message.push(
                "There is no such remote. Add one in a terminal: git remote add origin <url>"
                    .into(),
            );
        }
        SyncErrorKind::DetachedHead => {
            message.push("HEAD is detached. Create a branch for your work first.".into());
            steps.push(NextStep::NewBranch);
        }
        SyncErrorKind::Rejected => {
            message.push(
                "The remote has commits you don't have. Pull them first (fast-forward or \
                 rebase), then push again."
                    .into(),
            );
            steps.push(NextStep::PullFirst);
            steps.push(NextStep::PullRebase);
        }
        SyncErrorKind::StaleLease => {
            message.push(
                "The remote branch moved since your last fetch, so the force push was refused \
                 and nothing was overwritten. Fetch and look at the new commits before deciding."
                    .into(),
            );
            steps.push(NextStep::Fetch);
        }
        SyncErrorKind::AuthenticationFailed => {
            message.push(format!(
                "The remote refused the credentials{}.",
                ctx.remote_url
                    .as_deref()
                    .map(|u| format!(" for {u}"))
                    .unwrap_or_default()
            ));
            message.push(credential_sources(ctx));
            steps.push(NextStep::Retry);
        }
        SyncErrorKind::HostKeyUnknown => {
            message.push(
                "ssh does not know this server's host key and it was not confirmed. Retry and \
                 check the fingerprint in the dialog against the one your host publishes."
                    .into(),
            );
            steps.push(NextStep::Retry);
        }
        SyncErrorKind::HostKeyChanged => {
            message.push(
                "WARNING: the server's host key differs from the one in your known_hosts file. \
                 This can mean someone is intercepting the connection. Lector will not connect. \
                 If the host really changed its key, verify the new fingerprint with its \
                 administrators and remove the old entry (ssh-keygen -R <host>)."
                    .into(),
            );
        }
        SyncErrorKind::NetworkUnreachable => {
            message.push(
                "The remote could not be reached (offline, DNS, proxy, firewall or TLS). Check \
                 your connection and retry."
                    .into(),
            );
            steps.push(NextStep::Retry);
        }
        SyncErrorKind::RemoteNotFound => {
            message.push(
                "The remote repository was not found, or you have no access to it. Check the \
                 URL and your permissions."
                    .into(),
            );
            steps.push(NextStep::Retry);
        }
        SyncErrorKind::HookDeclined => {
            message.push(
                "A hook declined the operation. Its output is below; fix what it reports and \
                 retry."
                    .into(),
            );
            details = err
                .hook_output
                .clone()
                .or_else(|| Some(err.message.clone()));
            hook = true;
            steps.push(NextStep::Retry);
        }
        SyncErrorKind::Diverged => {
            message.push(
                "Your branch and its upstream have diverged, so a fast-forward is not possible. \
                 Pull with rebase to replay your commits on top (undoable)."
                    .into(),
            );
            steps.push(NextStep::PullRebase);
        }
        SyncErrorKind::LocalChanges | SyncErrorKind::DirtyWorktree => {
            message.push(
                "Your local changes would be overwritten. Stash them first (they stay in the \
                 stash list until you apply them), or commit them."
                    .into(),
            );
            match op {
                OpKind::Switch(name) => steps.push(NextStep::StashAndSwitch(name.clone())),
                _ => {
                    if let Some(a) = pull_action {
                        steps.push(NextStep::StashAndPull(a));
                    }
                }
            }
        }
        SyncErrorKind::NotMerged => {
            message.push(
                "The branch is not fully merged: deleting it would leave commits reachable from \
                 no branch."
                    .into(),
            );
        }
        SyncErrorKind::InvalidName
        | SyncErrorKind::AlreadyExists
        | SyncErrorKind::NotFound
        | SyncErrorKind::RefChanged => {
            message.push(format!("{}.", first(err.kind.describe())));
        }
        SyncErrorKind::Other => {
            message.push("git reported an error (below).".into());
            steps.push(NextStep::Retry);
        }
    }
    if let Some(hint) = err.hint.as_deref().filter(|h| !h.is_empty()) {
        if !message.iter().any(|m| m.contains(hint)) {
            message.push(hint.to_string());
        }
    }
    if details.is_none() && !err.message.trim().is_empty() {
        details = Some(err.message.trim().to_string());
    }
    FailureView {
        kind: Some(err.kind),
        title,
        message,
        paths: err.paths.clone(),
        details,
        details_are_hook_output: hook,
        steps,
    }
}

/// A failure that is not a classified [`SyncError`] (I/O, git missing, ...).
pub fn generic_failure(op: &OpKind, text: &str) -> FailureView {
    FailureView {
        kind: None,
        title: format!("{} failed", op.title()),
        message: vec!["git reported an error (below).".into()],
        paths: Vec::new(),
        details: Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        details_are_hook_output: false,
        steps: vec![NextStep::Retry],
    }
}

// ---------------------------------------------------------------------------------------------
// Outcomes
// ---------------------------------------------------------------------------------------------

fn s(v: &Value, k: &str) -> String {
    v.get(k)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn n(v: &Value, k: &str) -> u64 {
    v.get(k).and_then(Value::as_u64).unwrap_or(0)
}

/// What a successful operation reports, from the command's structured data.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SuccessView {
    /// Toast text ("Pulled 3 commits", "Pushed main → origin/main", "Already up to date").
    pub message: String,
    /// A rebase pull stopped with conflicts: hand over to the conflict flow.
    pub conflicts: bool,
    /// Commits pulled (for "Pull first, then push again").
    pub pulled: u64,
}

/// The toast for a successful `action` with the command's `data` and `summary`.
pub fn success_view(action: SyncAction, data: &Value, summary: &str) -> SuccessView {
    let mut view = SuccessView {
        message: summary.to_string(),
        conflicts: false,
        pulled: 0,
    };
    match action {
        SyncAction::Fetch => {
            let updated = data
                .get("updated")
                .and_then(Value::as_array)
                .map_or(0, Vec::len);
            view.message = match updated {
                0 => "Fetched: already up to date".into(),
                1 => "Fetched 1 updated ref".into(),
                n => format!("Fetched {n} updated refs"),
            };
        }
        SyncAction::Pull | SyncAction::PullRebase => {
            let incoming = n(data, "incoming");
            view.pulled = incoming;
            view.message = match s(data, "status").as_str() {
                "up_to_date" => "Already up to date".into(),
                "fast_forwarded" => format!(
                    "Pulled {} into {}",
                    commits(u32::try_from(incoming).unwrap_or(u32::MAX)),
                    s(data, "branch")
                ),
                "rebased" => format!(
                    "Pulled {} and rebased {} on {}",
                    commits(u32::try_from(incoming).unwrap_or(u32::MAX)),
                    commits(u32::try_from(n(data, "rebased")).unwrap_or(u32::MAX)),
                    s(data, "upstream")
                ),
                "conflicts" => {
                    view.conflicts = true;
                    let files = data
                        .get("conflicts")
                        .and_then(Value::as_array)
                        .map_or(0, Vec::len);
                    format!(
                        "Pull stopped with conflicts in {files} file{}: resolve them, then \
                         Continue or Abort",
                        if files == 1 { "" } else { "s" }
                    )
                }
                _ => summary.to_string(),
            };
        }
        SyncAction::Push | SyncAction::ForcePushWithLease | SyncAction::Publish => {
            let branch = s(data, "branch");
            let target = format!("{}/{}", s(data, "remote"), s(data, "remote_branch"));
            view.message = if data.get("up_to_date").and_then(Value::as_bool) == Some(true) {
                format!("Already up to date: {target} has {branch}")
            } else if data.get("set_upstream").and_then(Value::as_bool) == Some(true) {
                format!("Published {branch} → {target}")
            } else if action == SyncAction::ForcePushWithLease {
                format!("Force-pushed {branch} → {target}")
            } else {
                format!("Pushed {branch} → {target}")
            };
        }
    }
    view
}

// ---------------------------------------------------------------------------------------------
// Branch names
// ---------------------------------------------------------------------------------------------

/// Checks a new branch name against git's ref-name rules (`git check-ref-format --branch`)
/// and the existing branches; returns the problem in words.
pub fn validate_branch_name(name: &str, existing: &[String]) -> Result<(), String> {
    if name.is_empty() {
        return Err("Enter a branch name".into());
    }
    if name.trim() != name || name.contains(' ') {
        return Err("Branch names cannot contain spaces".into());
    }
    if name.starts_with('-') {
        return Err("Branch names cannot start with '-'".into());
    }
    if name == "HEAD" || name == "@" {
        return Err(format!("'{name}' is reserved"));
    }
    if let Some(c) = name
        .chars()
        .find(|c| c.is_control() || matches!(c, '~' | '^' | ':' | '?' | '*' | '[' | '\\'))
    {
        let shown = if c.is_control() {
            "control characters".to_string()
        } else {
            format!("'{c}'")
        };
        return Err(format!("Branch names cannot contain {shown}"));
    }
    if name.contains("..") {
        return Err("Branch names cannot contain '..'".into());
    }
    if name.contains("@{") {
        return Err("Branch names cannot contain '@{'".into());
    }
    if name.starts_with('/') || name.ends_with('/') || name.contains("//") {
        return Err("Slashes must separate non-empty parts".into());
    }
    if name.ends_with('.') {
        return Err("Branch names cannot end with '.'".into());
    }
    for part in name.split('/') {
        if part.starts_with('.') {
            return Err("No part of a branch name can start with '.'".into());
        }
        if part.ends_with(".lock") {
            return Err("No part of a branch name can end with '.lock'".into());
        }
    }
    if existing.iter().any(|b| b == name) {
        return Err(format!("A branch named {name} already exists"));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Credential prompts
// ---------------------------------------------------------------------------------------------

/// The dialog a credential prompt opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PromptDialogKind {
    /// Username (plain input).
    Username,
    /// Password or token (masked).
    Password,
    /// SSH key passphrase (masked; shows the key path).
    Passphrase,
    /// Unknown host key: Trust and continue / Cancel (answer `yes`).
    HostKey,
    /// Anything else: the prompt verbatim, masked if it looks secret.
    Generic,
}

/// Everything a credential dialog shows, parsed from the prompt text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptSpec {
    /// Which dialog.
    pub kind: PromptDialogKind,
    /// Mask the input.
    pub masked: bool,
    /// Dialog title.
    pub title: String,
    /// The prompt as git / ssh sent it, neutralized for display (control characters and
    /// bidi overrides replaced or removed, length capped; see
    /// [`askpass::sanitize_prompt_text`]).
    pub prompt: String,
    /// Host (host-key prompts; `user@host` of ssh password prompts; also from URLs).
    pub host: Option<String>,
    /// Key type (`ED25519`).
    pub key_type: Option<String>,
    /// Fingerprint (`SHA256:...`).
    pub fingerprint: Option<String>,
    /// SSH key file (passphrase prompts).
    pub key_path: Option<String>,
    /// URL the prompt is for, credentials removed.
    pub url: Option<String>,
    /// Shown prominently above the input: the prompt names another host than the
    /// operation's remote, or Lector cannot tell which operation asked.
    pub warning: Option<String>,
}

/// Whether a prompt git / ssh did not classify looks like it asks for a secret.
pub fn looks_secret(text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    [
        "password",
        "passphrase",
        "token",
        "secret",
        "pin",
        "otp",
        "code",
        "key",
    ]
    .iter()
    .any(|w| t.contains(w))
}

/// Classifies a prompt into a dialog (masking, fields to show). Structured dialogs and
/// their fields come only from exact OpenSSH / git templates
/// ([`askpass::parse_prompt`]); `kind` may only downgrade that result (pass
/// [`PromptKind::Other`] to force the generic dialog), never upgrade it. Anything else is
/// the generic "The server asks:" dialog with the sanitized text and no trusted-looking
/// fields.
pub fn prompt_spec(text: &str, kind: PromptKind) -> PromptSpec {
    let parsed = askpass::parse_prompt(text);
    let kind = if kind == parsed.kind {
        kind
    } else {
        PromptKind::Other
    };
    let mut spec = PromptSpec {
        kind: PromptDialogKind::Generic,
        masked: false,
        title: "The server asks:".into(),
        prompt: askpass::sanitize_prompt_text(text),
        host: None,
        key_type: None,
        fingerprint: None,
        key_path: None,
        url: None,
        warning: None,
    };
    let redacted = |u: &Option<String>| u.as_deref().map(lector_git::sync::redact_credentials);
    match kind {
        PromptKind::HostKeyConfirm => {
            spec.kind = PromptDialogKind::HostKey;
            spec.title = "Unknown SSH host".into();
            spec.host = parsed.host;
            spec.key_type = parsed.key_type;
            spec.fingerprint = parsed.fingerprint;
        }
        PromptKind::Passphrase => {
            spec.kind = PromptDialogKind::Passphrase;
            spec.masked = true;
            spec.title = "SSH key passphrase".into();
            spec.key_path = parsed.key_path;
        }
        PromptKind::Username => {
            spec.kind = PromptDialogKind::Username;
            spec.title = "Username".into();
            spec.url = redacted(&parsed.url);
        }
        PromptKind::Password => {
            spec.kind = PromptDialogKind::Password;
            spec.masked = true;
            spec.title = "Password or token".into();
            spec.url = redacted(&parsed.url);
            // ssh: `user@host's password: `
            spec.host = parsed.host;
        }
        PromptKind::Other => {
            spec.masked = looks_secret(text);
        }
    }
    if spec.host.is_none() {
        spec.host = spec.url.as_deref().and_then(host_of_url);
    }
    spec
}

/// The context a prompt is shown in: which operation asked (if Lector can tell) and its
/// remote, plus a file-system check for passphrase prompts.
pub struct PromptContextInfo<'a> {
    /// The operation's remote URL (credentials redacted), if known.
    pub remote: Option<&'a str>,
    /// `false` when several operations run and the prompt did not say which one it is from.
    pub attributed: bool,
    /// Whether the key file a passphrase prompt names exists (`~/` expanded by the callee).
    pub key_exists: &'a dyn Fn(&str) -> bool,
}

/// [`prompt_spec`] for a prompt shown in `ctx`: a passphrase prompt naming a key file that
/// does not exist gets the generic dialog, and a warning is set when the host the prompt
/// names differs from the host of the operation's remote, or when the operation is unknown.
pub fn prompt_spec_in(text: &str, kind: PromptKind, ctx: &PromptContextInfo<'_>) -> PromptSpec {
    let mut spec = prompt_spec(text, kind);
    if spec.kind == PromptDialogKind::Passphrase
        && !spec
            .key_path
            .as_deref()
            .is_some_and(|k| (ctx.key_exists)(k))
    {
        spec = prompt_spec(text, PromptKind::Other);
    }
    let prompt_host = spec.host.as_deref().map(bare_host);
    let remote_host = ctx.remote.and_then(host_of_url);
    let mut warnings = Vec::new();
    if let (Some(asked), Some(remote)) = (prompt_host.as_deref(), remote_host.as_deref()) {
        if !asked.eq_ignore_ascii_case(remote) {
            warnings.push(format!(
                "This prompt is for {asked}, but the operation's remote is {remote}. Unless \
                 that is an ssh alias or a redirect you expect, cancel."
            ));
        }
    }
    if !ctx.attributed {
        warnings.push(
            "Several git operations are running and Lector cannot tell which one asks.".into(),
        );
    }
    if !warnings.is_empty() {
        spec.warning = Some(warnings.join(" "));
    }
    spec
}

/// `github.com` from `github.com`, `ann@github.com`, `github.com (1.2.3.4)`, `[h]:22`.
fn bare_host(host: &str) -> String {
    let h = host.split_whitespace().next().unwrap_or(host);
    let h = h.rsplit_once('@').map_or(h, |(_, r)| r);
    let h = h
        .strip_prefix('[')
        .map_or(h, |r| r.split(']').next().unwrap_or(r));
    h.to_string()
}

fn host_of_url(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let rest = rest.rsplit_once('@').map_or(rest, |(_, r)| r);
    let host = rest.split(['/', ':']).next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The literal answer "Trust and continue" sends for a host-key prompt (never the
/// fingerprint, never anything else).
pub const HOST_KEY_TRUST_ANSWER: &str = "yes";

#[cfg(test)]
mod tests;
