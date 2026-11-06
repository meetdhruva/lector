//! Agent commands in the [`lector_app::CommandRegistry`] (so palette, CLI and MCP get them):
//! `agent.list_configs`, `agent.start`, `agent.prompt`, `agent.cancel`, `agent.turns`,
//! `agent.undo_turn`, `agent.answer_permission`, `agent.send_comments`.
//!
//! Commands act on the project's current session ([`AgentManager::session_for`]).
//! `agent.start`, `agent.prompt`, `agent.undo_turn`, `agent.answer_permission` and
//! `agent.send_comments` are `writes = true` (they lead to file writes; MCP needs
//! `--allow-writes`). The agent's own writes and every turn are journaled by the session
//! (`agent.write_file`, `agent.turn`, undoable).

use std::time::Duration;

use anyhow::{anyhow, bail, Context as _};
use lector_app::commands::schema::object;
use lector_app::commands::{
    Availability, CommandContext, CommandRegistry, CommandSpec, FnCommand, Outcome, UiRequest,
};
use lector_app::comments::{CommentStore, ThreadFilter, ThreadState};
use lector_app::AppCore;
use serde_json::{json, Value};

use crate::config::{available_configs, AgentConfig};
use crate::manager::AgentManager;
use crate::model::{PermissionChoice, PermissionRequestId, PromptContext, Turn, TurnId};
use crate::session::{AgentSession, SessionStatus};
use crate::AgentError;

/// Registers the agent commands (call once per core; the GUI and the CLI do).
pub fn register(core: &AppCore) {
    let registry = core.registry();
    if registry.get("agent.prompt").is_some() {
        return;
    }
    register_into(registry);
    // Create the manager now so the provenance provider is in place.
    let _ = AgentManager::of(core);
}

fn session_of(cx: &CommandContext) -> anyhow::Result<AgentSession> {
    let project = cx.require_project()?;
    AgentManager::of(&cx.core)
        .session_for(project.root())
        .ok_or_else(|| anyhow!("no agent session; start one with agent.start"))
}

fn has_session(cx: &CommandContext) -> Availability {
    match &cx.project {
        None => Availability::no("No folder is open."),
        Some(p) => match AgentManager::of(&cx.core).session_for(p.root()) {
            Some(_) => Availability::Available,
            None => Availability::no("No agent session. Start one first."),
        },
    }
}

fn has_project(cx: &CommandContext) -> Availability {
    if cx.project.is_some() {
        Availability::Available
    } else {
        Availability::no("No folder is open.")
    }
}

/// The agent configurations offered (user settings + agents found on `PATH`).
pub fn configs() -> Vec<AgentConfig> {
    available_configs(&lector_config::load_settings().0.agent)
}

/// Compact JSON of a turn.
pub fn turn_json(t: &Turn) -> Value {
    json!({
        "turn": t.id.0,
        "prompt": t.prompt,
        "state": t.state,
        "changed_files": t.changed_files,
        "changeset": t.changeset().map(|c| lector_workspace::review::changeset_key(&c)),
        "undone": t.undone,
        "answer": t.answer(),
        "tool_calls": t.tool_calls.iter().map(|c| json!({
            "title": c.title, "kind": c.kind, "status": c.status,
        })).collect::<Vec<_>>(),
        "plan": t.plan,
        "pending_permissions": t.permissions.iter().filter(|p| p.pending).map(|p| json!({
            "id": p.id.0, "title": p.title, "kind": p.kind,
        })).collect::<Vec<_>>(),
    })
}

fn status_json(s: &AgentSession) -> Value {
    json!({ "session": s.id().0, "agent": s.agent_name(), "status": s.status() })
}

fn opt_turn(args: &Value, session: &AgentSession) -> anyhow::Result<TurnId> {
    match args.get("turn").and_then(Value::as_u64) {
        Some(n) => Ok(TurnId(n)),
        None => session
            .last_turn()
            .map(|t| t.id)
            .ok_or_else(|| anyhow!("the session has no turns")),
    }
}

/// Sends a prompt; with `wait`, blocks until the turn ends (or the timeout).
fn prompt_and_maybe_wait(
    session: &AgentSession,
    text: String,
    context: Vec<PromptContext>,
    args: &Value,
) -> anyhow::Result<Outcome> {
    let id = session.prompt(text, context)?;
    let wait = args.get("wait").and_then(Value::as_bool).unwrap_or(false);
    if !wait {
        return Ok(
            Outcome::new(format!("Prompted {} (turn {id})", session.config().name))
                .with_data(json!({ "turn": id.0, "state": "running" })),
        );
    }
    let timeout = args.get("timeout_s").and_then(Value::as_u64).unwrap_or(600);
    let turn = session
        .wait_turn(id, Duration::from_secs(timeout))
        .ok_or_else(|| anyhow!("turn {id} still running after {timeout} s"))?;
    let mut out = Outcome::new(format!(
        "{} turn {id} {} ({} files changed)",
        session.config().name,
        turn.state.label(),
        turn.changed_files.len()
    ))
    .with_data(turn_json(&turn));
    if let Some(cs) = turn.changeset() {
        if !turn.changed_files.is_empty() {
            out = out.with_ui(UiRequest::OpenChangeset(cs));
        }
    }
    Ok(out)
}

