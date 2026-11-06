//! Read / navigation / workspace commands. None of them writes to the user's
//! files or repository (`crates/lector-app/tests/read_only.rs` enforces it).
//!
//! Every command has a JSON schema so the CLI and MCP can expose it; results are
//! compact JSON: project-relative paths, 0-based lines, ranges as `[start, end)`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context as _};
use lector_git::{Changeset, LogQuery};
use lector_text::Point;
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::schema::{object, prop};
use super::{
    Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome, UiRequest,
};

/// Registers the read commands.
pub fn register(r: &CommandRegistry) {
    r.register(open_path());
    r.register(open_location());
    r.register(open_changeset());
    r.register(search_project());
    r.register(find_symbol());
    r.register(list_changes());
    r.register(show_diff());
    r.register(blame());
    r.register(history());
    r.register(list_commands());
    r.register(settings_get());
}

fn needs_project(cx: &CommandContext) -> Availability {
    if cx.project.is_some() {
        Availability::Available
    } else {
        Availability::no("No folder is open")
    }
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

fn u32_arg(args: &Value, key: &str) -> Option<u32> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v.min(u32::MAX as u64) as u32)
}

fn usize_arg(args: &Value, key: &str, default: usize) -> usize {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|v| v as usize)
        .unwrap_or(default)
}

/// Resolves a path argument: absolute, else relative to the project root, else
/// the focused file.
fn resolve_path(cx: &CommandContext, args: &Value) -> anyhow::Result<PathBuf> {
    match str_arg(args, "path") {
        Some(p) => {
            let p = Path::new(p);
            if p.is_absolute() {
                Ok(p.to_path_buf())
            } else {
                Ok(cx.require_project()?.absolute_path(&p.to_string_lossy()))
            }
        }
        None => cx
            .focus
            .path
            .clone()
            .ok_or_else(|| anyhow!("no `path` given and no file is active")),
    }
}

fn rel(cx: &CommandContext, abs: &Path) -> String {
    cx.project
        .as_ref()
        .and_then(|p| p.relative_path(abs))
        .unwrap_or_else(|| abs.to_string_lossy().into_owned())
}

/// Repo-relative path for git calls.
fn repo_rel(cx: &CommandContext, abs: &Path) -> anyhow::Result<String> {
    let project = cx.require_project()?;
    let rel = project
        .relative_path(abs)
        .ok_or_else(|| anyhow!("{} is outside the project", abs.display()))?;
    Ok(project.repo_path(&rel))
}

fn changeset_arg(cx: &CommandContext, args: &Value) -> anyhow::Result<Changeset> {
    match str_arg(args, "changeset") {
        Some(key) => lector_workspace::review::parse_changeset_key(key).ok_or_else(|| {
            anyhow!("unknown changeset `{key}` (worktree, staged, unstaged, untracked, commit:<sha>, range:<a>..<b>, branch:<base>...<branch>)")
        }),
        None => Ok(cx.focus.changeset.clone().unwrap_or(Changeset::WorkingTree)),
    }
}

fn changeset_schema() -> Value {
    prop(
        "string",
        "Changeset key: worktree (default), staged, unstaged, untracked, commit:<sha>, range:<from>..<to>, branch:<base>...<branch>",
    )
}

/// Parses `path[:line[:column]]` (1-based line/column, as printed by compilers).
pub fn parse_location(s: &str) -> (String, Option<u32>, Option<u32>) {
    let mut parts: Vec<&str> = s.rsplitn(3, ':').collect();
    parts.reverse();
    let num = |x: &str| x.trim().parse::<u32>().ok().filter(|n| *n > 0);
    match parts.as_slice() {
        [p, l, c] if num(l).is_some() && num(c).is_some() => (p.to_string(), num(l), num(c)),
        [p, l, c] if num(c).is_some() => (format!("{p}:{l}"), num(c), None),
        [p, l] if num(l).is_some() => (p.to_string(), num(l), None),
        _ => (s.to_string(), None, None),
    }
}

fn open_point(line: Option<u32>, column: Option<u32>) -> Option<Point> {
    line.map(|l| Point {
        row: l,
        column: column.unwrap_or(0),
    })
}

