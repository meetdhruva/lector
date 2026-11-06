//! End-to-end MCP protocol tests: the server loop runs over in-memory pipes (a byte
//! buffer in, a byte buffer out) against a real git fixture repository.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use lector_app::commands::{CommandSpec, FnCommand, Outcome};
use lector_app::journal::Journal;
use lector_app::{AppCore, CoreOptions};
use lector_mcp::{McpServer, ServerOptions, LATEST_PROTOCOL_VERSION};
use lector_workspace::Project;
use serde_json::{json, Value};
use tempfile::TempDir;

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

struct Fixture {
    _tmp: TempDir,
    repo: PathBuf,
    core: AppCore,
    project: Project,
}

/// A repository with one commit, one modified file and one untracked file.
fn fixture() -> Fixture {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = dunce::canonicalize(tmp.path())
        .expect("canonical")
        .join("repo");
    std::fs::create_dir_all(repo.join("src")).expect("mkdir");
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "gc.auto", "0"]);
    git(&repo, &["config", "maintenance.auto", "false"]);
    std::fs::write(repo.join("src/lib.rs"), "pub fn a() {}\npub fn b() {}\n").expect("write");
    std::fs::write(repo.join("README.md"), "# demo\n").expect("write");
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "-q", "-m", "init"]);
    std::fs::write(
        repo.join("src/lib.rs"),
        "pub fn a(x: u8) {}\npub fn b() {}\n",
    )
    .expect("write");
    std::fs::write(repo.join("notes.txt"), "todo\n").expect("write");
    let core = AppCore::open(CoreOptions {
        in_memory_db: true,
        workers: Some(2),
        data_dir: Some(tmp.path().join("data")),
        ..Default::default()
    });
    let mut settings = lector_config::Settings::default();
    settings.lsp.enabled = false;
    let project = Project::open(&repo, settings).expect("project");
    Fixture {
        _tmp: tmp,
        repo,
        core,
        project,
    }
}

impl Fixture {
    fn server(&self, allow_writes: bool) -> McpServer {
        McpServer::new(
            self.core.clone(),
            Some(self.project.clone()),
            ServerOptions {
                allow_writes,
                ..Default::default()
            },
        )
    }
}

/// Feeds `messages` (one JSON document per line) to the server and returns every output line.
fn converse(server: &McpServer, messages: &[Value]) -> Vec<Value> {
    let mut input = String::new();
    for m in messages {
        input.push_str(&serde_json::to_string(m).expect("json"));
        input.push('\n');
    }
    converse_raw(server, &input)
}

fn converse_raw(server: &McpServer, input: &str) -> Vec<Value> {
    let mut output: Vec<u8> = Vec::new();
    server
        .serve(Cursor::new(input.as_bytes().to_vec()), &mut output)
        .expect("serve");
    String::from_utf8(output)
        .expect("utf-8")
        .lines()
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("not JSON ({e}): {l}")))
        .collect()
}

fn req(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

fn call(id: u64, tool: &str, args: Value) -> Value {
    req(id, "tools/call", json!({ "name": tool, "arguments": args }))
}

fn handshake() -> Vec<Value> {
    vec![
        req(
            1,
            "initialize",
            json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": { "name": "test", "version": "0" },
            }),
        ),
        json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
    ]
}

fn response(out: &[Value], id: u64) -> &Value {
    out.iter()
        .find(|m| m["id"] == json!(id))
        .unwrap_or_else(|| panic!("no response for id {id} in {out:?}"))
}

