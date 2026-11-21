//! Who may run each command (ADR 0010, item 3), over the registry exactly as `lector run` and
//! `lector mcp` assemble it (built-in commands plus the agent commands).
//!
//! * The classification table below is exhaustive: a new command fails
//!   `every_command_is_classified` until it is added here with what it touches, so a network
//!   command cannot be `All` by omission and every change of reach is reviewed.
//! * A command whose spec says network or credentials is never `All`.
//! * MCP with `--allow-writes` can neither list nor call the network commands, the agent
//!   steering commands or `agent.answer_permission`; the CLI runs the interactive ones.

use std::io::Cursor;
use std::process::Command;

use lector_app::commands::{CommandError, Source, Sources};
use lector_app::{AppCore, CommandContext, CoreOptions, Project};
use lector_mcp::{McpServer, ServerOptions};
use serde_json::{json, Value};

/// (id, writes, network, credentials, sources)
const TABLE: &[(&str, bool, bool, bool, Sources)] = {
    use Sources::{All, Interactive, UiOnly};
    &[
        ("agent.answer_permission", true, false, false, UiOnly),
        ("agent.cancel", false, false, false, Interactive),
        ("agent.list_configs", false, false, false, All),
        ("agent.prompt", true, false, false, Interactive),
        ("agent.send_comments", true, false, false, Interactive),
        ("agent.start", true, false, false, Interactive),
        ("agent.turns", false, false, false, All),
        ("agent.undo_turn", true, false, false, All),
        ("commands.list", false, false, false, All),
        ("comments.add", false, false, false, All),
        ("comments.delete", false, false, false, All),
        ("comments.export", false, false, false, All),
        ("comments.list", false, false, false, All),
        ("comments.reply", false, false, false, All),
        ("comments.resolve", false, false, false, All),
        ("document.apply_edits", true, false, false, All),
        ("document.close", false, false, false, All),
        ("document.list", false, false, false, All),
        ("document.open", false, false, false, All),
        ("document.read", false, false, false, All),
        ("document.revert", true, false, false, All),
        ("document.save", true, false, false, All),
        ("document.save_all", true, false, false, All),
        ("document.set_read_only", false, false, false, All),
        ("git.abort_operation", true, false, false, All),
        ("git.accept_file", true, false, false, All),
        ("git.blame", false, false, false, All),
        ("git.commit", true, false, false, All),
        ("git.continue_operation", true, false, false, All),
        ("git.create_branch", true, false, false, All),
        ("git.delete_branch", true, false, false, All),
        ("git.discard", true, false, false, All),
        ("git.discard_all", true, false, false, All),
        ("git.discard_hunk", true, false, false, All),
        ("git.fetch", true, true, true, Interactive),
        ("git.history", false, false, false, All),
        ("git.list_changes", false, false, false, All),
        ("git.list_remotes", false, false, false, All),
        ("git.mark_resolved", true, false, false, All),
        ("git.open_changeset", false, false, false, All),
        ("git.pull", true, true, true, Interactive),
        ("git.push", true, true, true, Interactive),
        ("git.rename_branch", true, false, false, All),
        ("git.resolve_delete", true, false, false, All),
        ("git.restore_ref", true, false, false, All),
        ("git.set_upstream", true, false, false, All),
        ("git.show_diff", false, false, false, All),
        ("git.stage", true, false, false, All),
        ("git.stage_all", true, false, false, All),
        ("git.stage_hunk", true, false, false, All),
        ("git.stash_apply", true, false, false, All),
        ("git.stash_drop", true, false, false, All),
        ("git.stash_list", false, false, false, All),
        ("git.stash_pop", true, false, false, All),
        ("git.stash_push", true, false, false, All),
        ("git.stash_store", true, false, false, All),
        ("git.switch_branch", true, false, false, All),
        ("git.unstage", true, false, false, All),
        ("git.unstage_all", true, false, false, All),
        ("git.unstage_hunk", true, false, false, All),
        ("git.upstream_status", false, false, false, All),
        ("review.fetch_pull_request", false, true, true, UiOnly),
        ("search.project", false, false, false, All),
        ("settings.get", false, false, false, All),
        ("symbols.find", false, false, false, All),
        ("workspace.open_location", false, false, false, All),
        ("workspace.open_path", false, false, false, All),
    ]
};

/// Commands MCP may never reach, with or without `--allow-writes`.
const NEVER_MCP: &[&str] = &[
    "git.fetch",
    "git.pull",
    "git.push",
    "agent.start",
    "agent.prompt",
    "agent.cancel",
    "agent.send_comments",
    "agent.answer_permission",
    "review.fetch_pull_request",
];

fn core() -> AppCore {
    let core = AppCore::open(CoreOptions::for_tests());
    lector_agent::commands::register(&core);
    core
}

#[test]
fn every_command_is_classified() {
    let core = core();
    let mut actual: Vec<(String, bool, bool, bool, Sources)> = core
        .registry()
        .list()
        .into_iter()
        .map(|c| {
            let s = c.spec();
            (
                s.id.to_string(),
                s.writes,
                s.touches.network,
                s.touches.credentials,
                s.allowed_sources(),
            )
        })
        .collect();
    actual.sort_by(|a, b| a.0.cmp(&b.0));
    let expected: Vec<(String, bool, bool, bool, Sources)> = TABLE
        .iter()
        .map(|(id, w, n, c, s)| (id.to_string(), *w, *n, *c, *s))
        .collect();
    let changed: Vec<String> = actual
        .iter()
        .zip(expected.iter())
        .filter(|(a, e)| a != e)
        .map(|(a, e)| format!("{a:?} (table says {e:?})"))
        .collect();
    assert!(
        changed.is_empty(),
        "classification changed:\n{}",
        changed.join("\n")
    );
    assert_eq!(
        actual.iter().map(|a| a.0.as_str()).collect::<Vec<_>>(),
        expected.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(),
        "a command was added or removed: classify it in TABLE"
    );
}

