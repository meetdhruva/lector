//! Network operations (v0.7): fetch, pull, push, upstream state — through the `git` CLI
//! (hooks, credential helpers, signing and ssh behave as in a terminal), run under
//! `lector-process` (cancellation kills git and everything it started).
//!
//! * Progress: git's `--progress` stderr (`\r`-updated lines) is parsed into [`Progress`]
//!   events ([`parse_progress`]); other stderr lines arrive as [`SyncEvent::Message`].
//! * Failures the UI can act on are [`Error::Sync`] with a [`SyncErrorKind`]
//!   (authentication, unknown host key, network, rejected push, hook output, ...).
//! * Credentials: [`SyncOptions::env`] carries the askpass environment of the app; tokens
//!   never go into argv, and messages are passed through [`redact_credentials`].
//! * Pull never creates a merge commit: fast-forward only (`--no-rebase --ff-only`) or
//!   `--rebase`. Push never force-pushes except `--force-with-lease` with an explicit
//!   expected value, on request, and only when that fetched value is integrated locally
//!   (the `--force-if-includes` rule, checked by Lector so it works with any git version).

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::Path;

use lector_process::{Line, LineEnd, Stream};

use crate::error::{Error, Result};
use crate::proc::{self, SyncOptions};
use crate::{Oid, OperationKind, Repo};

/// What went wrong in a sync / branch operation, for the UI to react to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum SyncErrorKind {
    /// The branch has no upstream (pull) — push with "set upstream", or set one.
    NoUpstream,
    /// No remote is configured (or the named one does not exist).
    NoRemote,
    /// `HEAD` is detached; the operation needs a branch.
    DetachedHead,
    /// Push rejected because the remote has commits we don't ("pull first").
    Rejected,
    /// `--force-with-lease` refused: the remote branch moved since we last fetched.
    StaleLease,
    /// Credentials were refused or could not be obtained (prompt cancelled).
    AuthenticationFailed,
    /// ssh does not know the host key (and it could not be confirmed).
    HostKeyUnknown,
    /// ssh's known host key differs from the server's (possible attack).
    HostKeyChanged,
    /// The host could not be reached (DNS, refused, timeout, TLS).
    NetworkUnreachable,
    /// The remote repository does not exist or is not accessible.
    RemoteNotFound,
    /// A local (`pre-push`) or remote (`pre-receive`, `update`) hook declined; see `hook_output`.
    HookDeclined,
    /// Fast-forward-only pull impossible: local and upstream diverged.
    Diverged,
    /// Local changes would be overwritten (switch, stash apply, pull); see `paths`.
    LocalChanges,
    /// The working tree must be clean for this (pull with rebase).
    DirtyWorktree,
    /// Branch delete refused: not fully merged (delete with force explicitly).
    NotMerged,
    /// The branch name is not valid.
    InvalidName,
    /// A branch with that name exists already.
    AlreadyExists,
    /// The named branch, ref or stash does not exist.
    NotFound,
    /// The ref changed since it was recorded; undo refused.
    RefChanged,
    /// Anything else; see the message.
    Other,
}

impl SyncErrorKind {
    /// Short human description.
    pub fn describe(self) -> &'static str {
        match self {
            SyncErrorKind::NoUpstream => "the branch has no upstream",
            SyncErrorKind::NoRemote => "no such remote",
            SyncErrorKind::DetachedHead => "HEAD is detached",
            SyncErrorKind::Rejected => "the remote has changes you don't have",
            SyncErrorKind::StaleLease => "the remote branch changed since the last fetch",
            SyncErrorKind::AuthenticationFailed => "authentication failed",
            SyncErrorKind::HostKeyUnknown => "the host key is not known",
            SyncErrorKind::HostKeyChanged => "the host key changed",
            SyncErrorKind::NetworkUnreachable => "the remote cannot be reached",
            SyncErrorKind::RemoteNotFound => "the remote repository was not found",
            SyncErrorKind::HookDeclined => "a hook declined",
            SyncErrorKind::Diverged => "the branch and its upstream have diverged",
            SyncErrorKind::LocalChanges => "local changes would be overwritten",
            SyncErrorKind::DirtyWorktree => "the working tree has uncommitted changes",
            SyncErrorKind::NotMerged => "the branch is not fully merged",
            SyncErrorKind::InvalidName => "invalid name",
            SyncErrorKind::AlreadyExists => "it already exists",
            SyncErrorKind::NotFound => "not found",
            SyncErrorKind::RefChanged => "the branch moved since",
            SyncErrorKind::Other => "git failed",
        }
    }
}

/// A classified failure ([`Error::Sync`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SyncError {
    /// What kind of failure.
    pub kind: SyncErrorKind,
    /// git's output (credentials in URLs redacted).
    pub message: String,
    /// What the user can do ("Pull first, then push again.").
    pub hint: Option<String>,
    /// Paths involved (local changes that would be overwritten).
    pub paths: Vec<String>,
    /// Output of the hook that declined.
    pub hook_output: Option<String>,
    /// Alternatives the UI can offer, as data (e.g. `"stash_and_switch"`); never automatic.
    pub alternatives: Vec<String>,
}

impl SyncError {
    /// A failure of `kind` with `message`.
    pub fn new(kind: SyncErrorKind, message: impl Into<String>) -> Self {
        SyncError {
            kind,
            message: message.into(),
            hint: None,
            paths: Vec::new(),
            hook_output: None,
            alternatives: Vec::new(),
        }
    }

    /// Sets the hint.
    pub fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.kind.describe())?;
        let msg = self.message.trim();
        if !msg.is_empty() {
            write!(f, ": {msg}")?;
        }
        if let Some(h) = &self.hint {
            write!(f, " ({h})")?;
        }
        Ok(())
    }
}

impl std::error::Error for SyncError {}

impl From<SyncError> for Error {
    fn from(e: SyncError) -> Self {
        Error::Sync(Box::new(e))
    }
}

/// One progress update parsed from git's `--progress` output.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Progress {
    /// The phase as git names it ("Receiving objects", "Resolving deltas", ...).
    pub phase: String,
    /// Reported by the remote side (`remote: ` prefix).
    pub remote: bool,
    /// Percentage, when git knows the total.
    pub percent: Option<u8>,
    /// Items done (objects, deltas, files).
    pub current: Option<u64>,
    /// Items in total.
    pub total: Option<u64>,
    /// Bytes transferred so far (approximate: git prints `1.20 MiB`).
    pub bytes: Option<u64>,
    /// Throughput in bytes per second (approximate).
    pub throughput: Option<u64>,
    /// The phase finished (`, done.`).
    pub done: bool,
}