fn open_path() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "workspace.open_path",
            "Open File",
            "File",
            object(
                &[
                    ("path", prop("string", "Absolute or project-relative path")),
                    (
                        "line",
                        json!({"type": "integer", "minimum": 0, "description": "0-based line"}),
                    ),
                    (
                        "column",
                        json!({"type": "integer", "minimum": 0, "description": "0-based byte column"}),
                    ),
                ],
                &["path"],
            ),
        ),
        |cx, args| {
            let path = resolve_path(cx, args)?;
            if !path.exists() {
                bail!("{} does not exist", path.display());
            }
            let point = open_point(u32_arg(args, "line"), u32_arg(args, "column"));
            let mut data = json!({ "path": rel(cx, &path) });
            if let Some(p) = point {
                data["line"] = json!(p.row);
            }
            Ok(Outcome::new(format!("Opened {}", rel(cx, &path)))
                .with_data(data)
                .with_ui(UiRequest::OpenPath { path, point }))
        },
    )
}

fn open_location() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "workspace.open_location",
            "Open Location (path:line:col)",
            "File",
            object(
                &[(
                    "location",
                    prop(
                        "string",
                        "`path[:line[:column]]`, 1-based line/column as printed by compilers",
                    ),
                )],
                &["location"],
            ),
        ),
        |cx, args| {
            let (p, line, col) = parse_location(str_arg(args, "location").unwrap_or_default());
            let mut inner = json!({ "path": p });
            if let Some(l) = line {
                inner["line"] = json!(l - 1);
            }
            if let Some(c) = col {
                inner["column"] = json!(c - 1);
            }
            super::Command::apply(&open_path(), cx, &inner)
        },
    )
}

fn open_changeset() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.open_changeset",
            "Review Changeset",
            "Git",
            object(&[("changeset", changeset_schema())], &["changeset"]),
        ),
        |cx, args| {
            let cs = changeset_arg(cx, args)?;
            let title = lector_workspace::review::changeset_title(cx.project.as_ref().and_then(|p| p.repo()), &cs);
            Ok(Outcome::new(format!("Opened {title}"))
                .with_data(json!({ "changeset": lector_workspace::review::changeset_key(&cs), "title": title }))
                .with_ui(UiRequest::OpenChangeset(cs)))
        },
    )
    .with_available(needs_repo)
}