/// ADR 0010: a command whose spec says network or credentials is never `All`, and the
/// `sources` it declares cannot widen what it touches.
#[test]
fn network_and_credentials_are_never_all() {
    let core = core();
    for c in core.registry().list() {
        let s = c.spec();
        if s.touches.network || s.touches.credentials || s.touches.steers_agent {
            assert_ne!(s.allowed_sources(), Sources::All, "`{}`", s.id);
            for source in [Source::Mcp, Source::Agent] {
                assert!(!s.allowed_sources().allows(source), "`{}` {source:?}", s.id);
            }
        }
        if s.touches.answers_for_user {
            assert_eq!(s.allowed_sources(), Sources::UiOnly, "`{}`", s.id);
        }
    }
}

fn mcp(core: &AppCore, project: &Project, messages: &[Value]) -> Vec<Value> {
    let server = McpServer::new(
        core.clone(),
        Some(project.clone()),
        ServerOptions {
            allow_writes: true,
            ..Default::default()
        },
    );
    let mut input = String::new();
    for m in messages {
        input.push_str(&m.to_string());
        input.push('\n');
    }
    let mut output = Vec::new();
    server
        .serve(Cursor::new(input.into_bytes()), &mut output)
        .expect("serve");
    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|l| serde_json::from_str(l).expect("json"))
        .collect()
}

fn project() -> (tempfile::TempDir, Project) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = dunce::canonicalize(tmp.path()).expect("canonical");
    let git = |dir: &std::path::Path, args: &[&str]| {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git");
        assert!(ok.success(), "git {args:?}");
    };
    let remote = root.join("remote.git");
    let root = root.join("work");
    std::fs::create_dir_all(&root).expect("mkdir");
    git(&root, &["init", "-q", "--bare", &remote.to_string_lossy()]);
    git(&root, &["init", "-q"]);
    git(
        &root,
        &["remote", "add", "origin", &remote.to_string_lossy()],
    );
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&root, settings).expect("project");
    (tmp, project)
}

/// MCP with `--allow-writes` neither lists nor runs the network, agent-steering and
/// answer-for-the-user commands.
#[test]
fn mcp_with_allow_writes_cannot_reach_interactive_commands() {
    let core = core();
    let (_tmp, project) = project();
    let mut messages = vec![
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
            "protocolVersion": "2025-06-18", "capabilities": {},
            "clientInfo": {"name": "t", "version": "0"}}}),
        json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}}),
    ];
    for (i, id) in NEVER_MCP.iter().enumerate() {
        messages.push(
            json!({"jsonrpc": "2.0", "id": 10 + i, "method": "tools/call",
            "params": {"name": id.replace('.', "_"), "arguments": {}}}),
        );
    }
    let out = mcp(&core, &project, &messages);
    let by_id = |id: u64| {
        out.iter()
            .find(|m| m["id"] == json!(id))
            .unwrap_or_else(|| panic!("no response {id}: {out:?}"))
    };
    let names: Vec<String> = by_id(2)["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect();
    assert!(names.contains(&"git_stage".to_string()), "{names:?}");
    assert!(names.contains(&"agent_undo_turn".to_string()), "{names:?}");
    for (i, id) in NEVER_MCP.iter().enumerate() {
        let tool = id.replace('.', "_");
        assert!(!names.contains(&tool), "{tool} listed");
        let r = by_id(10 + i as u64);
        let refused = r["result"]["isError"] == json!(true) || r.get("error").is_some();
        assert!(refused, "{tool} ran: {r}");
    }
}

/// The CLI is interactive: `lector run git.fetch` is allowed (a person or their script started
/// it); `agent.answer_permission` is refused (only the window answers for the user).
#[test]
fn the_cli_runs_interactive_commands_but_never_answers_for_the_user() {
    let core = core();
    let (_tmp, project) = project();
    let cx = CommandContext::new(core.clone(), Some(project), Source::Cli);
    let listed: Vec<&str> = core
        .registry()
        .list_for(Source::Cli)
        .iter()
        .map(|c| c.spec().id)
        .collect();
    assert!(listed.contains(&"git.fetch"));
    assert!(listed.contains(&"agent.prompt"));
    assert!(!listed.contains(&"agent.answer_permission"));
    // A local bare remote: the fetch runs (and has nothing to do).
    let fetched = core.registry().invoke("git.fetch", json!({}), &cx);
    assert!(fetched.is_ok(), "{fetched:?}");
    match core.registry().invoke(
        "agent.answer_permission",
        json!({"id": 1, "choice": "allow_once"}),
        &cx,
    ) {
        Err(CommandError::Unavailable(m)) => assert!(m.contains("only from Lector's window")),
        other => panic!("expected a refusal, got {other:?}"),
    }
    let mcp_cx = CommandContext::new(core.clone(), cx.project.clone(), Source::Mcp);
    match core.registry().invoke("git.fetch", json!({}), &mcp_cx) {
        Err(CommandError::Unavailable(m)) => assert!(m.contains("not from MCP clients")),
        other => panic!("expected a refusal, got {other:?}"),
    }
}