/// An event of a running sync operation.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SyncEvent {
    /// A progress update.
    Progress(Progress),
    /// Any other line git printed on stderr (credentials redacted).
    Message(String),
}

/// Parses one line of git's progress output ("Receiving objects:  33% (1/3), 1.20 MiB |
/// 1.19 MiB/s"); `None` for lines that are not progress.
pub fn parse_progress(line: &str) -> Option<Progress> {
    let line = line.trim_end();
    let (remote, rest) = match line.strip_prefix("remote:") {
        Some(r) => (true, r.trim_start()),
        None => (false, line.trim_start()),
    };
    let (phase, rest) = rest.split_once(':')?;
    let phase = phase.trim();
    if phase.is_empty()
        || phase.len() > 40
        || !phase
            .chars()
            .all(|c| c.is_ascii_alphabetic() || c == ' ' || c == '-')
    {
        return None;
    }
    let mut p = Progress {
        phase: phase.to_owned(),
        remote,
        percent: None,
        current: None,
        total: None,
        bytes: None,
        throughput: None,
        done: false,
    };
    let rest = rest.trim();
    let mut parts = rest.split(", ");
    let first = parts.next()?.trim();
    if let Some((pct, counts)) = first.split_once('%') {
        let pct: u8 = pct.trim().parse().ok()?;
        p.percent = Some(pct.min(100));
        let counts = counts.trim();
        if let Some(inner) = counts.strip_prefix('(').and_then(|c| c.strip_suffix(')')) {
            let (cur, tot) = inner.split_once('/')?;
            p.current = cur.trim().parse().ok();
            p.total = tot.trim().parse().ok();
        } else if !counts.is_empty() {
            return None;
        }
    } else {
        p.current = Some(first.parse().ok()?);
    }
    for part in parts {
        let part = part.trim();
        if part.starts_with("done") || part.starts_with("completed") {
            p.done = true;
        } else if let Some((bytes, rate)) = part.split_once('|') {
            p.bytes = parse_size(bytes.trim());
            p.throughput = rate.trim().strip_suffix("/s").and_then(parse_size);
        } else {
            p.bytes = p.bytes.or_else(|| parse_size(part));
        }
    }
    Some(p)
}

