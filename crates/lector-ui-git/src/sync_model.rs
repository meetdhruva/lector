//! UI-free logic of "sync with the remote" (v0.7): the state of the current branch
//! relative to its upstream and what to offer for it, the commands each sync action runs,
//! the gate in front of dangerous ones (force with lease), the next step offered for each
//! structured failure, branch-name validation as you type, and the classification of
//! credential prompts into dialogs. Everything here is plain data so it is unit-tested
//! without a window; [`crate::sync`] renders it and runs the commands.

use std::time::{Duration, SystemTime};

use lector_git::{PullMode, SyncError, SyncErrorKind, UpstreamStatus};
use lector_process::askpass::PromptKind;
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
    /// The prompt exactly as git / ssh sent it.
    pub prompt: String,
    /// Host (host-key prompts; also from URLs).
    pub host: Option<String>,
    /// Key type (`ED25519`).
    pub key_type: Option<String>,
    /// Fingerprint (`SHA256:...`).
    pub fingerprint: Option<String>,
    /// SSH key file (passphrase prompts).
    pub key_path: Option<String>,
    /// URL the prompt is for, credentials removed.
    pub url: Option<String>,
}

/// The text between the first pair of single quotes.
fn quoted(text: &str) -> Option<String> {
    let start = text.find('\'')? + 1;
    let len = text[start..].find('\'')?;
    Some(text[start..start + len].to_string())
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

/// Classifies a prompt into a dialog (masking, fields to show).
pub fn prompt_spec(text: &str, kind: PromptKind) -> PromptSpec {
    let mut spec = PromptSpec {
        kind: PromptDialogKind::Generic,
        masked: false,
        title: "git is asking".into(),
        prompt: text.to_string(),
        host: None,
        key_type: None,
        fingerprint: None,
        key_path: None,
        url: None,
    };
    match kind {
        PromptKind::HostKeyConfirm => {
            spec.kind = PromptDialogKind::HostKey;
            spec.title = "Unknown SSH host".into();
            if let Some(i) = text.find("host '") {
                spec.host = quoted(&text[i..]);
            }
            for line in text.lines() {
                if let Some((before, after)) = line.split_once(" key fingerprint is ") {
                    spec.key_type = before.split_whitespace().last().map(str::to_string);
                    spec.fingerprint = Some(after.trim().trim_end_matches('.').to_string());
                }
            }
        }
        PromptKind::Passphrase => {
            spec.kind = PromptDialogKind::Passphrase;
            spec.masked = true;
            spec.title = "SSH key passphrase".into();
            spec.key_path = quoted(text);
        }
        PromptKind::Username => {
            spec.kind = PromptDialogKind::Username;
            spec.title = "Username".into();
            spec.url = quoted(text).map(|u| lector_git::sync::redact_credentials(&u));
        }
        PromptKind::Password => {
            spec.kind = PromptDialogKind::Password;
            spec.masked = true;
            spec.title = "Password or token".into();
            spec.url = quoted(text).map(|u| lector_git::sync::redact_credentials(&u));
            if spec.url.is_none() {
                // ssh: `user@host's password: `
                spec.host = text
                    .split("'s password")
                    .next()
                    .filter(|h| h.len() < text.len())
                    .map(|h| h.trim().to_string());
            }
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
mod tests {
    use super::*;

    fn status(ahead: u32, behind: u32) -> SyncStatus {
        SyncStatus {
            loaded: true,
            branch: Some("main".into()),
            head_short: Some("abc1234".into()),
            upstream: Some("origin/main".into()),
            push_remote: Some("origin".into()),
            ahead,
            behind,
            remotes: vec![RemoteInfo {
                name: "origin".into(),
                url: "https://example.com/r.git".into(),
            }],
            ..Default::default()
        }
    }

    #[test]
    fn every_state_maps_to_a_label_and_an_action() {
        let mut no_upstream = status(0, 0);
        no_upstream.upstream = None;
        let mut gone = no_upstream.clone();
        gone.upstream_gone = true;
        let mut detached = status(0, 0);
        detached.detached = true;
        detached.branch = None;
        let mut no_remote = no_upstream.clone();
        no_remote.remotes.clear();
        no_remote.push_remote = None;
        let cases: Vec<(SyncStatus, SyncState, &str, &str, PrimaryAction)> = vec![
            (
                SyncStatus::default(),
                SyncState::Loading,
                "",
                "",
                PrimaryAction::None,
            ),
            (
                status(0, 0),
                SyncState::UpToDate,
                "up to date",
                "",
                PrimaryAction::Sync(SyncAction::Fetch),
            ),
            (
                status(2, 0),
                SyncState::NeedsPush,
                "push",
                "↑2",
                PrimaryAction::Sync(SyncAction::Push),
            ),
            (
                status(0, 3),
                SyncState::NeedsPull,
                "pull",
                "↓3",
                PrimaryAction::Sync(SyncAction::Pull),
            ),
            (
                status(2, 1),
                SyncState::Diverged,
                "diverged",
                "↑2 ↓1",
                PrimaryAction::Sync(SyncAction::PullRebase),
            ),
            (
                no_upstream,
                SyncState::NoUpstream,
                "publish",
                "",
                PrimaryAction::Sync(SyncAction::Publish),
            ),
            (
                gone,
                SyncState::UpstreamGone,
                "upstream gone",
                "",
                PrimaryAction::Sync(SyncAction::Publish),
            ),
            (
                detached,
                SyncState::Detached,
                "detached",
                "",
                PrimaryAction::NewBranch,
            ),
            (
                no_remote,
                SyncState::NoRemote,
                "no remote",
                "",
                PrimaryAction::None,
            ),
        ];
        for (st, state, word, counts, primary) in cases {
            assert_eq!(st.state(), state);
            let v = control_view(&st);
            assert_eq!(v.state_word, word, "{state:?}");
            assert_eq!(v.counts, counts, "{state:?}");
            assert_eq!(v.primary, primary, "{state:?}");
            assert!(!v.tooltip.is_empty(), "{state:?}");
        }
        assert_eq!(control_view(&status(0, 0)).branch, "main");
        let mut d = status(0, 0);
        d.detached = true;
        assert_eq!(control_view(&d).branch, "detached abc1234");
        assert_eq!(
            PrimaryAction::Sync(SyncAction::Push).action_name(),
            Some("git::Push")
        );
        assert_eq!(
            PrimaryAction::NewBranch.action_name(),
            Some("git::NewBranch")
        );
        assert_eq!(PrimaryAction::None.action_name(), None);
    }

    #[test]
    fn actions_map_to_commands_and_never_plain_force() {
        for a in SyncAction::ALL {
            let (id, args) = a.command();
            assert!(id.starts_with("git."));
            assert!(args.get("force").is_none(), "no plain --force");
        }
        assert_eq!(
            SyncAction::Pull.command(),
            ("git.pull", json!({"mode": "ff_only"}))
        );
        assert_eq!(
            SyncAction::PullRebase.command(),
            ("git.pull", json!({"mode": "rebase"}))
        );
        assert_eq!(
            SyncAction::ForcePushWithLease.command(),
            ("git.push", json!({"force_with_lease": true}))
        );
        assert_eq!(
            SyncAction::Publish.command(),
            ("git.push", json!({"set_upstream": true}))
        );
    }

    #[test]
    fn force_with_lease_is_always_confirmed_first() {
        for st in [status(0, 0), status(3, 0), status(1, 4)] {
            match plan(SyncAction::ForcePushWithLease, &st, None) {
                Plan::Confirm(confirm, cmds) => {
                    assert_eq!(confirm.title, "Force push to origin/main?");
                    assert!(confirm.message[0].contains("origin/main"));
                    assert!(confirm.message[0].contains("may be discarded"));
                    assert!(confirm.message[1].contains("--force-with-lease"));
                    assert_eq!(cmds, vec![SyncAction::ForcePushWithLease.command()]);
                }
                other => panic!("force push must be confirmed, got {other:?}"),
            }
        }
        // Other actions run directly; a busy repository blocks everything.
        assert!(matches!(
            plan(SyncAction::Push, &status(1, 0), None),
            Plan::Run(_)
        ));
        assert!(matches!(
            plan(
                SyncAction::ForcePushWithLease,
                &status(1, 0),
                Some("Fetching")
            ),
            Plan::Blocked(_)
        ));
        let mut detached = status(0, 0);
        detached.detached = true;
        assert!(matches!(
            plan(SyncAction::Push, &detached, None),
            Plan::Blocked(_)
        ));
        assert!(matches!(
            plan(SyncAction::Fetch, &detached, None),
            Plan::Run(_)
        ));
        let mut no_up = status(0, 0);
        no_up.upstream = None;
        assert!(matches!(
            plan(SyncAction::Pull, &no_up, None),
            Plan::Blocked(_)
        ));
    }

    fn err(kind: SyncErrorKind) -> SyncError {
        let mut e = SyncError::new(kind, "git said no");
        if kind == SyncErrorKind::HookDeclined {
            e.hook_output = Some("pre-push: tests failed".into());
        }
        if kind == SyncErrorKind::LocalChanges {
            e.paths = vec!["a.txt".into()];
        }
        e
    }

    #[test]
    fn every_failure_kind_maps_to_a_message_and_next_steps() {
        use NextStep as S;
        use SyncErrorKind as K;
        let push = OpKind::Sync(SyncAction::Push);
        let pull = OpKind::Sync(SyncAction::Pull);
        let rebase = OpKind::Sync(SyncAction::PullRebase);
        let switch = OpKind::Switch("feature".into());
        let force = OpKind::Sync(SyncAction::ForcePushWithLease);
        let delete = OpKind::DeleteBranch("x".into());
        let create = OpKind::CreateBranch("x".into());
        let ctx = FailureContext {
            remote_url: Some("https://example.com/r.git".into()),
            credential_helpers: vec!["store".into()],
            prompt_answered: Some(false),
            ssh_agent: false,
        };
        let cases: Vec<(K, &OpKind, Vec<S>)> = vec![
            (K::NoUpstream, &pull, vec![S::Publish, S::SetUpstream]),
            (K::NoRemote, &push, vec![]),
            (K::DetachedHead, &push, vec![S::NewBranch]),
            (K::Rejected, &push, vec![S::PullFirst, S::PullRebase]),
            (K::StaleLease, &force, vec![S::Fetch]),
            (K::AuthenticationFailed, &push, vec![S::Retry]),
            (K::HostKeyUnknown, &push, vec![S::Retry]),
            (K::HostKeyChanged, &push, vec![]),
            (K::NetworkUnreachable, &pull, vec![S::Retry]),
            (K::RemoteNotFound, &push, vec![S::Retry]),
            (K::HookDeclined, &push, vec![S::Retry]),
            (K::Diverged, &pull, vec![S::PullRebase]),
            (
                K::LocalChanges,
                &pull,
                vec![S::StashAndPull(SyncAction::Pull)],
            ),
            (
                K::LocalChanges,
                &switch,
                vec![S::StashAndSwitch("feature".into())],
            ),
            (
                K::DirtyWorktree,
                &rebase,
                vec![S::StashAndPull(SyncAction::PullRebase)],
            ),
            (K::NotMerged, &delete, vec![]),
            (K::InvalidName, &create, vec![]),
            (K::AlreadyExists, &create, vec![]),
            (K::NotFound, &switch, vec![]),
            (K::RefChanged, &pull, vec![]),
            (K::Other, &push, vec![S::Retry]),
        ];
        for (kind, op, steps) in cases {
            let v = failure_view(op, &err(kind), &ctx);
            assert_eq!(v.kind, Some(kind));
            assert_eq!(v.steps, steps, "{kind:?} after {op:?}");
            assert!(v.title.ends_with("failed"), "{kind:?}");
            assert!(!v.message.is_empty(), "{kind:?}");
            assert!(v.details.is_some(), "{kind:?} shows git's output");
        }
        let hook = failure_view(&push, &err(K::HookDeclined), &ctx);
        assert!(hook.details_are_hook_output);
        assert_eq!(hook.details.as_deref(), Some("pre-push: tests failed"));
        let auth = failure_view(&push, &err(K::AuthenticationFailed), &ctx);
        assert!(auth.message[1].contains("credential helper first (store)"));
        assert!(auth.message[1].contains("cancelled"));
        let local = failure_view(&switch, &err(K::LocalChanges), &ctx);
        assert_eq!(local.paths, vec!["a.txt".to_string()]);
        let changed = failure_view(&push, &err(K::HostKeyChanged), &ctx);
        assert!(changed.message[0].starts_with("WARNING"));
        assert_eq!(generic_failure(&push, "boom").steps, vec![S::Retry]);
    }

    #[test]
    fn credential_sources_name_ssh_and_https_paths() {
        let ssh = credential_sources(&FailureContext {
            remote_url: Some("git@github.com:me/r.git".into()),
            ssh_agent: true,
            ..Default::default()
        });
        assert!(ssh.contains("SSH") && ssh.contains("ssh-agent"));
        let https = credential_sources(&FailureContext {
            remote_url: Some("https://***@example.com/r.git".into()),
            prompt_answered: Some(true),
            ..Default::default()
        });
        assert!(https.contains("No git credential helper"));
        assert!(https.contains("refused that answer"));
    }

    #[test]
    fn outcomes_become_toasts() {
        let pull = json!({"status": "fast_forwarded", "branch": "main", "upstream": "origin/main", "incoming": 3, "rebased": 0, "conflicts": []});
        let v = success_view(SyncAction::Pull, &pull, "x");
        assert_eq!(v.message, "Pulled 3 commits into main");
        assert_eq!(v.pulled, 3);
        let v = success_view(
            SyncAction::Pull,
            &json!({"status": "up_to_date", "incoming": 0}),
            "x",
        );
        assert_eq!(v.message, "Already up to date");
        let v = success_view(
            SyncAction::PullRebase,
            &json!({"status": "conflicts", "conflicts": ["a", "b"]}),
            "x",
        );
        assert!(v.conflicts);
        assert!(v.message.contains("2 files"));
        let push = json!({"branch": "main", "remote": "origin", "remote_branch": "main", "up_to_date": false, "set_upstream": false});
        assert_eq!(
            success_view(SyncAction::Push, &push, "x").message,
            "Pushed main → origin/main"
        );
        let publish = json!({"branch": "f", "remote": "origin", "remote_branch": "f", "up_to_date": false, "set_upstream": true});
        assert_eq!(
            success_view(SyncAction::Publish, &publish, "x").message,
            "Published f → origin/f"
        );
        assert_eq!(
            success_view(SyncAction::Fetch, &json!({"updated": []}), "x").message,
            "Fetched: already up to date"
        );
    }

    #[test]
    fn branch_names_follow_git_ref_rules() {
        let existing = vec!["main".to_string()];
        let ok = [
            "feature",
            "feature/login",
            "fix-123",
            "release/v1.2",
            "ünïcode",
            "a.b",
        ];
        for name in ok {
            assert_eq!(validate_branch_name(name, &existing), Ok(()), "{name}");
        }
        let bad = [
            ("", "Enter"),
            ("main", "already exists"),
            ("has space", "spaces"),
            (" lead", "spaces"),
            ("-x", "start with '-'"),
            ("HEAD", "reserved"),
            ("@", "reserved"),
            ("a..b", "'..'"),
            ("a~1", "'~'"),
            ("a^", "'^'"),
            ("a:b", "':'"),
            ("a?", "'?'"),
            ("a*", "'*'"),
            ("a[b", "'['"),
            ("a\\b", "'\\'"),
            ("a@{b", "'@{'"),
            ("/a", "Slashes"),
            ("a/", "Slashes"),
            ("a//b", "Slashes"),
            ("a.", "end with '.'"),
            (".a", "start with '.'"),
            ("a/.b", "start with '.'"),
            ("a.lock", ".lock"),
            ("a/b.lock/c", ".lock"),
            ("tab\there", "control"),
        ];
        for (name, why) in bad {
            match validate_branch_name(name, &existing) {
                Err(e) => assert!(e.contains(why), "{name:?}: {e}"),
                Ok(()) => panic!("{name:?} must be rejected"),
            }
        }
    }

    #[test]
    fn prompts_are_classified_into_dialogs() {
        let host = "The authenticity of host 'github.com (140.82.121.4)' can't be established.\nED25519 key fingerprint is SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU.\nThis key is not known by any other names.\nAre you sure you want to continue connecting (yes/no/[fingerprint])? ";
        let spec = prompt_spec(host, lector_process::askpass::classify_prompt(host));
        assert_eq!(spec.kind, PromptDialogKind::HostKey);
        assert!(!spec.masked);
        assert_eq!(spec.host.as_deref(), Some("github.com (140.82.121.4)"));
        assert_eq!(spec.key_type.as_deref(), Some("ED25519"));
        assert_eq!(
            spec.fingerprint.as_deref(),
            Some("SHA256:+DiY3wvvV6TuJJhbpZisF/zLDA0zPMSvHdkr4UvCOqU")
        );
        assert_eq!(HOST_KEY_TRUST_ANSWER, "yes");

        let pass = "Enter passphrase for key '/home/ann/.ssh/id_ed25519': ";
        let spec = prompt_spec(pass, lector_process::askpass::classify_prompt(pass));
        assert_eq!(spec.kind, PromptDialogKind::Passphrase);
        assert!(spec.masked);
        assert_eq!(spec.key_path.as_deref(), Some("/home/ann/.ssh/id_ed25519"));

        let user = "Username for 'https://github.com': ";
        let spec = prompt_spec(user, lector_process::askpass::classify_prompt(user));
        assert_eq!(spec.kind, PromptDialogKind::Username);
        assert!(!spec.masked);
        assert_eq!(spec.url.as_deref(), Some("https://github.com"));
        assert_eq!(spec.host.as_deref(), Some("github.com"));

        let pw = "Password for 'https://ann:tok@github.com': ";
        let spec = prompt_spec(pw, lector_process::askpass::classify_prompt(pw));
        assert_eq!(spec.kind, PromptDialogKind::Password);
        assert!(spec.masked);
        let url = spec.url.unwrap_or_default();
        assert!(!url.contains("tok"), "credentials stripped: {url}");
        assert!(url.contains("github.com"));

        let ssh_pw = "ann@build.example's password: ";
        let spec = prompt_spec(ssh_pw, lector_process::askpass::classify_prompt(ssh_pw));
        assert_eq!(spec.kind, PromptDialogKind::Password);
        assert_eq!(spec.host.as_deref(), Some("ann@build.example"));

        let otp = "Verification code: ";
        let spec = prompt_spec(otp, PromptKind::Other);
        assert_eq!(spec.kind, PromptDialogKind::Generic);
        assert!(spec.masked, "looks secret");
        assert_eq!(spec.prompt, otp, "shown verbatim");
        let plain = "Which remote? ";
        assert!(!prompt_spec(plain, PromptKind::Other).masked);
    }

    #[test]
    fn last_fetched_is_human() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
        let ago = |s: u64| Some(now - Duration::from_secs(s));
        assert_eq!(last_fetched_label(None, now), "Never fetched");
        assert_eq!(last_fetched_label(ago(5), now), "Last fetched just now");
        assert_eq!(last_fetched_label(ago(720), now), "Last fetched 12 min ago");
        assert_eq!(
            last_fetched_label(ago(3600), now),
            "Last fetched 1 hour ago"
        );
        assert_eq!(
            last_fetched_label(ago(3 * 86_400), now),
            "Last fetched 3 days ago"
        );
    }

    #[test]
    fn push_target_and_remote_url() {
        let st = status(1, 0);
        assert_eq!(st.push_target().as_deref(), Some("origin/main"));
        assert_eq!(st.remote_url(), Some("https://example.com/r.git"));
        let mut other = st.clone();
        other.upstream = None;
        other.branch = Some("feat".into());
        assert_eq!(other.push_target().as_deref(), Some("origin/feat"));
    }
}