fn search_project() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "search.project",
            "Search in Project",
            "Search",
            object(
                &[
                    ("pattern", prop("string", "Text or regex to find")),
                    ("regex", prop("boolean", "Treat pattern as a regular expression")),
                    ("case", json!({"type": "string", "enum": ["smart", "sensitive", "insensitive"]})),
                    ("whole_word", prop("boolean", "Match whole words")),
                    ("include", json!({"type": "array", "items": {"type": "string"}, "description": "Globs to include"})),
                    ("exclude", json!({"type": "array", "items": {"type": "string"}, "description": "Globs to exclude"})),
                    ("max_results", json!({"type": "integer", "minimum": 1, "description": "Matching lines (default 200)"})),
                ],
                &["pattern"],
            ),
        ),
        |cx, args| {
            let project = cx.require_project()?;
            let settings = project.settings();
            let strings = |k: &str| -> Vec<String> {
                args.get(k)
                    .and_then(Value::as_array)
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default()
            };
            let query = lector_search::SearchQuery {
                pattern: str_arg(args, "pattern").unwrap_or_default().to_string(),
                regex: args.get("regex").and_then(Value::as_bool).unwrap_or(false),
                case: match str_arg(args, "case") {
                    Some("sensitive") => lector_search::CaseMode::Sensitive,
                    Some("insensitive") => lector_search::CaseMode::Insensitive,
                    _ => lector_search::CaseMode::Smart,
                },
                whole_word: args.get("whole_word").and_then(Value::as_bool).unwrap_or(false),
                include: strings("include"),
                exclude: strings("exclude"),
                max_results: usize_arg(args, "max_results", 200),
            };
            let walk = lector_search::WalkOptions {
                hidden: settings.files.show_hidden,
                git_ignore: true,
                exclude: settings.files.exclude.clone(),
                max_depth: None,
            };
            let files = Mutex::new(Vec::new());
            let done = Mutex::new((0usize, false));
            let error = Mutex::new(None);
            let cancel = lector_search::CancelToken::new();
            let result = lector_search::search_project(project.root(), &query, &walk, &cancel, |ev| match ev {
                lector_search::SearchEvent::File(fm) => files.lock().push(fm),
                lector_search::SearchEvent::Done { matches, truncated, .. } => *done.lock() = (matches, truncated),
                lector_search::SearchEvent::Error(e) => *error.lock() = Some(e),
            });
            if let Some(e) = error.into_inner() {
                bail!("{e}");
            }
            result.map_err(|e| anyhow!("{e}"))?;
            let mut files = files.into_inner();
            files.sort_by(|a, b| a.path.cmp(&b.path));
            let (matches, truncated) = done.into_inner();
            let out: Vec<Value> = files
                .iter()
                .map(|f| {
                    json!({
                        "path": f.path.to_string_lossy().replace('\\', "/"),
                        "matches": f.matches.iter().map(|m| json!({
                            "line": m.line,
                            "text": m.text,
                            "ranges": m.ranges.iter().map(|r| [r.start, r.end]).collect::<Vec<_>>(),
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            Ok(Outcome::new(format!("{matches} matches in {} files", files.len()))
                .with_data(json!({ "files": out, "matches": matches, "truncated": truncated })))
        },
    )
    .with_available(needs_project)
}

fn find_symbol() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "symbols.find",
            "Find Symbol in Project",
            "Navigate",
            object(
                &[
                    ("name", prop("string", "Symbol name (exact, or a case-insensitive substring unless `exact`)")),
                    ("exact", prop("boolean", "Only exact name matches")),
                    ("limit", json!({"type": "integer", "minimum": 1, "description": "Maximum results (default 50)"})),
                ],
                &["name"],
            ),
        ),
        |cx, args| {
            let project = cx.require_project()?;
            let name = str_arg(args, "name").unwrap_or_default();
            let exact = args.get("exact").and_then(Value::as_bool).unwrap_or(false);
            let limit = usize_arg(args, "limit", 50);
            if project.symbols().file_count() == 0 {
                project.index_symbols(&lector_search::CancelToken::new(), |_, _| {});
            }
            let mut found = project.symbols().definitions(name);
            if !exact {
                let lower = name.to_lowercase();
                let mut fuzzy: Vec<_> = lector_workspace::symbols::project_symbols(project)
                    .into_iter()
                    .filter(|s| s.name != name && s.name.to_lowercase().contains(&lower))
                    .collect();
                fuzzy.sort_by_key(|s| (s.name.len(), s.name.clone()));
                found.extend(fuzzy);
            }
            let total = found.len();
            let out: Vec<Value> = found
                .into_iter()
                .take(limit)
                .map(|s| {
                    json!({
                        "name": s.name,
                        "kind": format!("{:?}", s.kind).to_lowercase(),
                        "path": rel(cx, &s.path),
                        "line": s.point.row,
                        "column": s.point.column,
                    })
                })
                .collect();
            let mut outcome = Outcome::new(format!("{total} symbols matching `{name}`"))
                .with_data(json!({ "symbols": out, "total": total }));
            if let Some(first) = out_first(&outcome.data, cx) {
                outcome = outcome.with_ui(first);
            }
            Ok(outcome)
        },
    )
    .with_available(needs_project)
}

/// Opening the first result is what the UI does for a single match.
fn out_first(data: &Value, cx: &CommandContext) -> Option<UiRequest> {
    let syms = data.get("symbols")?.as_array()?;
    if syms.len() != 1 {
        return None;
    }
    let s = &syms[0];
    let path = cx.project.as_ref()?.absolute_path(s.get("path")?.as_str()?);
    Some(UiRequest::OpenPath {
        path,
        point: Some(Point {
            row: s.get("line")?.as_u64()? as u32,
            column: s.get("column")?.as_u64()? as u32,
        }),
    })
}

fn list_changes() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.list_changes",
            "List Changes",
            "Git",
            object(&[("changeset", changeset_schema())], &[]),
        ),
        |cx, args| {
            let project = cx.require_project()?;
            let cs = changeset_arg(cx, args)?;
            let view =
                lector_workspace::review::load_changeset(project, &cs, &cx.core.review_store())
                    .context("cannot load changeset")?;
            let files: Vec<Value> = view
                .files
                .iter()
                .map(|f| {
                    let mut v = json!({
                        "path": f.path(),
                        "kind": format!("{:?}", f.change.kind).to_lowercase(),
                    });
                    if let (Some(old), Some(new)) = (&f.change.old_path, &f.change.new_path) {
                        if old != new {
                            v["old_path"] = json!(old);
                        }
                    }
                    if let Some(s) = f.stats {
                        v["added"] = json!(s.added);
                        v["removed"] = json!(s.removed);
                    }
                    if f.binary {
                        v["binary"] = json!(true);
                    }
                    if f.viewed {
                        v["viewed"] = json!(true);
                    }
                    v
                })
                .collect();
            let mut data = json!({ "changeset": view.key, "title": view.title, "files": files });
            if cs == Changeset::WorkingTree {
                if let Some(repo) = project.repo() {
                    let state = cx.core.git().repo_for(repo);
                    let mut snap = state.snapshot();
                    if snap.generation == 0 {
                        snap = state.refresh_blocking();
                    }
                    if let Some(op) = &snap.conflicts.operation {
                        data["operation"] = json!(format!("{op:?}"));
                    }
                    if !snap.conflicts.entries.is_empty() {
                        data["conflicts"] = json!(snap.conflicts.entries.len());
                    }
                    if let Some(b) = snap.branch_label() {
                        data["branch"] = json!(b);
                    }
                }
            }
            Ok(Outcome::new(format!("{}: {} files", view.title, view.files.len())).with_data(data))
        },
    )
    .with_available(needs_repo)
}