/// `1.20 MiB` → bytes.
fn parse_size(text: &str) -> Option<u64> {
    let (num, unit) = text.trim().split_once(' ')?;
    let n: f64 = num.parse().ok()?;
    let mult = match unit.trim() {
        "bytes" | "byte" | "B" => 1.0,
        "KiB" => 1024.0,
        "MiB" => 1024.0 * 1024.0,
        "GiB" => 1024.0 * 1024.0 * 1024.0,
        "TiB" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    let v = n * mult;
    (v.is_finite() && v >= 0.0).then_some(v as u64)
}

/// Replaces the user-info part of URLs (`https://user:token@host` → `https://***@host`), so
/// credentials embedded in remote URLs never reach the UI or logs.
///
/// Works line by line. For every `scheme://` the user info is everything up to the **last**
/// `@` of the following whitespace-free run (passwords may contain `/`, quotes, `:`), or,
/// when that run looks like `user:pass` without a `/`, up to an `@` later on the same line
/// (a password with a space). Over-redacting (a URL with `@` in its path) is preferred to
/// leaking. scp-like addresses keep a plain user name (`git@github.com:o/r`: no secret) but
/// lose `user:secret@`.
pub fn redact_credentials(text: &str) -> String {
    text.split_inclusive('\n')
        .map(|line| redact_scp_like(&redact_scheme_urls(line)))
        .collect()
}

/// `scheme://userinfo@host` → `scheme://***@host` within one line.
fn redact_scheme_urls(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    while let Some(pos) = rest.find("://") {
        let (head, tail) = rest.split_at(pos + 3);
        out.push_str(head);
        // The next URL starts a new authority.
        let limit = tail.find("://").unwrap_or(tail.len());
        let run_end = tail
            .find(char::is_whitespace)
            .unwrap_or(tail.len())
            .min(limit);
        let run = &tail[..run_end];
        let at = run.rfind('@').or_else(|| {
            // `user:pa ss@host`: a run with a colon, no slash and no `@`, then an `@` on the
            // same line before the next `/`.
            if run.contains('/') || !run.contains(':') {
                return None;
            }
            let after = &tail[run_end..limit];
            let stop = after.find(['/', '\n']).unwrap_or(after.len());
            after[..stop].rfind('@').map(|i| run_end + i)
        });
        match at {
            Some(at) => {
                out.push_str("***");
                rest = &tail[at..];
            }
            None => {
                out.push_str(run);
                rest = &tail[run_end..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// scp-like `user:secret@host:path` → `***@host:path`; a plain `user@host` is kept.
fn redact_scp_like(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut token_start: Option<usize> = None;
    let flush = |out: &mut String, token: &str| {
        let lead = token.len() - token.trim_start_matches(['\'', '"', '(', '<', '[']).len();
        let (prefix, body) = token.split_at(lead);
        out.push_str(prefix);
        match body.rfind('@') {
            Some(at) if !body.contains("://") && body[..at].contains(':') => {
                out.push_str("***");
                out.push_str(&body[at..]);
            }
            _ => out.push_str(body),
        }
    };
    for (i, c) in line.char_indices() {
        if c.is_whitespace() {
            if let Some(start) = token_start.take() {
                flush(&mut out, &line[start..i]);
            }
            out.push(c);
        } else if token_start.is_none() {
            token_start = Some(i);
        }
    }
    if let Some(start) = token_start {
        flush(&mut out, &line[start..]);
    }
    out
}

/// Classifies git's error output. `None` when nothing specific matched.
pub fn classify_failure(stderr: &str) -> Option<SyncErrorKind> {
    let t = stderr.to_ascii_lowercase();
    let any = |needles: &[&str]| needles.iter().any(|n| t.contains(n));
    let kind = if any(&["remote host identification has changed"]) {
        SyncErrorKind::HostKeyChanged
    } else if any(&[
        "host key verification failed",
        "no matching host key",
        "host key is not known",
    ]) {
        SyncErrorKind::HostKeyUnknown
    } else if any(&[
        "authentication failed",
        "could not read username",
        "could not read password",
        "permission denied (publickey",
        "permission denied, please try again",
        "terminal prompts disabled",
        "http basic: access denied",
        "invalid username or password",
        "returned error: 401",
        "returned error: 403",
        "logon failed",
        "incorrect passphrase",
    ]) {
        SyncErrorKind::AuthenticationFailed
    } else if any(&[
        "repository not found",
        "does not appear to be a git repository",
        "returned error: 404",
    ]) {
        SyncErrorKind::RemoteNotFound
    } else if any(&[
        "could not resolve host",
        "could not resolve hostname",
        "connection refused",
        "connection timed out",
        "network is unreachable",
        "no route to host",
        "failed to connect",
        "operation timed out",
        "connection reset",
        "temporary failure in name resolution",
        "name or service not known",
        "ssl certificate problem",
        "unable to access",
        "could not read from remote repository",
    ]) {
        SyncErrorKind::NetworkUnreachable
    } else if any(&["hook declined"]) {
        SyncErrorKind::HookDeclined
    } else if any(&[
        "no tracking information",
        "has no upstream branch",
        "no upstream configured",
    ]) {
        SyncErrorKind::NoUpstream
    } else if any(&["not possible to fast-forward", "diverging branches"]) {
        SyncErrorKind::Diverged
    } else if any(&["would be overwritten by", "already exists, no checkout"]) {
        SyncErrorKind::LocalChanges
    } else if any(&[
        "you have unstaged changes",
        "your index contains uncommitted changes",
        "cannot pull with rebase",
        "please commit or stash them",
    ]) {
        SyncErrorKind::DirtyWorktree
    } else if any(&["not fully merged"]) {
        SyncErrorKind::NotMerged
    } else if any(&["not a valid branch name", "is not a valid ref name"]) {
        SyncErrorKind::InvalidName
    } else if any(&["already exists"]) {
        SyncErrorKind::AlreadyExists
    } else if any(&[
        "not found",
        "no such branch",
        "did not match any",
        "invalid reference",
        "is not a stash",
        "no stash entries",
    ]) {
        SyncErrorKind::NotFound
    } else {
        return None;
    };
    Some(kind)
}

/// Paths git lists (tab-indented) after "would be overwritten by ..." headers.
pub(crate) fn overwritten_paths(stderr: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut listing = false;
    for line in stderr.lines() {
        if line.contains("would be overwritten by") {
            listing = true;
            continue;
        }
        if listing {
            match line.strip_prefix('\t') {
                Some(p) if !p.trim().is_empty() => out.push(p.trim().to_owned()),
                _ => listing = false,
            }
        } else if let Some(p) = line.strip_suffix(" already exists, no checkout") {
            out.push(p.trim().to_owned());
        }
    }
    out.dedup();
    out
}

/// Builds the error of a failed command from its output.
pub(crate) fn failure(out: &lector_process::RunOutput, fallback: SyncErrorKind) -> SyncError {
    let stderr = redact_credentials(out.stderr_text().trim());
    let stdout = redact_credentials(out.stdout_text().trim());
    let text = if stderr.is_empty() {
        stdout.clone()
    } else if stdout.is_empty() {
        stderr.clone()
    } else {
        format!("{stderr}\n{stdout}")
    };
    let kind = classify_failure(&text).unwrap_or(fallback);
    let mut err = SyncError::new(kind, message_without_hints(&text));
    match kind {
        SyncErrorKind::LocalChanges => {
            err.paths = overwritten_paths(&text);
            err.hint = Some("Commit or stash your changes first.".into());
        }
        SyncErrorKind::DirtyWorktree => {
            err.hint = Some("Commit or stash your changes first.".into());
        }
        SyncErrorKind::Diverged => {
            err.hint = Some("Pull with rebase, or merge in a terminal.".into());
        }
        SyncErrorKind::NoUpstream => {
            err.hint = Some("Set an upstream, or push to create one.".into());
        }
        SyncErrorKind::AuthenticationFailed => {
            err.hint = Some("Check your credentials or credential helper.".into());
        }
        SyncErrorKind::HostKeyUnknown => {
            err.hint = Some("Verify the host and add its key to known_hosts.".into());
        }
        SyncErrorKind::NotMerged => {
            err.hint = Some("Delete with force to discard its unmerged commits.".into());
        }
        SyncErrorKind::HookDeclined => {
            let hook: Vec<&str> = text
                .lines()
                .filter_map(|l| l.strip_prefix("remote:").map(str::trim))
                .collect();
            if !hook.is_empty() {
                err.hook_output = Some(hook.join("\n"));
            }
        }
        _ => {}
    }
    err
}

/// git's message without the `hint:` advice lines (Lector gives its own hint).
fn message_without_hints(text: &str) -> String {
    text.lines()
        .filter(|l| !l.starts_with("hint:"))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned()
}

/// Forwards git's stderr lines to `on` as progress / message events.
fn event_sink<'a>(on: &'a mut dyn FnMut(SyncEvent)) -> impl FnMut(&Line) + 'a {
    move |line: &Line| {
        if line.stream != Stream::Stderr {
            return;
        }
        let text = line.text();
        if let Some(p) = parse_progress(&text) {
            on(SyncEvent::Progress(p));
        } else if line.end != LineEnd::CarriageReturn && !text.trim().is_empty() {
            on(SyncEvent::Message(redact_credentials(text.trim_end())));
        }
    }
}

/// Options of [`Repo::fetch`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FetchOptions {
    /// One remote, or `None` for all (`--all`).
    pub remote: Option<String>,
    /// Remove remote-tracking refs that no longer exist on the remote (`--prune`).
    pub prune: bool,
}

/// A ref that changed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RefUpdate {
    /// Full ref name (`refs/remotes/origin/main`).
    pub name: String,
    /// Before (`None` = created).
    pub old: Option<Oid>,
    /// After (`None` = deleted / pruned).
    pub new: Option<Oid>,
}

/// Result of [`Repo::fetch`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct FetchOutcome {
    /// Remote-tracking refs and tags that changed.
    pub updated: Vec<RefUpdate>,
}

/// How [`Repo::pull`] integrates upstream changes. There is no merge mode: Lector never
/// creates a merge commit implicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PullMode {
    /// `git pull --no-rebase --ff-only`: fails with [`SyncErrorKind::Diverged`] otherwise.
    #[default]
    FastForwardOnly,
    /// `git pull --rebase`: replays local commits on the upstream.
    Rebase,
}