fn tool_names(list: &Value) -> Vec<String> {
    list["result"]["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|t| t["name"].as_str().expect("name").to_string())
        .collect()
}

#[test]
fn handshake_negotiates_the_protocol_version() {
    let fx = fixture();
    let server = fx.server(false);
    let out = converse(&server, &handshake());
    assert_eq!(
        out.len(),
        1,
        "the initialized notification has no response: {out:?}"
    );
    let init = &out[0]["result"];
    assert_eq!(init["protocolVersion"], "2025-06-18");
    assert_eq!(init["serverInfo"]["name"], "lector");
    assert!(init["capabilities"]["tools"].is_object());
    assert!(init["capabilities"]["resources"].is_object());
    assert!(init["instructions"]
        .as_str()
        .expect("instructions")
        .contains("read-only"));

    for (asked, expected) in [
        ("2024-11-05", "2024-11-05"),
        (LATEST_PROTOCOL_VERSION, LATEST_PROTOCOL_VERSION),
        ("1999-01-01", LATEST_PROTOCOL_VERSION),
    ] {
        let out = converse(
            &server,
            &[req(7, "initialize", json!({ "protocolVersion": asked }))],
        );
        assert_eq!(
            out[0]["result"]["protocolVersion"], expected,
            "asked {asked}"
        );
    }

    let out = converse(&server, &[req(2, "ping", json!({}))]);
    assert_eq!(out[0]["result"], json!({}));
    assert_eq!(out[0]["id"], 2);
    assert_eq!(out[0]["jsonrpc"], "2.0");
}

#[test]
fn json_rpc_errors() {
    let fx = fixture();
    let server = fx.server(false);
    let out = converse_raw(
        &server,
        "this is not json\n{\"jsonrpc\":\"2.0\",\"id\":5,\"method\":\"nope\"}\n[]\n42\n\n{\"jsonrpc\":\"2.0\",\"id\":6}\n",
    );
    assert_eq!(out[0]["error"]["code"], -32700);
    assert_eq!(out[0]["id"], Value::Null);
    assert_eq!(out[1]["error"]["code"], -32601);
    assert_eq!(out[1]["id"], 5);
    assert_eq!(out[2]["error"]["code"], -32600, "empty batch");
    assert_eq!(out[3]["error"]["code"], -32600, "not an object");
    assert_eq!(out[4]["error"]["code"], -32600, "no method");
    assert_eq!(out[4]["id"], 6);
}

#[test]
fn tools_list_is_the_registry_and_hides_writes_by_default() {
    let fx = fixture();
    let out = converse(&fx.server(false), &[req(1, "tools/list", json!({}))]);
    let names = tool_names(&out[0]);
    for expected in [
        "git_list_changes",
        "git_show_diff",
        "git_blame",
        "document_read",
        "search_project",
        "commands_list",
    ] {
        assert!(
            names.contains(&expected.to_string()),
            "{expected} in {names:?}"
        );
    }
    for hidden in ["git_stage", "git_commit", "git_discard", "document_save"] {
        assert!(
            !names.contains(&hidden.to_string()),
            "{hidden} must be hidden: {names:?}"
        );
    }
    let tools = out[0]["result"]["tools"].as_array().expect("tools");
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert!(
            t["description"].as_str().is_some_and(|d| !d.is_empty()),
            "{t}"
        );
        assert_eq!(t["annotations"]["readOnlyHint"], true, "{t}");
        assert!(
            t["name"]
                .as_str()
                .expect("name")
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "{t}"
        );
    }
    let list_changes = tools
        .iter()
        .find(|t| t["name"] == "git_list_changes")
        .expect("tool");
    assert!(list_changes["inputSchema"]["properties"]["changeset"].is_object());
}

#[test]
fn tools_list_with_writes_annotates_destructive_tools() {
    let fx = fixture();
    let out = converse(&fx.server(true), &[req(1, "tools/list", json!({}))]);
    let tools = out[0]["result"]["tools"].as_array().expect("tools");
    let by_name = |n: &str| {
        tools
            .iter()
            .find(|t| t["name"] == n)
            .unwrap_or_else(|| panic!("{n}"))
    };
    let stage = by_name("git_stage");
    assert_eq!(stage["annotations"]["readOnlyHint"], false);
    assert_eq!(stage["annotations"]["destructiveHint"], false);
    assert_eq!(
        by_name("git_discard")["annotations"]["destructiveHint"],
        true
    );
    assert_eq!(
        by_name("git_list_changes")["annotations"]["readOnlyHint"],
        true
    );
}

#[test]
fn tools_list_is_dynamic() {
    let fx = fixture();
    let server = fx.server(false);
    let before = tool_names(&converse(&server, &[req(1, "tools/list", json!({}))])[0]);
    assert!(!before.contains(&"test_late_tool".to_string()));
    fx.core.registry().register(FnCommand::new(
        CommandSpec::read("test.late_tool", "Late", "Test", json!({"type": "object"})),
        |_, _| Ok(Outcome::new("late")),
    ));
    let after = tool_names(&converse(&server, &[req(1, "tools/list", json!({}))])[0]);
    assert!(after.contains(&"test_late_tool".to_string()));
}