fn show_diff() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.show_diff",
            "Show Diff",
            "Git",
            object(
                &[
                    ("changeset", changeset_schema()),
                    ("path", prop("string", "Only this file (repo- or project-relative)")),
                    ("context", json!({"type": "integer", "minimum": 0, "maximum": 50, "description": "Context lines (default 3)"})),
                    ("max_files", json!({"type": "integer", "minimum": 1, "description": "Default 50"})),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let project = cx.require_project()?;
            let cs = changeset_arg(cx, args)?;
            let ctx_lines = u32_arg(args, "context").unwrap_or(3);
            let max_files = usize_arg(args, "max_files", 50);
            let view = lector_workspace::review::load_changeset(project, &cs, &cx.core.review_store())
                .context("cannot load changeset")?;
            let only = str_arg(args, "path").map(|p| p.trim_start_matches("./").to_string());
            let opts = lector_workspace::review::diff_options(&project.settings().diff);
            let mut out = Vec::new();
            let selected: Vec<_> = view
                .files
                .iter()
                .filter(|f| only.as_deref().is_none_or(|p| f.path() == p || project.repo_path(p) == f.path()))
                .collect();
            if only.is_some() && selected.is_empty() {
                bail!("`{}` is not changed in {}", only.unwrap_or_default(), view.title);
            }
            for f in selected.iter().take(max_files) {
                let diff = lector_workspace::review::load_file_diff(project, &f.change, &opts)?;
                out.push(file_diff_json(f.path(), &diff, ctx_lines));
            }
            Ok(Outcome::new(format!("{} — {} files", view.title, selected.len()))
                .with_data(json!({
                    "changeset": view.key,
                    "files": out,
                    "truncated": selected.len() > max_files,
                })))
        },
    )
    .with_available(needs_repo)
}

