//! The `comments.*` registry commands: argument parsing, availability and the JSON each
//! command returns. All state lives in [`CommentStore`](super::CommentStore).

use anyhow::{anyhow, bail};
use lector_git::Changeset;
use serde_json::{json, Value};

use super::{
    export_markdown, local_author, side_text, thread_json, Anchor, CommentStore, DiffSide,
    NewThread, ThreadFilter, ThreadState,
};
use crate::commands::schema::{object, prop};
use crate::commands::{
    Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome,
};

/// Registers `comments.*` commands.
pub fn register_commands(r: &CommandRegistry) {
    r.register(list_cmd());
    r.register(add_cmd());
    r.register(reply_cmd());
    r.register(resolve_cmd());
    r.register(delete_cmd());
    r.register(export_cmd());
}

fn needs_repo(cx: &CommandContext) -> Availability {
    match &cx.project {
        None => Availability::no("No folder is open"),
        Some(p) if p.repo().is_none() => Availability::no("Not a git repository"),
        Some(_) => Availability::Available,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn u64_arg(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

/// The review key and (when it is a changeset) the changeset of the arguments / focus.
fn review_key(cx: &CommandContext, args: &Value) -> (String, Option<Changeset>) {
    match str_arg(args, "changeset") {
        Some(key) => (
            key.to_string(),
            lector_review::review::parse_changeset_key(key),
        ),
        None => {
            let cs = cx.focus.changeset.clone().unwrap_or(Changeset::WorkingTree);
            (lector_review::review::changeset_key(&cs), Some(cs))
        }
    }
}

fn changeset_schema() -> Value {
    prop(
        "string",
        "Review key: a changeset key (worktree, staged, commit:<sha>, range:<a>..<b>, branch:<base>...<branch>) or a pull-request key (pr:<host>/<repo>#<n>); default: the focused changeset or worktree",
    )
}

fn list_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.list",
        "List Review Comments",
        "Review",
        object(
            &[
                ("changeset", changeset_schema()),
                ("path", prop("string", "Only this repo-relative path")),
                (
                    "state",
                    json!({"type": "string", "enum": ["open", "resolved", "outdated", "all"],
                           "description": "Filter by state (default all)"}),
                ),
                (
                    "drafts_only",
                    prop("boolean", "Only threads with draft comments"),
                ),
                (
                    "all_changesets",
                    prop("boolean", "Every review key of the repository"),
                ),
            ],
            &[],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let store = CommentStore::for_core(&cx.core);
        let (key, cs) = review_key(cx, args);
        let all = args.get("all_changesets").and_then(Value::as_bool) == Some(true);
        if !all {
            if let Some(cs) = &cs {
                if let Err(err) = store.sync_changeset(repo, &key, cs) {
                    tracing::debug!("re-anchoring skipped: {err}");
                }
            }
        }
        let states = match str_arg(args, "state") {
            Some("open") => vec![ThreadState::Open],
            Some("resolved") => vec![ThreadState::Resolved],
            Some("outdated") => vec![ThreadState::Outdated],
            _ => vec![],
        };
        let filter = ThreadFilter {
            changeset: (!all).then_some(key.clone()),
            path: str_arg(args, "path").map(str::to_string),
            states,
            drafts_only: args.get("drafts_only").and_then(Value::as_bool) == Some(true),
            origin: None,
        };
        let threads = store.threads(repo.workdir(), &filter)?;
        Ok(
            Outcome::new(format!("{} comment thread(s)", threads.len())).with_data(json!({
                "changeset": key,
                "threads": threads.iter().map(thread_json).collect::<Vec<_>>(),
            })),
        )
    })
    .with_available(needs_repo)
}

fn add_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.add",
        "Add Review Comment",
        "Review",
        object(
            &[
                ("path", prop("string", "Repo-relative path")),
                ("line", prop("integer", "0-based line")),
                (
                    "end_line",
                    prop("integer", "0-based last line of a range (inclusive)"),
                ),
                (
                    "side",
                    json!({"type": "string", "enum": ["new", "old"], "description": "Diff side (default new)"}),
                ),
                ("body", prop("string", "Markdown body")),
                ("changeset", changeset_schema()),
                (
                    "draft",
                    prop(
                        "boolean",
                        "Keep as a draft of a pending pull-request review",
                    ),
                ),
                ("author", prop("string", "Author (default: git user.name)")),
            ],
            &["path", "line", "body"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let path = str_arg(args, "path").ok_or_else(|| anyhow!("missing path"))?;
        let path = path.trim_start_matches("./").replace('\\', "/");
        let line = u64_arg(args, "line").unwrap_or(0) as u32;
        let end_line = u64_arg(args, "end_line").map(|v| v as u32);
        let side = match str_arg(args, "side") {
            Some("old") => DiffSide::Old,
            _ => DiffSide::New,
        };
        let body = str_arg(args, "body").ok_or_else(|| anyhow!("missing body"))?;
        let (key, cs) = review_key(cx, args);
        let (text, blob) = side_text(repo, cs.as_ref(), &path, side)
            .ok_or_else(|| anyhow!("cannot read {path} on the {side:?} side of {key}"))?;
        let anchor = Anchor::capture(&path, side, &text, line, end_line, &key, blob)
            .ok_or_else(|| anyhow!("line {} is past the end of {path}", line + 1))?;
        let store = CommentStore::for_core(&cx.core);
        store.remember_text(&text);
        let author = str_arg(args, "author")
            .map(str::to_string)
            .unwrap_or_else(|| local_author(repo.workdir()));
        let t = store.add(NewThread {
            repo: repo.workdir().to_path_buf(),
            anchor,
            body: body.to_string(),
            author,
            draft: args.get("draft").and_then(Value::as_bool) == Some(true),
            ..Default::default()
        })?;
        Ok(Outcome::new(format!("Comment added on {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn reply_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.reply",
        "Reply to Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                ("body", prop("string", "Markdown body")),
                ("draft", prop("boolean", "Keep as a draft")),
                ("author", prop("string", "Author (default: git user.name)")),
            ],
            &["thread", "body"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let body = str_arg(args, "body").ok_or_else(|| anyhow!("missing body"))?;
        let author = str_arg(args, "author")
            .map(str::to_string)
            .unwrap_or_else(|| local_author(repo.workdir()));
        let draft = args.get("draft").and_then(Value::as_bool) == Some(true);
        let t = CommentStore::for_core(&cx.core).reply(repo.workdir(), id, body, &author, draft)?;
        Ok(Outcome::new(format!("Replied on {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn resolve_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.resolve",
        "Resolve Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                ("resolved", prop("boolean", "false reopens (default true)")),
            ],
            &["thread"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let resolved = args.get("resolved").and_then(Value::as_bool) != Some(false);
        let t = CommentStore::for_core(&cx.core).set_resolved(repo.workdir(), id, resolved)?;
        let verb = if resolved { "Resolved" } else { "Reopened" };
        Ok(Outcome::new(format!("{verb} {}", t.location())).with_data(thread_json(&t)))
    })
    .with_available(needs_repo)
}

fn delete_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.delete",
        "Delete Review Comment",
        "Review",
        object(
            &[
                ("thread", prop("integer", "Thread id")),
                (
                    "comment",
                    prop("integer", "Only this comment of the thread"),
                ),
            ],
            &["thread"],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let id = u64_arg(args, "thread").ok_or_else(|| anyhow!("missing thread"))?;
        let store = CommentStore::for_core(&cx.core);
        match u64_arg(args, "comment") {
            Some(c) => {
                let left = store.delete_comment(repo.workdir(), id, c)?;
                Ok(Outcome::new("Comment deleted").with_data(json!({
                    "thread": left.as_ref().map(thread_json),
                })))
            }
            None => {
                store.delete(repo.workdir(), id)?;
                Ok(Outcome::new("Thread deleted").with_data(json!({ "thread": null })))
            }
        }
    })
    .with_available(needs_repo)
}

fn export_cmd() -> FnCommand {
    let spec = CommandSpec::read(
        "comments.export",
        "Export Review Comments",
        "Review",
        object(
            &[
                ("changeset", changeset_schema()),
                (
                    "format",
                    json!({"type": "string", "enum": ["markdown", "json"], "description": "Default markdown"}),
                ),
                (
                    "include_resolved",
                    prop("boolean", "Include resolved threads (default false)"),
                ),
            ],
            &[],
        ),
    );
    FnCommand::new(spec, |cx, args| {
        let repo = cx.require_repo()?;
        let (key, _) = review_key(cx, args);
        let mut threads = CommentStore::for_core(&cx.core)
            .threads(repo.workdir(), &ThreadFilter::changeset(key.clone()))?;
        if args.get("include_resolved").and_then(Value::as_bool) != Some(true) {
            threads.retain(|t| t.state != ThreadState::Resolved);
        }
        match str_arg(args, "format") {
            Some("json") => Ok(
                Outcome::new(format!("{} thread(s)", threads.len())).with_data(
                    json!({ "threads": threads.iter().map(thread_json).collect::<Vec<_>>() }),
                ),
            ),
            Some("markdown") | None => {
                let md = export_markdown(&key, &threads);
                Ok(
                    Outcome::new(format!("{} thread(s) exported", threads.len()))
                        .with_data(json!({ "markdown": md })),
                )
            }
            Some(other) => bail!("unknown format {other}"),
        }
    })
    .with_available(needs_repo)
}