#[test]
fn read_tools_return_structured_content_and_text() {
    let fx = fixture();
    let server = fx.server(false);
    let out = converse(
        &server,
        &[
            call(2, "git_list_changes", json!({})),
            call(
                3,
                "git_show_diff",
                json!({ "changeset": "worktree", "path": "src/lib.rs" }),
            ),
            call(4, "document_read", json!({ "path": "src/lib.rs" })),
            call(5, "git_list_changes", json!({ "changeset": 12 })),
            call(6, "git_list_changes", json!({ "changeset": "commit:zzzz" })),
        ],
    );
    let list = &response(&out, 2)["result"];
    assert_eq!(list["isError"], false);
    let files = list["structuredContent"]["files"]
        .as_array()
        .expect("files");
    let paths: Vec<&str> = files
        .iter()
        .map(|f| f["path"].as_str().expect("path"))
        .collect();
    assert!(
        paths.contains(&"src/lib.rs") && paths.contains(&"notes.txt"),
        "{paths:?}"
    );
    let text = list["content"][0]["text"].as_str().expect("text");
    assert!(text.starts_with("Working tree"), "summary first: {text}");
    assert!(
        text.contains("src/lib.rs"),
        "data follows the summary for clients without structuredContent"
    );

    let diff = &response(&out, 3)["result"];
    assert_eq!(
        diff["structuredContent"]["files"][0]["hunks"][0]["lines"][0],
        "-pub fn a() {}"
    );

    let read = &response(&out, 4)["result"];
    assert_eq!(
        read["structuredContent"]["text"],
        "pub fn a(x: u8) {}\npub fn b() {}\n"
    );

    // Argument problems are tool errors (the model can read and fix them), not protocol errors.
    let bad = response(&out, 5);
    assert!(bad.get("error").is_none(), "{bad}");
    assert_eq!(bad["result"]["isError"], true);
    assert!(bad["result"]["content"][0]["text"]
        .as_str()
        .expect("text")
        .contains("changeset"));
    let unresolvable = response(&out, 6);
    assert_eq!(unresolvable["result"]["isError"], true);
}

#[test]
fn unknown_tools_and_bad_params_are_protocol_errors() {
    let fx = fixture();
    let out = converse(
        &fx.server(true),
        &[
            call(1, "no_such_tool", json!({})),
            req(2, "tools/call", json!({})),
            req(
                3,
                "tools/call",
                json!({ "name": "git_list_changes", "arguments": [1] }),
            ),
        ],
    );
    for id in 1..=3 {
        assert_eq!(response(&out, id)["error"]["code"], -32602, "id {id}");
    }
}

#[test]
fn writes_are_rejected_without_allow_writes_and_nothing_is_journaled() {
    let fx = fixture();
    let server = fx.server(false);
    let out = converse(
        &server,
        &[call(1, "git_stage", json!({ "paths": ["src/lib.rs"] }))],
    );
    let result = &response(&out, 1)["result"];
    assert_eq!(result["isError"], true, "{result}");
    let text = result["content"][0]["text"].as_str().expect("text");
    assert!(text.contains("--allow-writes"), "{text}");
    assert_eq!(
        git(&fx.repo, &["diff", "--cached", "--name-only"]),
        "",
        "nothing was staged"
    );
    let journal = Journal::of(&fx.core).expect("journal");
    assert!(journal.list(10).is_empty(), "nothing was journaled");
}