/// One file's diff as compact JSON: hunks with git-style headers and `+`/`-`/` ` lines.
pub fn file_diff_json(
    path: &str,
    diff: &lector_workspace::review::FileDiff,
    context: u32,
) -> Value {
    use lector_workspace::review::DiffKind;
    let mut v = json!({
        "path": path,
        "kind": format!("{:?}", diff.change.kind).to_lowercase(),
    });
    match diff.kind {
        DiffKind::Text => {}
        DiffKind::Binary => {
            v["binary"] = json!(true);
            return v;
        }
        DiffKind::Image(_) => {
            v["image"] = json!(true);
            return v;
        }
        DiffKind::TooLarge => {
            v["too_large"] = json!(true);
            return v;
        }
    }
    let stats = diff.stats();
    v["added"] = json!(stats.added);
    v["removed"] = json!(stats.removed);
    let old_n = diff.old.line_count();
    let new_n = diff.new.line_count();
    let hunks: Vec<Value> = diff
        .diff
        .hunks
        .iter()
        .map(|h| {
            let before = context.min(h.old.start);
            let after_old = (h.old.end + context).min(old_n);
            let after = after_old - h.old.end;
            let mut lines = Vec::new();
            for r in h.old.start - before..h.old.start {
                lines.push(format!(" {}", diff.old.line(r)));
            }
            for r in h.old.clone() {
                lines.push(format!("-{}", diff.old.line(r)));
            }
            for r in h.new.clone() {
                lines.push(format!("+{}", diff.new.line(r)));
            }
            for r in h.new.end..(h.new.end + after).min(new_n) {
                lines.push(format!(" {}", diff.new.line(r)));
            }
            let os = h.old.start - before;
            let ol = h.old.len() as u32 + before + after;
            let ns = h.new.start.saturating_sub(before);
            let nl = h.new.len() as u32 + before + after;
            json!({
                "header": format!("@@ -{},{} +{},{} @@", os + 1, ol, ns + 1, nl),
                "old": [h.old.start, h.old.end],
                "new": [h.new.start, h.new.end],
                "lines": lines,
            })
        })
        .collect();
    v["hunks"] = json!(hunks);
    v
}

fn blame() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.blame",
            "Blame",
            "Git",
            object(
                &[
                    ("path", prop("string", "File (default: the active file)")),
                    ("start_line", json!({"type": "integer", "minimum": 0})),
                    (
                        "end_line",
                        json!({"type": "integer", "minimum": 0, "description": "Exclusive"}),
                    ),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let project = cx.require_project()?;
            let path = resolve_path(cx, args)?;
            let rel_path = project
                .relative_path(&path)
                .ok_or_else(|| anyhow!("{} is outside the project", path.display()))?;
            let hunks = project.blame(&rel_path)?;
            let start = u32_arg(args, "start_line").unwrap_or(0);
            let end = u32_arg(args, "end_line").unwrap_or(u32::MAX);
            let out: Vec<Value> = hunks
                .iter()
                .filter(|h| h.lines.end > start && h.lines.start < end)
                .map(|h| {
                    let mut v = json!({
                        "lines": [h.lines.start, h.lines.end],
                        "commit": h.oid.short(),
                        "author": h.author,
                        "time": h.time,
                        "summary": h.summary,
                    });
                    if h.uncommitted {
                        v["uncommitted"] = json!(true);
                    }
                    if h.orig_path != rel_path {
                        v["orig_path"] = json!(h.orig_path);
                    }
                    v
                })
                .collect();
            Ok(
                Outcome::new(format!("Blame {rel_path}: {} blocks", out.len()))
                    .with_data(json!({ "path": rel_path, "blame": out })),
            )
        },
    )
    .with_available(needs_repo)
}