/// The feedback prompt for review comments of `changeset_key` plus `extra` text.
pub fn feedback_text(cx: &CommandContext, changeset_key: Option<&str>, extra: &str) -> String {
    let mut parts = Vec::new();
    if let (Some(repo), Some(key)) = (cx.project.as_ref().and_then(|p| p.repo()), changeset_key) {
        let store = CommentStore::for_core(&cx.core);
        let filter = ThreadFilter {
            states: vec![ThreadState::Open],
            ..ThreadFilter::changeset(key)
        };
        if let Ok(threads) = store.threads(repo.workdir(), &filter) {
            parts.extend(threads.iter().map(|t| t.to_prompt()));
        }
    }
    if !extra.trim().is_empty() {
        parts.push(extra.trim().to_string());
    }
    parts.join("\n")
}

fn register_into(r: &CommandRegistry) {
    r.register(FnCommand::new(
        CommandSpec::read(
            "agent.list_configs",
            "Agent: List Agents",
            "Agent",
            object(&[], &[]),
        ),
        |_, _| {
            let list = configs();
            Ok(
                Outcome::new(format!("{} agents", list.len())).with_data(json!(list
                    .iter()
                    .map(|c| json!({ "name": c.name, "command": c.command_line() }))
                    .collect::<Vec<_>>())),
            )
        },
    ));
    r.register(
        FnCommand::new(
            CommandSpec::write(
                "agent.start",
                "Agent: New Session",
                "Agent",
                object(
                    &[(
                        "name",
                        json!({"type": "string", "description": "Agent name from agent.list_configs (default: the first)"}),
                    )],
                    &[],
                ),
            ),
            |cx, args| {
                let project = cx.require_project()?;
                let list = configs();
                let cfg = match args.get("name").and_then(Value::as_str) {
                    Some(name) => list
                        .iter()
                        .find(|c| c.name.eq_ignore_ascii_case(name))
                        .ok_or_else(|| anyhow!("no agent named {name:?}"))?,
                    None => list.first().ok_or_else(|| {
                        anyhow!("no agent configured: add [[agent.servers]] to settings.toml or install an ACP agent")
                    })?,
                };
                let session = AgentManager::of(&cx.core).start(&cx.core, project, cfg)?;
                Ok(Outcome::new(format!("Started {}", cfg.name)).with_data(status_json(&session)))
            },
        )
        .with_available(has_project),
    );
    r.register(
        FnCommand::new(
            CommandSpec::write(
                "agent.prompt",
                "Agent: Prompt",
                "Agent",
                object(
                    &[
                        ("text", json!({"type": "string"})),
                        (
                            "files",
                            json!({"type": "array", "items": {"type": "string"}, "description": "Files to attach (project-relative)"}),
                        ),
                        ("wait", json!({"type": "boolean", "description": "Block until the turn ends"})),
                        ("timeout_s", json!({"type": "integer", "minimum": 1})),
                    ],
                    &["text"],
                ),
            ),
            |cx, args| {
                let session = session_of(cx)?;
                let text = args["text"].as_str().unwrap_or_default().to_string();
                if text.trim().is_empty() {
                    bail!("empty prompt");
                }
                let context = args
                    .get("files")
                    .and_then(Value::as_array)
                    .map(|f| {
                        f.iter()
                            .filter_map(Value::as_str)
                            .map(|p| PromptContext::File { path: p.to_string() })
                            .collect()
                    })
                    .unwrap_or_default();
                prompt_and_maybe_wait(&session, text, context, args)
            },
        )
        .with_available(has_session),
    );
    r.register(
        FnCommand::new(
            CommandSpec::read(
                "agent.cancel",
                "Agent: Cancel Turn",
                "Agent",
                object(&[], &[]),
            )
            .with_ui_action("agent::Cancel"),
            |cx, _| {
                let session = session_of(cx)?;
                session.cancel();
                Ok(Outcome::new("Cancelling the agent turn"))
            },
        )
        .with_available(|cx| match has_session(cx) {
            Availability::Available => {
                let busy = cx
                    .project
                    .as_ref()
                    .and_then(|p| AgentManager::of(&cx.core).session_for(p.root()))
                    .is_some_and(|s| matches!(s.status(), SessionStatus::Busy(_)));
                if busy {
                    Availability::Available
                } else {
                    Availability::no("The agent is not working on a turn.")
                }
            }
            other => other,
        }),
    );
    r.register(
        FnCommand::new(
            CommandSpec::read(
                "agent.turns",
                "Agent: List Turns",
                "Agent",
                object(
                    &[
                        ("limit", json!({"type": "integer", "minimum": 1})),
                        ("turn", json!({"type": "integer", "minimum": 1})),
                    ],
                    &[],
                ),
            ),
            |cx, args| {
                let session = session_of(cx)?;
                let mut turns = session.turns();
                if let Some(n) = args.get("turn").and_then(Value::as_u64) {
                    turns.retain(|t| t.id.0 == n);
                }
                let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
                let skip = turns.len().saturating_sub(limit);
                let list: Vec<Value> = turns[skip..].iter().map(turn_json).collect();
                Ok(
                    Outcome::new(format!("{} turns", list.len())).with_data(json!({
                        "session": status_json(&session),
                        "turns": list,
                    })),
                )
            },
        )
        .with_available(has_session),
    );
    r.register(
        FnCommand::new(
            CommandSpec::write(
                "agent.undo_turn",
                "Agent: Undo Turn",
                "Agent",
                object(
                    &[
                        ("turn", json!({"type": "integer", "minimum": 1, "description": "Default: the last turn"})),
                        ("force", json!({"type": "boolean", "description": "Overwrite files edited since the turn"})),
                    ],
                    &[],
                ),
            )
            .destructive(),
            |cx, args| {
                let session = session_of(cx)?;
                let id = opt_turn(args, &session)?;
                let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
                match session.undo_turn(id, force, cx.source) {
                    Ok(entry) => Ok(Outcome::new(format!("Undid agent turn {id}"))
                        .with_data(json!({ "turn": id.0, "journal_id": entry }))
                        .with_ui(UiRequest::Refresh)),
                    Err(AgentError::UndoConflict { paths }) => Err(anyhow!(
                        "Files changed since turn {id}: {}. Undo with force to overwrite them.",
                        paths.join(", ")
                    )),
                    Err(e) => Err(e.into()),
                }
            },
        )
        .with_available(has_session),
    );
    r.register(
        FnCommand::new(
            CommandSpec::write(
                "agent.answer_permission",
                "Agent: Answer Permission Request",
                "Agent",
                object(
                    &[
                        ("id", json!({"type": "integer", "minimum": 1})),
                        ("choice", json!({"type": "string", "enum": ["allow_once", "allow_always", "reject"]})),
                    ],
                    &["id", "choice"],
                ),
            ),
            |cx, args| {
                let session = session_of(cx)?;
                let id = PermissionRequestId(args["id"].as_u64().unwrap_or(0));
                let choice: PermissionChoice =
                    serde_json::from_value(args["choice"].clone()).context("choice")?;
                if !session.pending_permissions().iter().any(|p| p.id == id) {
                    bail!("no pending permission request {}", id.0);
                }
                session.answer_permission(id, choice);
                Ok(Outcome::new("Answered the agent"))
            },
        )
        .with_available(has_session),
    );
    r.register(
        FnCommand::new(
            CommandSpec::write(
                "agent.send_comments",
                "Agent: Send Review Feedback",
                "Agent",
                object(
                    &[
                        ("text", json!({"type": "string", "description": "Typed feedback (selected lines, instructions)"})),
                        ("changeset", json!({"type": "string", "description": "Review key whose open comment threads are sent (default: the focused changeset, else the last turn)"})),
                        ("wait", json!({"type": "boolean"})),
                        ("timeout_s", json!({"type": "integer", "minimum": 1})),
                    ],
                    &[],
                ),
            ),
            |cx, args| {
                let session = session_of(cx)?;
                let key = args
                    .get("changeset")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| {
                        cx.focus
                            .changeset
                            .as_ref()
                            .map(lector_workspace::review::changeset_key)
                    })
                    .or_else(|| {
                        session
                            .last_turn()
                            .and_then(|t| t.changeset())
                            .map(|c| lector_workspace::review::changeset_key(&c))
                    });
                let extra = args.get("text").and_then(Value::as_str).unwrap_or("");
                let feedback = feedback_text(cx, key.as_deref(), extra);
                if feedback.trim().is_empty() {
                    bail!("no open review comments and no feedback text to send");
                }
                let text = "Please address this review feedback on your changes:".to_string();
                prompt_and_maybe_wait(
                    &session,
                    text,
                    vec![PromptContext::Comments { text: feedback }],
                    args,
                )
            },
        )
        .with_available(has_session),
    );
}