/// What [`Repo::pull`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PullStatus {
    /// Nothing new.
    UpToDate,
    /// The branch moved forward to the upstream.
    FastForwarded,
    /// Local commits were replayed on the upstream.
    Rebased,
    /// The rebase stopped with conflicts (see `conflicts`); continue or abort it with the
    /// existing conflict flow.
    Conflicts,
}

/// Result of [`Repo::pull`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PullOutcome {
    /// What happened.
    pub status: PullStatus,
    /// The branch.
    pub branch: String,
    /// Its upstream (`origin/main`).
    pub upstream: String,
    /// `HEAD` before (`None` on an unborn branch).
    pub before: Option<Oid>,
    /// `HEAD` after.
    pub after: Option<Oid>,
    /// Upstream commits that were not in `before`.
    pub incoming: u32,
    /// Local commits replayed by a rebase.
    pub rebased: u32,
    /// Conflicted paths when `status == Conflicts`.
    pub conflicts: Vec<String>,
}

/// Options of [`Repo::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PushOptions {
    /// Remote; default: `branch.<b>.pushRemote`, `remote.pushDefault`, the upstream's
    /// remote, then the only remote / `origin`.
    pub remote: Option<String>,
    /// Local branch; default: the current one.
    pub branch: Option<String>,
    /// Set the upstream (`-u`) when the branch has none. Default `true`.
    pub set_upstream: bool,
    /// `--force-with-lease=<ref>:<last fetched value>`. Plain force is never used.
    pub force_with_lease: bool,
}

impl Default for PushOptions {
    fn default() -> Self {
        PushOptions {
            remote: None,
            branch: None,
            set_upstream: true,
            force_with_lease: false,
        }
    }
}

/// Status of one ref in `git push --porcelain`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PushRefStatus {
    /// ` `: fast-forward.
    FastForward,
    /// `+`: forced update (only with force-with-lease).
    Forced,
    /// `-`: deleted.
    Deleted,
    /// `*`: new ref.
    Created,
    /// `!`: rejected (see `reason`).
    Rejected,
    /// `=`: already up to date.
    UpToDate,
}

/// One ref line of `git push --porcelain`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PushedRef {
    /// Status flag.
    pub status: PushRefStatus,
    /// Local ref (`refs/heads/main`).
    pub local: String,
    /// Remote ref.
    pub remote: String,
    /// git's summary (`abc..def`, `[new branch]`, `[rejected]`).
    pub summary: String,
    /// Reason in parentheses (`non-fast-forward`, `fetch first`, `stale info`, `pre-receive hook declined`).
    pub reason: Option<String>,
}

/// Parses the ref lines of `git push --porcelain` stdout.
pub fn parse_push_porcelain(stdout: &str) -> Vec<PushedRef> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.splitn(3, '\t');
        let (Some(flag), Some(refs), Some(summary)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let status = match flag {
            " " => PushRefStatus::FastForward,
            "+" => PushRefStatus::Forced,
            "-" => PushRefStatus::Deleted,
            "*" => PushRefStatus::Created,
            "!" => PushRefStatus::Rejected,
            "=" => PushRefStatus::UpToDate,
            _ => continue,
        };
        let (local, remote) = refs.split_once(':').unwrap_or(("", refs));
        let (summary, reason) = match summary.find(" (") {
            Some(i) if summary.ends_with(')') => (
                summary[..i].to_owned(),
                Some(summary[i + 2..summary.len() - 1].to_owned()),
            ),
            _ => (summary.to_owned(), None),
        };
        out.push(PushedRef {
            status,
            local: local.to_owned(),
            remote: remote.to_owned(),
            summary,
            reason,
        });
    }
    out
}

/// Result of [`Repo::push`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct PushOutcome {
    /// The remote pushed to.
    pub remote: String,
    /// The local branch.
    pub branch: String,
    /// The remote branch name.
    pub remote_branch: String,
    /// The upstream was set by this push.
    pub set_upstream: bool,
    /// Per-ref results.
    pub refs: Vec<PushedRef>,
    /// Nothing needed pushing.
    pub up_to_date: bool,
}

/// The current branch's upstream and how far apart they are.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct UpstreamStatus {
    /// Current branch (`None` when detached).
    pub branch: Option<String>,
    /// `HEAD` is detached.
    pub detached: bool,
    /// Upstream short name (`origin/main`) if configured and present.
    pub upstream: Option<String>,
    /// `branch.<b>.remote`.
    pub remote: Option<String>,
    /// The remote [`Repo::push`] would use by default.
    pub push_remote: Option<String>,
    /// An upstream is configured but its ref is gone (deleted on the remote and pruned).
    pub upstream_gone: bool,
    /// Commits to push.
    pub ahead: u32,
    /// Commits to pull.
    pub behind: u32,
}

/// Reflog entries of the local branch examined by the force-with-lease integration check.
const MAX_REFLOG_ENTRIES: usize = 1000;

fn reject_option_like(what: &str, value: &str) -> Result<()> {
    if value.is_empty() || value.starts_with('-') {
        return Err(SyncError::new(
            SyncErrorKind::InvalidName,
            format!("invalid {what}: {value:?}"),
        )
        .into());
    }
    Ok(())
}

impl Repo {
    pub(crate) fn config_value(&self, key: &str) -> Result<Option<String>> {
        let workdir = self.workdir();
        let os: Vec<&OsStr> = ["config", "--get", key]
            .into_iter()
            .map(OsStr::new)
            .collect();
        let cmd = proc::git(workdir, &os, "reading config").env("GIT_OPTIONAL_LOCKS", "0");
        let out = proc::run(cmd, &SyncOptions::default(), "reading config", None)?;
        // Exit 1 = not set.
        if !out.success() {
            return Ok(None);
        }
        let v = out.stdout_text().trim().to_owned();
        Ok((!v.is_empty()).then_some(v))
    }