#[test]
fn writes_with_allow_writes_are_journaled_with_origin_mcp() {
    let fx = fixture();
    let server = fx.server(true);
    let out = converse(
        &server,
        &[call(1, "git_stage", json!({ "paths": ["src/lib.rs"] }))],
    );
    let result = &response(&out, 1)["result"];
    assert_eq!(result["isError"], false, "{result}");
    assert_eq!(
        git(&fx.repo, &["diff", "--cached", "--name-only"]).trim(),
        "src/lib.rs"
    );

    let journal = Journal::of(&fx.core).expect("journal");
    let entries = journal.list(10);
    assert_eq!(entries.len(), 1, "{entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.command, "git.stage");
    assert_eq!(
        serde_json::to_value(entry.source).expect("source"),
        json!("mcp")
    );
    assert_eq!(result["_meta"]["lector"]["journalId"], json!(entry.id));
    assert_eq!(result["_meta"]["lector"]["undoable"], true);

    // Reads in the same session are not journaled.
    converse(&server, &[call(2, "git_list_changes", json!({}))]);
    assert_eq!(journal.list(10).len(), 1);
}

#[test]
fn invalid_arguments_of_a_write_tool_change_nothing() {
    let fx = fixture();
    let server = fx.server(true);
    let out = converse(
        &server,
        &[call(1, "git_stage", json!({ "paths": "src/lib.rs" }))],
    );
    assert_eq!(response(&out, 1)["result"]["isError"], true);
    assert_eq!(git(&fx.repo, &["diff", "--cached", "--name-only"]), "");
    assert!(Journal::of(&fx.core).expect("journal").list(10).is_empty());
}

#[test]
fn resources_list_read_and_templates() {
    let fx = fixture();
    let server = fx.server(false);
    // Open a document through the tool so it shows up as a resource.
    let opened = converse(
        &server,
        &[call(1, "document_open", json!({ "path": "src/lib.rs" }))],
    );
    assert_eq!(response(&opened, 1)["result"]["isError"], false);
    let out = converse(
        &server,
        &[
            req(2, "resources/list", json!({})),
            req(
                3,
                "resources/read",
                json!({ "uri": "lector://document/src/lib.rs" }),
            ),
            req(
                4,
                "resources/read",
                json!({ "uri": "lector://changeset/worktree" }),
            ),
            req(5, "resources/templates/list", json!({})),
            req(
                6,
                "resources/read",
                json!({ "uri": "lector://document/../outside.txt" }),
            ),
            req(
                7,
                "resources/read",
                json!({ "uri": "https://example.com/x" }),
            ),
            req(
                8,
                "resources/read",
                json!({ "uri": "lector://document/does-not-exist.rs" }),
            ),
            req(9, "resources/read", json!({})),
        ],
    );
    let list = response(&out, 2)["result"]["resources"]
        .as_array()
        .expect("resources")
        .clone();
    let uris: Vec<&str> = list
        .iter()
        .map(|r| r["uri"].as_str().expect("uri"))
        .collect();
    assert!(uris.contains(&"lector://changeset/worktree"), "{uris:?}");
    assert!(uris.contains(&"lector://changeset/staged"), "{uris:?}");
    assert!(uris.contains(&"lector://document/src/lib.rs"), "{uris:?}");

    let doc = &response(&out, 3)["result"]["contents"][0];
    assert_eq!(doc["uri"], "lector://document/src/lib.rs");
    assert_eq!(doc["text"], "pub fn a(x: u8) {}\npub fn b() {}\n");

    let changes = &response(&out, 4)["result"]["contents"][0];
    assert_eq!(changes["mimeType"], "application/json");
    let parsed: Value =
        serde_json::from_str(changes["text"].as_str().expect("text")).expect("json");
    assert!(parsed["files"].as_array().is_some_and(|f| !f.is_empty()));

    let templates = response(&out, 5)["result"]["resourceTemplates"]
        .as_array()
        .expect("templates")
        .clone();
    assert_eq!(templates.len(), 2);

    for id in [6, 7, 8] {
        assert_eq!(response(&out, id)["error"]["code"], -32002, "id {id}");
    }
    assert_eq!(response(&out, 9)["error"]["code"], -32602);
}

#[test]
fn a_symlink_cannot_escape_the_project_through_resources() {
    #[cfg(unix)]
    {
        let fx = fixture();
        let outside = fx.repo.parent().expect("parent").join("secret.txt");
        std::fs::write(&outside, "secret").expect("write");
        std::os::unix::fs::symlink(&outside, fx.repo.join("link.txt")).expect("symlink");
        let out = converse(
            &fx.server(false),
            &[req(
                1,
                "resources/read",
                json!({ "uri": "lector://document/link.txt" }),
            )],
        );
        assert_eq!(response(&out, 1)["error"]["code"], -32002, "{out:?}");
    }
}

/// Regression: `resources/read` served files inside `.git` (remote URLs with credentials).
#[test]
fn resources_never_serve_the_git_dir() {
    let fx = fixture();
    let out = converse(
        &fx.server(false),
        &[req(
            1,
            "resources/read",
            json!({ "uri": "lector://document/.git/config" }),
        )],
    );
    assert_eq!(response(&out, 1)["error"]["code"], -32002, "{out:?}");
}

#[test]
fn batches_are_answered_together() {
    let fx = fixture();
    let input = format!(
        "{}\n",
        json!([
            req(1, "ping", json!({})),
            { "jsonrpc": "2.0", "method": "notifications/initialized" },
            call(2, "git_list_changes", json!({})),
        ])
    );
    let out = converse_raw(&fx.server(false), &input);
    assert_eq!(out.len(), 1);
    let batch = out[0].as_array().expect("one array response");
    assert_eq!(batch.len(), 2, "notifications have no response");
    assert_eq!(batch[0]["id"], 1);
    assert_eq!(batch[1]["result"]["isError"], false);
}

#[test]
fn cancelled_requests_get_no_response() {
    let fx = fixture();
    fx.core.registry().register(FnCommand::new(
        CommandSpec::read("test.slow", "Slow", "Test", json!({"type": "object"})),
        |_, _| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            Ok(Outcome::new("slow done"))
        },
    ));
    let out = converse(
        &fx.server(false),
        &[
            call(1, "test_slow", json!({})),
            call(2, "git_list_changes", json!({})),
            json!({ "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 1 } }),
            req(3, "ping", json!({})),
        ],
    );
    assert!(
        out.iter().all(|m| m["id"] != json!(1)),
        "cancelled: {out:?}"
    );
    assert_eq!(response(&out, 2)["result"]["isError"], false);
    assert_eq!(response(&out, 3)["result"], json!({}));
}

#[test]
fn tool_calls_run_in_order_and_ping_is_not_blocked() {
    let fx = fixture();
    fx.core.registry().register(FnCommand::new(
        CommandSpec::read("test.slow", "Slow", "Test", json!({"type": "object"})),
        |_, _| {
            std::thread::sleep(std::time::Duration::from_millis(250));
            Ok(Outcome::new("slow done"))
        },
    ));
    let out = converse(
        &fx.server(false),
        &[
            call(1, "test_slow", json!({})),
            req(2, "ping", json!({})),
            call(3, "git_list_changes", json!({})),
        ],
    );
    let ids: Vec<u64> = out.iter().map(|m| m["id"].as_u64().expect("id")).collect();
    assert_eq!(
        ids,
        vec![2, 1, 3],
        "ping answers while the slow call runs; calls keep their order"
    );
}

#[test]
fn a_server_without_a_project_reports_tool_errors() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let core = AppCore::open(CoreOptions {
        in_memory_db: true,
        workers: Some(2),
        data_dir: Some(tmp.path().join("data")),
        ..Default::default()
    });
    let server = McpServer::new(core, None, ServerOptions::default());
    let out = converse(
        &server,
        &[
            req(1, "tools/list", json!({})),
            call(2, "git_list_changes", json!({})),
            req(3, "resources/list", json!({})),
        ],
    );
    assert!(!tool_names(response(&out, 1)).is_empty());
    assert_eq!(response(&out, 2)["result"]["isError"], true);
    assert_eq!(response(&out, 3)["result"]["resources"], json!([]));
}

/// Regression: `document.*` / `workspace.open_path` accepted absolute paths and `..`
/// anywhere on disk, so an MCP client could read (or, with `--allow-writes`, overwrite)
/// files outside the project, and `settings.get` handed out agent API keys.
#[test]
fn path_arguments_cannot_leave_the_project() {
    let fx = fixture();
    let outside = fx.repo.parent().expect("parent").join("secret.txt");
    std::fs::write(&outside, "secret").expect("write");
    let abs = outside.to_string_lossy().into_owned();
    let mut msgs = handshake();
    msgs.extend([
        call(2, "document_read", json!({ "path": abs })),
        call(3, "document_read", json!({ "path": "../secret.txt" })),
        call(4, "workspace_open_path", json!({ "path": abs })),
        call(5, "document_read", json!({ "path": ".git/config" })),
        call(
            6,
            "document_apply_edits",
            json!({ "path": abs, "edits": [{ "range": [0, 0], "new_text": "x" }] }),
        ),
        call(7, "document_save", json!({ "path": abs })),
        call(8, "document_read", json!({ "path": "src/../README.md" })),
    ]);
    let out = converse(&fx.server(true), &msgs);
    for id in 2..=7 {
        let r = &response(&out, id)["result"];
        assert_eq!(r["isError"], true, "call {id} must be refused: {r}");
        assert!(!r.to_string().contains("\"secret\""), "{r}");
    }
    assert_eq!(response(&out, 8)["result"]["isError"], false, "{out:?}");
    assert_eq!(std::fs::read_to_string(&outside).expect("read"), "secret");
}

/// Regression: `settings.get` returned agent environments (API keys) verbatim.
#[test]
fn settings_get_redacts_agent_environments() {
    let fx = fixture();
    let mut settings = fx.project.settings();
    settings.agent.servers.push(lector_config::AgentServer {
        name: "A".into(),
        command: "a".into(),
        args: vec![],
        env: [("API_KEY".to_string(), "sk-very-secret".to_string())].into(),
    });
    let project = Project::open(&fx.repo, settings).expect("project");
    let server = McpServer::new(fx.core.clone(), Some(project), ServerOptions::default());
    let mut msgs = handshake();
    msgs.push(call(2, "settings_get", json!({})));
    msgs.push(call(3, "settings_get", json!({ "key": "agent.servers" })));
    let out = converse(&server, &msgs);
    for id in [2, 3] {
        let text = response(&out, id).to_string();
        assert!(!text.contains("sk-very-secret"), "{text}");
        assert!(text.contains("<redacted>"), "{text}");
    }
}
