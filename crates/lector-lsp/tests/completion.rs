//! Completion, completion resolve and signature help against the scripted
//! `tests/fixtures/fake_completion.py`: wire format, capabilities, request
//! lifecycle (stale answers, cancellation, superseding) and the session models
//! driven end to end.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use futures::executor::block_on;
use lector_lsp::{
    CancelToken, CloseReason, CompletionRequest, CompletionResponse, CompletionSession,
    CompletionTrigger, InsertMode, LspError, LspManager, LspOptions, OpenReason, PointUtf16,
    RequestOutcome, ServerConfig, SessionOptions, SignatureSession, SignatureTrigger, Update,
};
use lector_text::Rope;
use serde_json::{json, Value};

fn have_python() -> bool {
    lector_lsp::which("python3").is_some()
}

struct Fx {
    _dir: tempfile::TempDir,
    file: PathBuf,
    log: PathBuf,
    m: LspManager,
}

const CAPS: fn() -> Value = || {
    json!({
        "completionProvider": {"triggerCharacters": ["."], "resolveProvider": true,
                               "allCommitCharacters": [";"]},
        "signatureHelpProvider": {"triggerCharacters": ["("], "retriggerCharacters": [","]}
    })
};

fn manager(dir: &Path, script: &Value, log: &Path, snippets: Option<bool>) -> LspManager {
    let script_path = dir.join("script.json");
    std::fs::write(&script_path, script.to_string()).unwrap();
    let cfg = ServerConfig {
        name: "fake".into(),
        command: "python3".into(),
        args: vec![
            format!(
                "{}/tests/fixtures/fake_completion.py",
                env!("CARGO_MANIFEST_DIR")
            ),
            "--script".into(),
            script_path.display().to_string(),
            "--log".into(),
            log.display().to_string(),
        ],
        languages: vec!["Rust".into()],
        root_markers: vec![],
    };
    let opts = LspOptions {
        request_timeout: Duration::from_secs(10),
        ..LspOptions::default()
    };
    let m = LspManager::with_options(dir.to_path_buf(), vec![cfg], opts);
    if let Some(on) = snippets {
        m.set_completion_snippets(on);
    }
    m
}