    /// Full ref → oid for refs under the given prefixes.
    pub(crate) fn ref_map(&self, prefixes: &[&str]) -> Result<BTreeMap<String, Oid>> {
        let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
        args.extend_from_slice(prefixes);
        let out = proc::read(self.workdir(), &args, "listing refs")?;
        let mut map = BTreeMap::new();
        for line in String::from_utf8_lossy(&out).lines() {
            if let Some((oid, name)) = line.split_once(' ') {
                if let Ok(oid) = oid.parse::<Oid>() {
                    map.insert(name.to_owned(), oid);
                }
            }
        }
        Ok(map)
    }

    pub(crate) fn rev_parse_opt(&self, rev: &str) -> Result<Option<Oid>> {
        let os: Vec<&OsStr> = ["rev-parse", "--verify", "--quiet", "--end-of-options", rev]
            .into_iter()
            .map(OsStr::new)
            .collect();
        let cmd = proc::git(self.workdir(), &os, "resolving").env("GIT_OPTIONAL_LOCKS", "0");
        let out = proc::run(cmd, &SyncOptions::default(), "resolving", None)?;
        if !out.success() {
            return Ok(None);
        }
        Ok(out.stdout_text().trim().parse::<Oid>().ok())
    }

    /// Whether `oid` is reachable from `local_ref` or from one of the commits its reflog
    /// recorded (the most recent [`MAX_REFLOG_ENTRIES`]): the `--force-if-includes` rule.
    /// Revisions go through stdin (no command-line length limit on Windows).
    fn is_integrated(&self, workdir: &Path, local_ref: &str, oid: &str) -> Result<bool> {
        let reflog = proc::read(
            workdir,
            &["log", "-g", "--format=%H", local_ref, "--"],
            "reading the branch reflog",
        )
        .unwrap_or_default();
        let mut input = format!("{oid}\n^{local_ref}\n");
        let mut seen = std::collections::BTreeSet::new();
        for entry in String::from_utf8_lossy(&reflog).lines() {
            let entry = entry.trim();
            if seen.len() >= MAX_REFLOG_ENTRIES {
                break;
            }
            if entry.len() >= 40
                && entry.bytes().all(|b| b.is_ascii_hexdigit())
                && seen.insert(entry.to_owned())
            {
                input.push('^');
                input.push_str(entry);
                input.push('\n');
            }
        }
        let out = proc::write_raw(
            workdir,
            &["rev-list", "--count", "--stdin"],
            Some(input.as_bytes()),
            "checking the lease",
        )?;
        if !out.success() {
            return Err(proc::command_error(
                ["rev-list", "--count", "--stdin"].map(OsStr::new),
                &out,
            ));
        }
        Ok(out.stdout_text().trim() == "0")
    }

    pub(crate) fn count_commits(&self, range: &str) -> Result<u32> {
        let out = proc::read(
            self.workdir(),
            &["rev-list", "--count", range, "--"],
            "counting commits",
        )?;
        Ok(String::from_utf8_lossy(&out).trim().parse().unwrap_or(0))
    }

    /// The remote [`push`](Self::push) uses for `branch` by default.
    pub fn push_remote_for(&self, branch: &str) -> Result<Option<String>> {
        if let Some(r) = self.config_value(&format!("branch.{branch}.pushRemote"))? {
            return Ok(Some(r));
        }
        if let Some(r) = self.config_value("remote.pushDefault")? {
            return Ok(Some(r));
        }
        if let Some(r) = self.config_value(&format!("branch.{branch}.remote"))? {
            if r != "." {
                return Ok(Some(r));
            }
        }
        let remotes = self.remotes()?;
        if remotes.len() == 1 {
            return Ok(Some(remotes[0].name.clone()));
        }
        Ok(remotes
            .iter()
            .find(|r| r.name == "origin")
            .map(|r| r.name.clone()))
    }

    /// The current branch's upstream, remotes and ahead/behind counts.
    pub fn upstream_status(&self) -> Result<UpstreamStatus> {
        let head = self.head()?;
        let mut st = UpstreamStatus {
            branch: head.branch.clone(),
            detached: head.detached,
            upstream: head.upstream.clone(),
            ahead: head.ahead,
            behind: head.behind,
            ..UpstreamStatus::default()
        };
        if let Some(b) = &head.branch {
            st.remote = self.config_value(&format!("branch.{b}.remote"))?;
            let merge = self.config_value(&format!("branch.{b}.merge"))?;
            st.upstream_gone = merge.is_some() && head.upstream.is_none();
            st.push_remote = self.push_remote_for(b)?;
        }
        Ok(st)
    }

    /// `git fetch --progress [--prune] (<remote> | --all)`. Reports progress through `on`;
    /// returns the remote-tracking refs and tags that changed.
    pub fn fetch(
        &self,
        opts: &FetchOptions,
        sync: &SyncOptions,
        on: &mut dyn FnMut(SyncEvent),
    ) -> Result<FetchOutcome> {
        let workdir = self.require_workdir()?;
        let remotes = self.remotes()?;
        if remotes.is_empty() {
            return Err(SyncError::new(SyncErrorKind::NoRemote, "no remote is configured").into());
        }
        if let Some(r) = &opts.remote {
            reject_option_like("remote", r)?;
            if !remotes.iter().any(|x| &x.name == r) {
                return Err(SyncError::new(
                    SyncErrorKind::NoRemote,
                    format!("no remote named {r}"),
                )
                .into());
            }
        }
        let prefixes = ["refs/remotes", "refs/tags"];
        let before = self.ref_map(&prefixes)?;
        let mut args: Vec<&OsStr> = vec![OsStr::new("fetch"), OsStr::new("--progress")];
        if opts.prune {
            args.push(OsStr::new("--prune"));
        }
        let label = match &opts.remote {
            Some(r) => {
                args.push(OsStr::new(r));
                format!("git fetch {r}")
            }
            None => {
                args.push(OsStr::new("--all"));
                "git fetch --all".to_owned()
            }
        };
        let cmd = proc::git(workdir, &args, &label).kill_tree_on_exit(false);
        let mut sink = event_sink(on);
        let out = proc::run(cmd, sync, "fetching", Some(&mut sink))?;
        if !out.success() {
            return Err(failure(&out, SyncErrorKind::Other).into());
        }
        let after = self.ref_map(&prefixes)?;
        Ok(FetchOutcome {
            updated: diff_refs(&before, &after),
        })
    }

