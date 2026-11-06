//! `lector status`: the cheapest command. It reads the repository with gix only (no
//! application core, no database), so it starts fast.

use std::path::PathBuf;

use anyhow::{anyhow, Context as _};
use lector_git::Repo;
use serde_json::{json, Value};

use crate::args::StatusArgs;
use crate::out::{print_json, Io};
use crate::session::resolve_dir;

/// Counts of changed paths by kind (a path can be both staged and unstaged).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    /// Paths with a staged change.
    pub staged: usize,
    /// Paths with an unstaged change.
    pub unstaged: usize,
    /// Untracked paths.
    pub untracked: usize,
    /// Paths with unresolved conflicts.
    pub conflicted: usize,
}

impl Counts {
    fn total(&self) -> usize {
        self.staged + self.unstaged + self.untracked + self.conflicted
    }
}

/// Everything `status` reports.
#[derive(Debug)]
pub struct Status {
    /// Work directory of the repository.
    pub root: PathBuf,
    /// The checked-out branch.
    pub branch: Option<String>,
    /// HEAD points at a commit rather than a branch.
    pub detached: bool,
    /// Full id of HEAD (`None` before the first commit).
    pub head: Option<String>,
    /// Short id of HEAD.
    pub head_short: Option<String>,
    /// Upstream branch.
    pub upstream: Option<String>,
    /// Commits ahead of the upstream.
    pub ahead: u32,
    /// Commits behind the upstream.
    pub behind: u32,
    /// Merge / rebase / ... in progress.
    pub operation: Option<lector_git::RepoOperation>,
    /// Change counts.
    pub counts: Counts,
}

/// Reads the status of the repository containing `dir`.
pub fn read_status(dir: &std::path::Path) -> anyhow::Result<Status> {
    let repo = Repo::discover(dir).map_err(|e| anyhow!("{e}"))?;
    let head = repo.head().context("cannot read HEAD")?;
    let entries = repo.status().context("cannot read the repository status")?;
    let mut counts = Counts::default();
    for e in &entries {
        if e.conflicted {
            counts.conflicted += 1;
            continue;
        }
        counts.staged += usize::from(e.staged.is_some());
        counts.unstaged += usize::from(e.unstaged.is_some());
        counts.untracked += usize::from(e.untracked);
    }
    let operation = repo.operation().unwrap_or_default();
    Ok(Status {
        root: repo.workdir().to_path_buf(),
        branch: head.branch,
        detached: head.detached,
        head: head.oid.map(|o| o.to_string()),
        head_short: head.oid.map(|o| o.short()),
        upstream: head.upstream,
        ahead: head.ahead,
        behind: head.behind,
        operation,
        counts,
    })
}

impl Status {
    /// The JSON document of `lector status --json`.
    pub fn to_json(&self) -> Value {
        json!({
            "root": self.root.to_string_lossy(),
            "branch": self.branch,
            "detached": self.detached,
            "head": self.head,
            "head_short": self.head_short,
            "upstream": self.upstream,
            "ahead": self.ahead,
            "behind": self.behind,
            "operation": self.operation.as_ref().map(|op| json!({
                "kind": op.kind.command(),
                "description": op.describe(),
                "interactive": op.interactive,
                "step": op.step.map(|(n, total)| json!([n, total])),
            })),
            "counts": {
                "staged": self.counts.staged,
                "unstaged": self.counts.unstaged,
                "untracked": self.counts.untracked,
                "conflicted": self.counts.conflicted,
            },
            "clean": self.counts.total() == 0 && self.operation.is_none(),
        })
    }
}

/// Runs `lector status`.
pub fn run(args: &StatusArgs, io: &mut Io) -> anyhow::Result<()> {
    let dir = resolve_dir(args.path.as_deref())?;
    let status = read_status(&dir)?;
    if args.json {
        return Ok(print_json(io.out, status.to_json())?);
    }
    let s = io.style;
    // Line 1: where HEAD is.
    let mut line = match (&status.branch, &status.head_short) {
        (Some(b), Some(_)) => s.bold(b),
        (Some(b), None) => format!("{} {}", s.bold(b), s.dim("(no commits yet)")),
        (None, Some(short)) => format!("{} {}", s.yellow("HEAD detached at"), s.bold(short)),
        (None, None) => s.dim("no commits yet"),
    };
    if let Some(up) = &status.upstream {
        line.push_str(&format!(" {} {up}", s.dim("→")));
        let mut sync = Vec::new();
        if status.ahead > 0 {
            sync.push(s.green(&format!("ahead {}", status.ahead)));
        }
        if status.behind > 0 {
            sync.push(s.yellow(&format!("behind {}", status.behind)));
        }
        if sync.is_empty() {
            line.push_str(&format!("  {}", s.dim("up to date")));
        } else {
            line.push_str(&format!("  {}", sync.join(", ")));
        }
    }
    writeln!(io.out, "{line}")?;
    if let Some(op) = &status.operation {
        let mut text = op.describe();
        if status.counts.conflicted > 0 {
            text.push_str(&format!(", {} conflicted", status.counts.conflicted));
        }
        writeln!(io.out, "{}", s.red(&text))?;
    }
    let c = status.counts;
    if c.total() == 0 && status.operation.is_none() {
        writeln!(io.out, "{}", s.dim("clean"))?;
    } else if c.total() > 0 {
        let mut parts = Vec::new();
        if c.staged > 0 {
            parts.push(s.green(&format!("staged {}", c.staged)));
        }
        if c.unstaged > 0 {
            parts.push(s.yellow(&format!("unstaged {}", c.unstaged)));
        }
        if c.untracked > 0 {
            parts.push(s.dim(&format!("untracked {}", c.untracked)));
        }
        if c.conflicted > 0 {
            parts.push(s.red(&format!("conflicted {}", c.conflicted)));
        }
        writeln!(io.out, "{}", parts.join(" · "))?;
    }
    Ok(())
}