/// `script` gets the default capabilities unless it has its own.
fn fixture(mut script: Value, text: &Rope) -> Fx {
    if script.get("capabilities").is_none() {
        script["capabilities"] = CAPS();
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.rs");
    std::fs::write(&file, text.to_string()).unwrap();
    let log = dir.path().join("log.jsonl");
    let m = manager(dir.path(), &script, &log, None);
    m.open_document_tagged(&file, "Rust", &text.to_string(), text.version());
    wait_for(|| m.capabilities(&file).is_some());
    Fx {
        _dir: dir,
        file,
        log,
        m,
    }
}

impl Fx {
    fn log(&self) -> Vec<Value> {
        std::fs::read_to_string(&self.log)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect()
    }

    fn calls(&self, method: &str) -> Vec<Value> {
        self.log()
            .into_iter()
            .filter(|e| e["method"] == method)
            .collect()
    }

    fn sync(&self, text: &Rope) {
        self.m
            .open_document_tagged(&self.file, "Rust", &text.to_string(), text.version());
    }

    fn complete(&self, request: CompletionRequest) -> RequestOutcome<CompletionResponse> {
        block_on(self.m.completion(&self.file, request, None)).expect("completion")
    }
}

fn wait_for(mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs(10);
    while Instant::now() < end {
        if f() {
            return;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting");
}

fn pt(row: u32, column: u32) -> PointUtf16 {
    PointUtf16 { row, column }
}

fn invoked(pos: PointUtf16) -> CompletionRequest {
    CompletionRequest {
        pos,
        trigger: CompletionTrigger::Invoked,
    }
}

fn range16(row: u32, sc: u32, ec: u32) -> Value {
    json!({"start": {"line": row, "character": sc}, "end": {"line": row, "character": ec}})
}

fn with_caret(s: &str) -> (Rope, usize) {
    let caret = s.find('|').expect("caret marker");
    (Rope::from_str(&s.replacen('|', "", 1)), caret)
}

fn apply(text: &Rope, edits: &[lector_lsp::ByteEdit]) -> String {
    let mut s = text.to_string();
    for e in edits.iter().rev() {
        s.replace_range(e.range.clone(), &e.new_text);
    }
    s
}

#[test]
fn capabilities_are_parsed_and_declared_accurately() {
    if !have_python() {
        return;
    }
    let text = Rope::from_str("fn a() {}\n");
    let fx = fixture(json!({}), &text);
    let caps = fx.m.capabilities(&fx.file).unwrap();
    let c = caps.completion.expect("completion");
    assert_eq!(c.trigger_characters, vec!['.']);
    assert_eq!(c.all_commit_characters, vec![';']);
    assert!(c.resolve && c.is_trigger('.') && !c.is_trigger(','));
    let s = caps.signature_help.expect("signature help");
    assert_eq!(
        (s.trigger_characters, s.retrigger_characters),
        (vec!['('], vec![','])
    );

    let init = &fx.calls("initialize")[0]["params"]["capabilities"]["textDocument"];
    let item = &init["completion"]["completionItem"];
    assert_eq!(
        item["snippetSupport"],
        json!(false),
        "snippets are off by default"
    );
    assert_eq!(item["insertReplaceSupport"], json!(true));
    assert_eq!(item["labelDetailsSupport"], json!(true));
    assert_eq!(item["commitCharactersSupport"], json!(true));
    assert_eq!(
        item["documentationFormat"],
        json!(["markdown", "plaintext"])
    );
    assert_eq!(item["tagSupport"], json!({"valueSet": [1]}));
    assert_eq!(
        init["completion"]["completionList"]["itemDefaults"],
        json!([
            "commitCharacters",
            "editRange",
            "insertTextFormat",
            "insertTextMode",
            "data"
        ])
    );
    assert_eq!(
        init["signatureHelp"]["signatureInformation"]["parameterInformation"]["labelOffsetSupport"],
        json!(true)
    );
    fx.m.shutdown();

    // With the setting on, servers started afterwards are told snippets work.
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log.jsonl");
    let m = manager(
        dir.path(),
        &json!({"capabilities": CAPS()}),
        &log,
        Some(true),
    );
    let file = dir.path().join("a.rs");
    m.open_document_tagged(&file, "Rust", "x", 1);
    wait_for(|| m.capabilities(&file).is_some());
    let init = std::fs::read_to_string(&log).unwrap();
    assert!(init.contains("\"snippetSupport\": true"), "{init}");
    m.shutdown();
}

#[test]
fn items_of_every_shape_end_to_end_with_utf16_positions() {
    if !have_python() {
        return;
    }
    // "𝄞" is two UTF-16 units: the caret after "x." is at column 6.
    let (text, caret) = with_caret("𝄞é x.|");
    let script = json!({"textDocument/completion": [{"result": {"isIncomplete": false, "items": [
        {"label": "plain"},
        {"label": "insert_text()", "insertText": "insert_text", "filterText": "insert_text", "kind": 2},
        {"label": "edit", "kind": 5, "textEdit": {"range": range16(0, 6, 6), "newText": "edit_me"}}]}}]});
    let fx = fixture(script, &text);
    let mut session = CompletionSession::open(
        &text,
        caret,
        OpenReason::TriggerCharacter('.'),
        SessionOptions::default(),
    );
    let request = session.request();
    assert_eq!(request.pos, pt(0, 6));
    let RequestOutcome::Done(response) = fx.complete(request) else {
        panic!("no answer");
    };
    assert_eq!(
        (response.doc_version, response.tag),
        (1, Some(text.version()))
    );
    assert!(response.resolve_supported && !response.is_incomplete);
    assert_eq!(response.items.len(), 3);
    assert_eq!(
        response.items[0].commit_characters,
        vec![';'],
        "server-wide commit characters"
    );
    assert!(session.set_response(&text, caret, response));

    let sent = &fx.calls("textDocument/completion")[0]["params"];
    assert_eq!(sent["position"], json!({"line": 0, "character": 6}));
    assert_eq!(
        sent["context"],
        json!({"triggerKind": 2, "triggerCharacter": "."})
    );

    let by_label =
        |s: &CompletionSession, l: &str| s.items().iter().position(|i| i.label == l).expect(l);
    for (label, want) in [
        ("plain", "𝄞é x.plain"),
        ("insert_text()", "𝄞é x.insert_text"),
        ("edit", "𝄞é x.edit_me"),
    ] {
        let c = session
            .commit(&text, &[caret], 0, by_label(&session, label))
            .unwrap();
        assert_eq!(apply(&text, &c.edits), want, "{label}");
    }
    fx.m.shutdown();
}

#[test]
fn item_defaults_and_incomplete_requery() {
    if !have_python() {
        return;
    }
    let first = json!({"isIncomplete": true,
        "itemDefaults": {"editRange": range16(0, 0, 2), "commitCharacters": ["("], "data": {"d": 1}},
        "items": [{"label": "foo", "textEditText": "foo_a"}, {"label": "fox"}]});
    let second = json!({"isIncomplete": false,
        "itemDefaults": {"editRange": range16(0, 0, 3)},
        "items": [{"label": "foo", "textEditText": "foo_a"}, {"label": "fooz", "textEditText": "fooz_b"}]});
    let script = json!({"textDocument/completion": [{"result": first}, {"result": second}]});
    let (text, caret) = with_caret("fo|");
    let fx = fixture(script, &text);
    let mut session =
        CompletionSession::open(&text, caret, OpenReason::Typing, SessionOptions::default());
    let RequestOutcome::Done(r1) = fx.complete(session.request()) else {
        panic!()
    };
    assert!(r1.is_incomplete);
    assert_eq!(r1.items[0].commit_characters, vec!['(']);
    assert_eq!(r1.items[0].raw["data"], json!({"d": 1}));
    assert!(session.set_response(&text, caret, r1));
    assert_eq!(session.matches().len(), 2);

    // The user types "o": the list is incomplete, so ask again (trigger kind 3).
    let (typed, caret) = with_caret("foo|");
    fx.sync(&typed);
    let Update::Requery(req) = session.update(&typed, caret) else {
        panic!("expected a re-query");
    };
    assert_eq!(
        req,
        CompletionRequest {
            pos: pt(0, 3),
            trigger: CompletionTrigger::Incomplete
        }
    );
    let RequestOutcome::Done(r2) = fx.complete(req) else {
        panic!()
    };
    assert_eq!(
        fx.calls("textDocument/completion")[1]["params"]["context"]["triggerKind"],
        json!(3)
    );
    assert!(session.set_response(&typed, caret, r2));
    assert_eq!(
        session.update(&typed, caret),
        Update::Continue,
        "complete now: no further queries"
    );
    let labels: Vec<_> = session
        .matches()
        .iter()
        .map(|m| session.items()[m.item].label.clone())
        .collect();
    assert_eq!(labels, vec!["foo", "fooz"]);
    let c = session
        .commit(&typed, &[caret], 0, session.matches()[1].item)
        .unwrap();
    assert_eq!(apply(&typed, &c.edits), "fooz_b");
    fx.m.shutdown();
}

#[test]
fn replace_mode_uses_the_replace_range() {
    if !have_python() {
        return;
    }
    let script = json!({"textDocument/completion": [{"result": [
        {"label": "foobar", "textEdit": {"newText": "foobar",
            "insert": range16(0, 0, 2), "replace": range16(0, 0, 5)}}]}]});
    let (text, caret) = with_caret("fo|ooo bar");
    let fx = fixture(script, &text);
    for (mode, want) in [
        (InsertMode::Insert, "foobarooo bar"),
        (InsertMode::Replace, "foobar bar"),
    ] {
        let opts = SessionOptions {
            insert_mode: mode,
            ..SessionOptions::default()
        };
        let mut s = CompletionSession::open(&text, caret, OpenReason::Manual, opts);
        let RequestOutcome::Done(r) = fx.complete(s.request()) else {
            panic!()
        };
        assert!(s.set_response(&text, caret, r));
        let c = s.commit(&text, &[caret], 0, 0).unwrap();
        assert_eq!(apply(&text, &c.edits), want, "{mode:?}");
    }
    fx.m.shutdown();
}

#[test]
fn resolve_adds_documentation_and_additional_edits() {
    if !have_python() {
        return;
    }
    let script = json!({
        "textDocument/completion": [{"result": [{"label": "HashMap", "kind": 7, "data": {"id": 42}}]}],
        "completionItem/resolve": [{"result": {"label": "HashMap", "data": {"id": 42},
            "documentation": {"kind": "markdown", "value": "A **map**."}, "detail": "struct HashMap",
            "additionalTextEdits": [{"range": range16(0, 0, 0), "newText": "use std::collections::HashMap;\n"}]}}]});
    let (text, caret) = with_caret("let m: Ha|");
    let fx = fixture(script, &text);
    let mut session =
        CompletionSession::open(&text, caret, OpenReason::Typing, SessionOptions::default());
    let RequestOutcome::Done(response) = fx.complete(session.request()) else {
        panic!()
    };
    assert!(session.set_response(&text, caret, response));
    assert!(session.needs_resolve(0));

    let item = session.item(0).unwrap().clone();
    let generation = session.generation();
    let RequestOutcome::Done(resolved) =
        block_on(fx.m.resolve_completion(&fx.file, &item, None)).unwrap()
    else {
        panic!("not resolved");
    };
    assert_eq!(resolved.tag, Some(text.version()));
    assert_eq!(resolved.documentation.as_ref().unwrap().value, "A **map**.");
    let sent = &fx.calls("completionItem/resolve")[0]["params"];
    assert_eq!(
        sent["data"],
        json!({"id": 42}),
        "the server's item goes back as it came"
    );
    assert!(session.apply_resolved(generation, 0, resolved));
    assert!(!session.needs_resolve(0));
    let doc = session.item(0).unwrap().documentation.as_ref().unwrap();
    assert!(doc.markdown);

    // Resolved items are not resolved again.
    let again =
        block_on(fx.m.resolve_completion(&fx.file, session.item(0).unwrap(), None)).unwrap();
    assert_eq!(again, RequestOutcome::Unavailable);
    assert_eq!(fx.calls("completionItem/resolve").len(), 1);

    let c = session.commit(&text, &[caret], 0, 0).unwrap();
    assert_eq!(
        apply(&text, &c.edits),
        "use std::collections::HashMap;\nlet m: HashMap"
    );
    assert_eq!(
        c.carets,
        vec!["use std::collections::HashMap;\nlet m: HashMap".len()]
    );
    fx.m.shutdown();
}

#[test]
fn answers_for_a_changed_document_are_dropped() {
    if !have_python() {
        return;
    }
    let script = json!({"textDocument/completion": [
        {"result": [{"label": "slow"}], "delay": 0.4}, {"result": [{"label": "fast"}]}]});
    let (text, _) = with_caret("fo|");
    let fx = fixture(script, &text);
    let fut = fx.m.completion(&fx.file, invoked(pt(0, 2)), None);
    let handle = std::thread::spawn(move || block_on(fut));
    wait_for(|| !fx.calls("textDocument/completion").is_empty());
    // The user types: a new version is synced while the request is in flight.
    let (typed, _) = with_caret("foo|");
    fx.sync(&typed);
    assert_eq!(handle.join().unwrap().unwrap(), RequestOutcome::Stale);
    // A request for the new version works.
    let RequestOutcome::Done(r) = fx.complete(invoked(pt(0, 3))) else {
        panic!()
    };
    assert_eq!(r.items[0].label, "fast");
    assert_eq!(r.doc_version, 2);
    // A request made for a version that is already outdated when it is sent is not sent at all.
    let before = fx.calls("textDocument/completion").len();
    let fut = fx.m.completion(&fx.file, invoked(pt(0, 3)), None);
    let (typed2, _) = with_caret("fooo|");
    fx.sync(&typed2);
    assert_eq!(block_on(fut).unwrap(), RequestOutcome::Stale);
    assert_eq!(fx.calls("textDocument/completion").len(), before);
    fx.m.shutdown();
}

#[test]
fn a_new_request_cancels_the_one_in_flight() {
    if !have_python() {
        return;
    }
    let script = json!({"textDocument/completion": [
        {"result": [{"label": "never"}], "delay": 30, "ignore_cancel": true},
        {"result": [{"label": "second"}]}]});
    let (text, _) = with_caret("f|");
    let fx = fixture(script, &text);
    let first = fx.m.completion(&fx.file, invoked(pt(0, 1)), None);
    let handle = std::thread::spawn(move || block_on(first));
    wait_for(|| !fx.calls("textDocument/completion").is_empty());
    let first_id = fx.calls("textDocument/completion")[0]["id"].clone();

    let RequestOutcome::Done(second) = fx.complete(invoked(pt(0, 1))) else {
        panic!()
    };
    assert_eq!(second.items[0].label, "second");
    // The first one ends at once, even though this server ignores cancellation.
    let started = Instant::now();
    assert_eq!(handle.join().unwrap().unwrap(), RequestOutcome::Cancelled);
    assert!(started.elapsed() < Duration::from_secs(5));
    wait_for(|| !fx.calls("$/cancelRequest").is_empty());
    assert_eq!(fx.calls("$/cancelRequest")[0]["params"]["id"], first_id);
    fx.m.shutdown();
}

#[test]
fn a_cancel_token_cancels_the_request() {
    if !have_python() {
        return;
    }
    let script = json!({"textDocument/completion": [{"result": [{"label": "late"}], "delay": 30}]});
    let (text, _) = with_caret("f|");
    let fx = fixture(script, &text);
    let token = CancelToken::new();
    let fut = fx.m.completion(&fx.file, invoked(pt(0, 1)), Some(&token));
    let handle = std::thread::spawn(move || block_on(fut));
    wait_for(|| !fx.calls("textDocument/completion").is_empty());
    token.cancel();
    assert_eq!(handle.join().unwrap().unwrap(), RequestOutcome::Cancelled);
    wait_for(|| !fx.calls("$/cancelRequest").is_empty());

    // Cancelled before the request is even polled: nothing is sent.
    let before = fx.calls("textDocument/completion").len();
    let token = CancelToken::new();
    let fut = fx.m.completion(&fx.file, invoked(pt(0, 1)), Some(&token));
    token.cancel();
    assert_eq!(block_on(fut).unwrap(), RequestOutcome::Cancelled);
    assert_eq!(fx.calls("textDocument/completion").len(), before);
    fx.m.shutdown();
}

#[test]
fn server_cancellation_and_content_modified_errors_are_not_failures() {
    if !have_python() {
        return;
    }
    let script = json!({"textDocument/completion": [
        {"error": {"code": -32801, "message": "content modified"}},
        {"error": {"code": -32800, "message": "cancelled"}},
        {"error": {"code": -32603, "message": "boom"}}]});
    let (text, _) = with_caret("f|");
    let fx = fixture(script, &text);
    let run = || block_on(fx.m.completion(&fx.file, invoked(pt(0, 1)), None));
    assert_eq!(run().unwrap(), RequestOutcome::Stale);
    assert_eq!(run().unwrap(), RequestOutcome::Cancelled);
    assert!(matches!(run(), Err(LspError::Server { code: -32603, .. })));
    fx.m.shutdown();
}

#[test]
fn unsupported_or_unknown_documents_are_unavailable() {
    if !have_python() {
        return;
    }
    let (text, _) = with_caret("f|");
    let fx = fixture(json!({"capabilities": {"hoverProvider": true}}), &text);
    assert!(fx.m.capabilities(&fx.file).unwrap().completion.is_none());
    assert_eq!(fx.complete(invoked(pt(0, 1))), RequestOutcome::Unavailable);
    let sig = SignatureSession::new().request(pt(0, 1), SignatureTrigger::Invoked);
    assert_eq!(
        block_on(fx.m.signature_help(&fx.file, sig, None)).unwrap(),
        RequestOutcome::Unavailable
    );
    assert!(fx.calls("textDocument/completion").is_empty());
    let other = fx.file.with_file_name("not_open.rs");
    assert_eq!(
        block_on(fx.m.completion(&other, invoked(pt(0, 1)), None)).unwrap(),
        RequestOutcome::Unavailable
    );
    fx.m.shutdown();
}

#[test]
fn signature_help_with_both_label_forms_and_retrigger() {
    if !have_python() {
        return;
    }
    let first = json!({"signatures": [
        {"label": "fn f(a: i32, b: &str)", "documentation": {"kind": "markdown", "value": "docs"},
         "parameters": [{"label": "a: i32"}, {"label": "b: &str", "documentation": "the b"}]},
        {"label": "fn f(é: u8, 😀: u8)", "parameters": [{"label": [5, 10]}, {"label": [12, 18]}]}],
        "activeSignature": 0, "activeParameter": 1});
    let script = json!({"textDocument/signatureHelp": [
        {"result": first}, {"result": first}, {"result": null}]});
    let (text, caret) = with_caret("f(1, |");
    let fx = fixture(script, &text);
    let mut s = SignatureSession::new();
    let caps = fx.m.capabilities(&fx.file).unwrap().signature_help.unwrap();
    assert_eq!(
        s.trigger_for_typed(&caps, '('),
        Some(SignatureTrigger::TriggerCharacter('('))
    );
    assert_eq!(s.trigger_for_typed(&caps, ','), None);

    let pos = pt(0, caret as u32);
    let request = s.request(pos, SignatureTrigger::TriggerCharacter('('));
    let RequestOutcome::Done(help) =
        block_on(fx.m.signature_help(&fx.file, request, None)).unwrap()
    else {
        panic!()
    };
    s.set_help(help);
    let a = s.active().unwrap();
    assert_eq!((a.index, a.count, a.parameter), (0, 2, Some(1)));
    assert_eq!(&a.signature.label[a.highlight.clone().unwrap()], "b: &str");
    assert_eq!(a.parameter_documentation.unwrap().value, "the b");
    let sent = &fx.calls("textDocument/signatureHelp")[0]["params"];
    assert_eq!(
        sent["context"],
        json!({"triggerKind": 2, "triggerCharacter": "(", "isRetrigger": false})
    );

    // The user picks the second overload, then types ",": a retrigger with the active help.
    s.next_signature();
    assert_eq!(
        s.trigger_for_typed(&caps, ','),
        Some(SignatureTrigger::TriggerCharacter(','))
    );
    let request = s.request(pos, SignatureTrigger::TriggerCharacter(','));
    let RequestOutcome::Done(help) =
        block_on(fx.m.signature_help(&fx.file, request, None)).unwrap()
    else {
        panic!()
    };
    s.set_help(help);
    let sent = &fx.calls("textDocument/signatureHelp")[1]["params"]["context"];
    assert_eq!(sent["isRetrigger"], json!(true));
    assert_eq!(sent["activeSignatureHelp"]["activeSignature"], json!(1));
    let a = s.active().unwrap();
    assert_eq!(a.index, 1, "the user's pick survives the answer");
    assert_eq!(
        &a.signature.label[a.highlight.clone().unwrap()],
        "😀: u8",
        "UTF-16 offsets"
    );

    // No signature at the caret: the popup closes.
    let request = s.request(pos, SignatureTrigger::ContentChange);
    let RequestOutcome::Done(help) =
        block_on(fx.m.signature_help(&fx.file, request, None)).unwrap()
    else {
        panic!()
    };
    assert!(help.is_none());
    s.set_help(help);
    assert!(!s.is_open());
    fx.m.shutdown();
}

#[test]
fn typing_flow_closes_when_the_word_ends() {
    if !have_python() {
        return;
    }
    let script =
        json!({"textDocument/completion": [{"result": [{"label": "foo_bar"}, {"label": "foo"}]}]});
    let (text, caret) = with_caret("fo|");
    let fx = fixture(script, &text);
    let mut s =
        CompletionSession::open(&text, caret, OpenReason::Typing, SessionOptions::default());
    let RequestOutcome::Done(r) = fx.complete(s.request()) else {
        panic!()
    };
    assert!(s.set_response(&text, caret, r));
    assert_eq!(s.selected_item().unwrap().1.label, "foo");
    let (typed, caret) = with_caret("foo(|");
    assert_eq!(
        s.update(&typed, caret),
        Update::Close(CloseReason::LeftWord)
    );
    fx.m.shutdown();
}

/// Needs `pylsp` on PATH (`pip install python-lsp-server`):
/// `cargo test -p lector-lsp --test completion -- --ignored`.
#[test]
#[ignore = "needs pylsp; run with --ignored"]
fn pylsp_smoke() {
    if lector_lsp::which("pylsp").is_none() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("main.py");
    let source = "import os\nos.pat|\nprint(\"a\", )\n";
    let (text, caret) = with_caret(source);
    std::fs::write(&file, text.to_string()).unwrap();
    let configs: Vec<_> = lector_lsp::builtin_servers()
        .into_iter()
        .filter(|c| c.name == "pylsp")
        .collect();
    let m = LspManager::new(dir.path().to_path_buf(), configs);
    m.open_document_tagged(&file, "Python", &text.to_string(), text.version());
    let mut session = CompletionSession::open(
        &text,
        caret,
        OpenReason::TriggerCharacter('.'),
        SessionOptions::default(),
    );
    let end = Instant::now() + Duration::from_secs(60);
    let mut found = false;
    while Instant::now() < end && !found {
        match block_on(m.completion(&file, session.request(), None)) {
            Ok(RequestOutcome::Done(r)) if r.items.iter().any(|i| i.label == "path") => {
                assert!(session.set_response(&text, caret, r));
                found = true;
            }
            _ => std::thread::sleep(Duration::from_millis(300)),
        }
    }
    assert!(found, "pylsp never offered `path`");
    assert_eq!(session.selected_item().unwrap().1.label, "path");
    let c = session
        .commit(&text, &[caret], 0, session.matches()[0].item)
        .unwrap();
    assert!(
        apply(&text, &c.edits).contains("os.path\n"),
        "{:?}",
        c.edits
    );

    // Signature help inside `print("a", |)`.
    let mut sig = SignatureSession::new();
    let request = sig.request(pt(2, 11), SignatureTrigger::Invoked);
    if let Ok(RequestOutcome::Done(help)) = block_on(m.signature_help(&file, request, None)) {
        sig.set_help(help);
        assert!(sig
            .active()
            .is_some_and(|a| a.signature.label.contains("print")));
    }
    m.shutdown();
}