    /// `git pull --progress --no-rebase --ff-only` or `--rebase` for the current branch.
    /// A rebase that stops on conflicts is reported as [`PullStatus::Conflicts`] (not an
    /// error); the repository is then in the rebase state the conflict flow handles.
    pub fn pull(
        &self,
        mode: PullMode,
        sync: &SyncOptions,
        on: &mut dyn FnMut(SyncEvent),
    ) -> Result<PullOutcome> {
        let workdir = self.require_workdir()?;
        let head = self.head()?;
        let Some(branch) = head.branch.clone().filter(|_| !head.detached) else {
            return Err(SyncError::new(SyncErrorKind::DetachedHead, "pull needs a branch").into());
        };
        let Some(upstream) = head.upstream.clone() else {
            let mut e = SyncError::new(
                SyncErrorKind::NoUpstream,
                format!("{branch} has no upstream branch"),
            )
            .with_hint("Set an upstream, or push to create one.");
            if self
                .config_value(&format!("branch.{branch}.merge"))?
                .is_some()
            {
                e.message = format!("the upstream of {branch} is gone");
            }
            return Err(e.into());
        };
        let before = head.oid;
        let mode_arg = match mode {
            PullMode::FastForwardOnly => ["--no-rebase", "--ff-only"].as_slice(),
            PullMode::Rebase => ["--rebase"].as_slice(),
        };
        let mut args: Vec<&OsStr> = vec![OsStr::new("pull"), OsStr::new("--progress")];
        args.extend(mode_arg.iter().map(OsStr::new));
        let cmd =
            proc::git(workdir, &args, &format!("git pull ({branch})")).kill_tree_on_exit(false);
        let mut sink = event_sink(on);
        let out = proc::run(cmd, sync, "pulling", Some(&mut sink))?;
        let upstream_tip = self.rev_parse_opt("@{upstream}")?;
        let incoming = match (before, upstream_tip) {
            (Some(b), Some(u)) => self.count_commits(&format!("{b}..{u}"))?,
            (None, Some(u)) => self.count_commits(&u.to_string())?,
            _ => 0,
        };
        let mut outcome = PullOutcome {
            status: PullStatus::UpToDate,
            branch,
            upstream,
            before,
            after: before,
            incoming,
            rebased: 0,
            conflicts: Vec::new(),
        };
        if !out.success() {
            let op = self.operation()?;
            if op.as_ref().is_some_and(|o| o.kind == OperationKind::Rebase) {
                outcome.status = PullStatus::Conflicts;
                outcome.after = self.head().ok().and_then(|h| h.oid);
                outcome.conflicts = self.conflicts()?.into_iter().map(|c| c.path).collect();
                return Ok(outcome);
            }
            return Err(failure(&out, SyncErrorKind::Other).into());
        }
        let after = self.head()?.oid;
        outcome.after = after;
        if after == before {
            return Ok(outcome);
        }
        let fast_forward = match (before, after) {
            (Some(b), Some(a)) => self.merge_base(b, a)? == Some(b),
            _ => true,
        };
        if fast_forward {
            outcome.status = PullStatus::FastForwarded;
        } else {
            outcome.status = PullStatus::Rebased;
            if let (Some(a), Some(u)) = (after, upstream_tip) {
                outcome.rebased = self.count_commits(&format!("{u}..{a}"))?;
            }
        }
        Ok(outcome)
    }

    /// Pushes a branch (default: the current one) with `--porcelain --progress`, setting
    /// the upstream when missing (`opts.set_upstream`) and `--force-with-lease` (against the
    /// last fetched remote-tracking value) only when asked. Rejections are
    /// [`Error::Sync`] with [`SyncErrorKind::Rejected`] (hint "pull first"),
    /// [`SyncErrorKind::StaleLease`] or [`SyncErrorKind::HookDeclined`].
    pub fn push(
        &self,
        opts: &PushOptions,
        sync: &SyncOptions,
        on: &mut dyn FnMut(SyncEvent),
    ) -> Result<PushOutcome> {
        let workdir = self.require_workdir()?;
        let branch = match &opts.branch {
            Some(b) => b.clone(),
            None => {
                let head = self.head()?;
                match head.branch.filter(|_| !head.detached) {
                    Some(b) => b,
                    None => {
                        return Err(SyncError::new(
                            SyncErrorKind::DetachedHead,
                            "push needs a branch",
                        )
                        .into())
                    }
                }
            }
        };
        reject_option_like("branch", &branch)?;
        let local_ref = format!("refs/heads/{branch}");
        if self.rev_parse_opt(&local_ref)?.is_none() {
            return Err(SyncError::new(
                SyncErrorKind::NotFound,
                format!("no branch named {branch}"),
            )
            .into());
        }
        let remote = match &opts.remote {
            Some(r) => r.clone(),
            None => self.push_remote_for(&branch)?.ok_or_else(|| {
                SyncError::new(SyncErrorKind::NoRemote, "no remote is configured")
                    .with_hint("Add a remote first.")
            })?,
        };
        reject_option_like("remote", &remote)?;
        if !self.remotes()?.iter().any(|r| r.name == remote) {
            return Err(SyncError::new(
                SyncErrorKind::NoRemote,
                format!("no remote named {remote}"),
            )
            .into());
        }
        let up_remote = self.config_value(&format!("branch.{branch}.remote"))?;
        let up_merge = self.config_value(&format!("branch.{branch}.merge"))?;
        let remote_branch = match (&up_remote, &up_merge) {
            (Some(r), Some(m)) if *r == remote => {
                m.strip_prefix("refs/heads/").unwrap_or(m).to_owned()
            }
            _ => branch.clone(),
        };
        let set_upstream = opts.set_upstream && up_merge.is_none();
        let mut args: Vec<String> = vec!["push".into(), "--porcelain".into(), "--progress".into()];
        if set_upstream {
            args.push("--set-upstream".into());
        }
        if opts.force_with_lease {
            let tracking = format!("refs/remotes/{remote}/{remote_branch}");
            let expected = self.rev_parse_opt(&tracking)?;
            // The lease alone only proves the remote did not move since the last fetch; a
            // fetch (auto-fetch included) that brought a teammate's commits makes it pass
            // although those commits would be overwritten unseen. So the fetched tip must
            // also be integrated: reachable from the local branch or one of its reflog
            // entries (what `git push --force-if-includes` checks; that option is a no-op
            // with an explicit lease value, which is kept to avoid racing a fetch).
            if let Some(oid) = expected {
                if !self.is_integrated(workdir, &local_ref, &oid.to_string())? {
                    return Err(SyncError::new(
                        SyncErrorKind::StaleLease,
                        format!(
                            "{remote}/{remote_branch} has commits that were fetched but never \
                             integrated into {branch}; a forced push would discard them"
                        ),
                    )
                    .with_hint("Pull (or rebase onto the remote branch), review, then push again.")
                    .into());
                }
            }
            // Empty expectation = the remote branch must not exist yet.
            let expected = expected.map(|o| o.to_string()).unwrap_or_default();
            args.push(format!(
                "--force-with-lease=refs/heads/{remote_branch}:{expected}"
            ));
        }
        args.push(remote.clone());
        args.push(format!("{local_ref}:refs/heads/{remote_branch}"));
        let os: Vec<&OsStr> = args.iter().map(OsStr::new).collect();
        let cmd = proc::git(workdir, &os, &format!("git push {remote} {branch}"))
            .kill_tree_on_exit(false);
        let mut sink = event_sink(on);
        let out = proc::run(cmd, sync, "pushing", Some(&mut sink))?;
        let refs = parse_push_porcelain(&out.stdout_text());
        if !out.success() {
            let mut err = failure(&out, SyncErrorKind::Other);
            if let Some(rej) = refs.iter().find(|r| r.status == PushRefStatus::Rejected) {
                let reason = rej.reason.clone().unwrap_or_default();
                if reason.contains("stale info") {
                    err.kind = SyncErrorKind::StaleLease;
                    err.hint = Some("Fetch, review the remote changes, then push again.".into());
                } else if reason.contains("hook declined")
                    || rej.summary.contains("remote rejected")
                {
                    err.kind = SyncErrorKind::HookDeclined;
                } else {
                    err.kind = SyncErrorKind::Rejected;
                    err.hint = Some("Pull first, then push again.".into());
                }
            } else if refs.is_empty()
                && matches!(err.kind, SyncErrorKind::Other)
                && out.stderr_text().contains("failed to push some refs")
            {
                // A local pre-push hook failed: everything before git's own error is its output.
                err.kind = SyncErrorKind::HookDeclined;
                let hook: Vec<String> = out
                    .stderr_text()
                    .lines()
                    .filter(|l| !l.starts_with("error: failed to push"))
                    .map(redact_credentials)
                    .collect();
                err.hook_output = Some(hook.join("\n").trim().to_owned());
            }
            if err.kind == SyncErrorKind::HookDeclined && err.hook_output.is_none() {
                let hook: Vec<&str> = err
                    .message
                    .lines()
                    .filter_map(|l| l.strip_prefix("remote:").map(str::trim))
                    .collect();
                err.hook_output = Some(hook.join("\n"));
            }
            return Err(err.into());
        }
        let up_to_date =
            !refs.is_empty() && refs.iter().all(|r| r.status == PushRefStatus::UpToDate);
        Ok(PushOutcome {
            remote,
            branch,
            remote_branch,
            set_upstream,
            refs,
            up_to_date,
        })
    }
}