fn history() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "git.history",
            "Show History",
            "Git",
            object(
                &[
                    ("path", prop("string", "File history (follows renames)")),
                    ("start_line", json!({"type": "integer", "minimum": 0, "description": "Line history (with path)"})),
                    ("end_line", json!({"type": "integer", "minimum": 0, "description": "Exclusive"})),
                    ("rev", prop("string", "Start revision (default HEAD)")),
                    ("author", prop("string", "Author substring")),
                    ("message", prop("string", "Message substring")),
                    ("limit", json!({"type": "integer", "minimum": 1, "maximum": 1000, "description": "Default 50"})),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let repo = cx.require_repo()?;
            let limit = usize_arg(args, "limit", 50);
            let path = match str_arg(args, "path") {
                Some(_) => Some(repo_rel(cx, &resolve_path(cx, args)?)?),
                None => None,
            };
            let commits = match (&path, u32_arg(args, "start_line")) {
                (Some(p), Some(start)) => {
                    let end = u32_arg(args, "end_line").unwrap_or(start + 1).max(start + 1);
                    repo.line_history(p, start..end, limit)?
                }
                (Some(p), None) if str_arg(args, "author").is_none() && str_arg(args, "message").is_none() => {
                    repo.file_history(p, limit)?
                }
                _ => repo.log(&LogQuery {
                    rev: str_arg(args, "rev").map(String::from),
                    path: path.clone(),
                    author: str_arg(args, "author").map(String::from),
                    message: str_arg(args, "message").map(String::from),
                    limit,
                    ..Default::default()
                })?,
            };
            let out: Vec<Value> = commits
                .iter()
                .map(|c| {
                    let mut v = json!({
                        "commit": c.oid.to_string(),
                        "author": c.author.name,
                        "time": c.author.time,
                        "summary": c.summary,
                    });
                    if c.parents.len() > 1 {
                        v["merge"] = json!(true);
                    }
                    if !c.refs.is_empty() {
                        v["refs"] = json!(c.refs.iter().map(|r| r.name.clone()).collect::<Vec<_>>());
                    }
                    v
                })
                .collect();
            Ok(Outcome::new(format!("{} commits", out.len())).with_data(json!({ "commits": out })))
        },
    )
    .with_available(needs_repo)
}

fn list_commands() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "commands.list",
            "List Commands",
            "Help",
            object(
                &[
                    ("category", prop("string", "Only this category")),
                    (
                        "available_only",
                        prop("boolean", "Hide commands unavailable in this context"),
                    ),
                ],
                &[],
            ),
        ),
        |cx, args| {
            let category = str_arg(args, "category");
            let available_only = args
                .get("available_only")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let mut out = Vec::new();
            for c in cx.core.registry().list() {
                let spec = c.spec();
                if category.is_some_and(|cat| !spec.category.eq_ignore_ascii_case(cat)) {
                    continue;
                }
                let mut v = spec.to_json();
                if let Availability::Unavailable(why) = c.available(cx) {
                    if available_only {
                        continue;
                    }
                    v["unavailable"] = json!(why);
                }
                out.push(v);
            }
            Ok(Outcome::new(format!("{} commands", out.len()))
                .with_data(json!({ "commands": out })))
        },
    )
}

fn settings_get() -> FnCommand {
    FnCommand::new(
        CommandSpec::read(
            "settings.get",
            "Get Setting",
            "Settings",
            object(
                &[(
                    "key",
                    prop(
                        "string",
                        "Dotted key (`git.confirm_discard`); omit for all settings",
                    ),
                )],
                &[],
            ),
        ),
        |cx, args| {
            let settings = match &cx.project {
                Some(p) => p.settings(),
                None => lector_config::load_settings().0,
            };
            let all = serde_json::to_value(&settings)?;
            let value = match str_arg(args, "key") {
                None => all,
                Some(key) => {
                    let mut v = &all;
                    for part in key.split('.') {
                        v = v
                            .get(part)
                            .ok_or_else(|| anyhow!("unknown setting `{key}`"))?;
                    }
                    v.clone()
                }
            };
            Ok(
                Outcome::new(str_arg(args, "key").unwrap_or("settings").to_string())
                    .with_data(json!({ "value": value })),
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        assert_eq!(
            parse_location("src/a.rs:12:4"),
            ("src/a.rs".into(), Some(12), Some(4))
        );
        assert_eq!(
            parse_location("src/a.rs:12"),
            ("src/a.rs".into(), Some(12), None)
        );
        assert_eq!(parse_location("src/a.rs"), ("src/a.rs".into(), None, None));
        assert_eq!(
            parse_location("C:/x/a.rs:3"),
            ("C:/x/a.rs".into(), Some(3), None)
        );
        assert_eq!(parse_location("a.rs:x"), ("a.rs:x".into(), None, None));
    }
}