fn diff_refs(before: &BTreeMap<String, Oid>, after: &BTreeMap<String, Oid>) -> Vec<RefUpdate> {
    let mut out = Vec::new();
    for (name, new) in after {
        match before.get(name) {
            Some(old) if old == new => {}
            old => out.push(RefUpdate {
                name: name.clone(),
                old: old.copied(),
                new: Some(*new),
            }),
        }
    }
    for (name, old) in before {
        if !after.contains_key(name) {
            out.push(RefUpdate {
                name: name.clone(),
                old: Some(*old),
                new: None,
            });
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_progress_lines() {
        let p = parse_progress("Receiving objects:  33% (1/3), 1.20 MiB | 1.19 MiB/s").unwrap();
        assert_eq!(p.phase, "Receiving objects");
        assert!(!p.remote);
        assert_eq!(
            (p.percent, p.current, p.total),
            (Some(33), Some(1), Some(3))
        );
        assert_eq!(p.bytes, Some(1_258_291));
        assert_eq!(p.throughput, Some(1_247_805));
        assert!(!p.done);

        let p = parse_progress("remote: Counting objects: 100% (5/5), done.").unwrap();
        assert!(p.remote && p.done);
        assert_eq!(p.phase, "Counting objects");
        assert_eq!(p.percent, Some(100));

        let p = parse_progress("remote: Enumerating objects: 5, done.").unwrap();
        assert_eq!((p.current, p.total, p.percent), (Some(5), None, None));

        let p =
            parse_progress("Writing objects: 100% (3/3), 274 bytes | 274.00 KiB/s, done.").unwrap();
        assert_eq!(p.bytes, Some(274));
        assert_eq!(p.throughput, Some(280_576));
        assert!(p.done);

        let p =
            parse_progress("Resolving deltas: 100% (2/2), completed with 1 local object.").unwrap();
        assert!(p.done);
        assert_eq!(
            parse_progress("Updating files:  50% (1/2)")
                .unwrap()
                .percent,
            Some(50)
        );

        for not in [
            "From /tmp/remote",
            " * branch            main       -> FETCH_HEAD",
            "Total 3 (delta 0), reused 0 (delta 0), pack-reused 0",
            "error: failed to push some refs to 'x'",
            "fatal: Authentication failed for 'https://x/'",
            "hint: Updates were rejected",
            "remote: server says no",
            "To https://example.com/repo.git",
        ] {
            assert_eq!(parse_progress(not), None, "{not}");
        }
    }

    #[test]
    fn progress_from_carriage_return_updates() {
        // Captured `git clone --progress` stderr: `\r` updates then a final `\n`.
        let raw = b"Counting objects:  33% (1/3)\rCounting objects:  66% (2/3)\rCounting objects: 100% (3/3)\rCounting objects: 100% (3/3), done.\nTotal 3 (delta 0)\n";
        let mut splitter = lector_process::LineSplitter::default();
        let mut events = Vec::new();
        let mut on = |e: SyncEvent| events.push(e);
        let mut sink = event_sink(&mut on);
        let mut lines = Vec::new();
        splitter.feed(raw, &mut |bytes, end| {
            lines.push(Line {
                stream: Stream::Stderr,
                bytes,
                end,
            })
        });
        for l in &lines {
            sink(l);
        }
        drop(sink);
        let pct: Vec<Option<u8>> = events
            .iter()
            .filter_map(|e| match e {
                SyncEvent::Progress(p) => Some(p.percent),
                _ => None,
            })
            .collect();
        assert_eq!(pct, vec![Some(33), Some(66), Some(100), Some(100)]);
        assert!(matches!(events.last(), Some(SyncEvent::Message(m)) if m == "Total 3 (delta 0)"));
    }

    #[test]
    fn redacts_url_credentials() {
        assert_eq!(
            redact_credentials(
                "fatal: unable to access 'https://ann:ghp_secret@github.com/x.git/': 403"
            ),
            "fatal: unable to access 'https://***@github.com/x.git/': 403"
        );
        assert_eq!(
            redact_credentials("https://token@host/a and ssh://git@host:22/b and http://plain/c"),
            "https://***@host/a and ssh://***@host:22/b and http://plain/c"
        );
        assert_eq!(redact_credentials("no urls here"), "no urls here");
    }

    #[test]
    fn redaction_handles_hostile_user_info() {
        let cases = [
            // `/`, quotes and colons inside the password.
            ("https://u:ab/cd@host/x.git", "https://***@host/x.git"),
            ("'https://u:a'b\"c@host/x'", "'https://***@host/x'"),
            ("https://u:p:q@host:443/x", "https://***@host:443/x"),
            // A space inside the password.
            (
                "fatal: https://u:pa ss@host/x failed",
                "fatal: https://***@host/x failed",
            ),
            // Several URLs and lines.
            (
                "a https://x:y@h1/p b http://h2/q\nc ssh://git:t/k@h3:22/r",
                "a https://***@h1/p b http://h2/q\nc ssh://***@h3:22/r",
            ),
            // scp-like: the plain user name stays, a secret goes.
            (
                "git@github.com:owner/repo.git",
                "git@github.com:owner/repo.git",
            ),
            (
                "'ann:s3cret@example.com:repo.git'",
                "'***@example.com:repo.git'",
            ),
            // Nothing to redact.
            ("https://github.com/o/r.git", "https://github.com/o/r.git"),
            (
                "Signed-off-by: A <a@b.example>",
                "Signed-off-by: A <a@b.example>",
            ),
            ("ends with scheme://", "ends with scheme://"),
            ("", ""),
        ];
        for (input, want) in cases {
            assert_eq!(redact_credentials(input), want, "{input:?}");
        }
        for input in [
            "https://u:ab/cd@host",
            "x 'https://tok\"en@h/' y",
            "https://u:pa ss@host/x",
            "u:hunter2@host:r",
        ] {
            let out = redact_credentials(input);
            for secret in ["ab/cd", "tok", "pa ss", "hunter2"] {
                assert!(!out.contains(secret), "{input:?} -> {out:?}");
            }
        }
    }

    #[test]
    fn classifies_failures() {
        let cases = [
            ("fatal: Authentication failed for 'https://x/'", SyncErrorKind::AuthenticationFailed),
            ("fatal: could not read Username for 'https://x': terminal prompts disabled", SyncErrorKind::AuthenticationFailed),
            ("git@x: Permission denied (publickey).\nfatal: Could not read from remote repository.", SyncErrorKind::AuthenticationFailed),
            ("Host key verification failed.\nfatal: Could not read from remote repository.", SyncErrorKind::HostKeyUnknown),
            ("@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@", SyncErrorKind::HostKeyChanged),
            ("fatal: unable to access 'https://x/': Could not resolve host: x", SyncErrorKind::NetworkUnreachable),
            ("ssh: connect to host x port 22: Connection refused", SyncErrorKind::NetworkUnreachable),
            ("remote: Repository not found.\nfatal: repository 'https://x/' not found", SyncErrorKind::RemoteNotFound),
            ("fatal: '/nope' does not appear to be a git repository", SyncErrorKind::RemoteNotFound),
            ("fatal: Not possible to fast-forward, aborting.", SyncErrorKind::Diverged),
            ("There is no tracking information for the current branch.", SyncErrorKind::NoUpstream),
            ("error: cannot pull with rebase: You have unstaged changes.", SyncErrorKind::DirtyWorktree),
            ("error: the branch 'x' is not fully merged.", SyncErrorKind::NotMerged),
            ("fatal: 'a..b' is not a valid branch name", SyncErrorKind::InvalidName),
            ("fatal: a branch named 'x' already exists", SyncErrorKind::AlreadyExists),
        ];
        for (text, kind) in cases {
            assert_eq!(classify_failure(text), Some(kind), "{text}");
        }
        assert_eq!(classify_failure("something odd"), None);
    }

    #[test]
    fn lists_overwritten_paths() {
        let text = "error: Your local changes to the following files would be overwritten by checkout:\n\ta.txt\n\tdir/b.txt\nPlease commit your changes or stash them before you switch branches.\nerror: The following untracked working tree files would be overwritten by checkout:\n\tnew.txt\nAborting";
        assert_eq!(
            overwritten_paths(text),
            vec!["a.txt", "dir/b.txt", "new.txt"]
        );
        assert_eq!(classify_failure(text), Some(SyncErrorKind::LocalChanges));
    }

    #[test]
    fn parses_push_porcelain() {
        let out = "To /tmp/remote.git\n*\trefs/heads/main:refs/heads/main\t[new branch]\n \trefs/heads/a:refs/heads/a\tabc1234..def5678\n!\trefs/heads/b:refs/heads/b\t[rejected] (non-fast-forward)\n!\trefs/heads/c:refs/heads/c\t[remote rejected] (pre-receive hook declined)\n=\trefs/heads/d:refs/heads/d\t[up to date]\n+\trefs/heads/e:refs/heads/e\tabc...def (forced update)\nDone\n";
        let refs = parse_push_porcelain(out);
        let st: Vec<PushRefStatus> = refs.iter().map(|r| r.status).collect();
        assert_eq!(
            st,
            vec![
                PushRefStatus::Created,
                PushRefStatus::FastForward,
                PushRefStatus::Rejected,
                PushRefStatus::Rejected,
                PushRefStatus::UpToDate,
                PushRefStatus::Forced
            ]
        );
        assert_eq!(refs[2].reason.as_deref(), Some("non-fast-forward"));
        assert_eq!(refs[2].summary, "[rejected]");
        assert_eq!(refs[3].reason.as_deref(), Some("pre-receive hook declined"));
        assert_eq!(refs[1].local, "refs/heads/a");
        assert_eq!(refs[1].reason, None);
    }
}
