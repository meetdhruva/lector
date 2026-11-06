#!/usr/bin/env python3
"""A fake ACP agent for Lector's tests and screenshots.

Speaks newline-delimited JSON-RPC on stdio. Each prompt is a small script, one command
per line (unknown lines are echoed back as text):

    say <text>              stream <text> as agent message chunks (word by word)
    think <text>            a thought chunk
    plan a|b|c              a plan with three entries (first completed, second in progress)
    write <path> <text>     tool call (edit) + fs/write_text_file; `\\n` in text = newline
    read <path>             fs/read_text_file, then say "READ:<content>"
    ask <title>             session/request_permission (kind edit), then say "PERMISSION:<answer>"
    askread <title>         same with kind read
    slow                    stream chunks until session/cancel arrives -> stopReason cancelled
    crash                   write to stderr and exit(3) mid-turn
    garbage                 print a malformed line
    stop <reason>           answer with this stopReason

Environment: FAKE_ACP_NO_EMBED=1 reports embeddedContext = false;
FAKE_ACP_DELAY=<seconds> sleeps between streamed chunks (screenshots / demos);
FAKE_ACP_DEMO=1 ignores the prompt text and plays a scripted demo (screenshots).
"""
import json
import os
import queue
import sys
import threading
import time

inbox = queue.Queue()
next_id = [1000]
session_id = "fake-session-1"
delay = float(os.environ.get("FAKE_ACP_DELAY", "0"))


def reader():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            inbox.put(json.loads(line))
        except ValueError:
            pass
    inbox.put(None)


def send(msg):
    msg["jsonrpc"] = "2.0"
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def update(u):
    send({"method": "session/update", "params": {"sessionId": session_id, "update": u}})
    if delay:
        time.sleep(delay)


pending_cancel = [False]
deferred = []


def handle_side(msg):
    """Messages that arrive while we wait for something else."""
    if msg.get("method") == "session/cancel":
        pending_cancel[0] = True
    elif "method" in msg and "id" in msg:
        deferred.append(msg)


def request(method, params):
    next_id[0] += 1
    rid = next_id[0]
    send({"id": rid, "method": method, "params": params})
    while True:
        msg = inbox.get()
        if msg is None:
            sys.exit(0)
        if msg.get("id") == rid and "method" not in msg:
            return msg
        handle_side(msg)


def say(text):
    words = text.split(" ")
    for i, w in enumerate(words):
        chunk = w if i == len(words) - 1 else w + " "
        update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": chunk}})


PARSER_V2 = """/// Parses `key = value` lines.
pub fn parse(input: &str) -> Result<Vec<(String, String)>, ParseError> {
    if input.trim().is_empty() {
        return Err(ParseError::Empty);
    }
    let mut out = Vec::new();
    for (n, line) in input.lines().enumerate() {
        let (key, value) = line
            .split_once('=')
            .ok_or(ParseError::MissingEquals { line: n + 1 })?;
        out.push((key.trim().to_string(), value.trim().to_string()));
    }
    Ok(out)
}

/// Why [`parse`] failed.
#[derive(Debug, PartialEq)]
pub enum ParseError {
    /// Nothing to parse.
    Empty,
    /// A line without `=`.
    MissingEquals { line: usize },
}
"""

TEST_V1 = """use demo::parser::{parse, ParseError};

#[test]
fn empty_input_is_an_error() {
    assert_eq!(parse(""), Err(ParseError::Empty));
}

#[test]
fn reports_the_line_without_equals() {
    let input = "a = 1
b";
    assert_eq!(parse(input), Err(ParseError::MissingEquals { line: 2 }));
}
"""

demo_turn = [0]


def demo_script():
    demo_turn[0] += 1
    enc = lambda t: t.replace("\n", "\\n")
    if demo_turn[0] == 1:
        return "\n".join([
            "think The parser unwraps on lines without '=' and accepts empty input. Return a typed error instead, then pin it with tests.",
            "plan Read src/parser.rs|Return ParseError instead of panicking|Add regression tests",
            "read src/parser.rs",
            "write src/parser.rs " + enc(PARSER_V2),
            "write tests/parser.rs " + enc(TEST_V1),
            "plandone Read src/parser.rs|Return ParseError instead of panicking|Add regression tests",
            "say `parse` now returns `Result<_, ParseError>` instead of panicking:\\n\\n- **Empty input** → `ParseError::Empty`\\n- **A line without `=`** → `ParseError::MissingEquals { line }` (1-based)\\n\\nI added two regression tests in `tests/parser.rs`. Callers that used the old `Vec` return need a `?`.",
        ])
    return "\n".join([
        "think Run the test suite to confirm the change.",
        "ask Run `cargo test --all`",
        "say All 6 tests pass.",
    ])


def run_prompt(rid, params):
    pending_cancel[0] = False
    blocks = params.get("prompt", [])
    text = ""
    for b in blocks:
        if b.get("type") == "text":
            text = b["text"]
            break
    if os.environ.get("FAKE_ACP_DEMO") == "1":
        text = demo_script()
        blocks = blocks[:1]
    extras = [b for b in blocks if b is not blocks[0]] if blocks else []
    if extras:
        kinds = ",".join(b.get("type", "?") for b in extras)
        say("CONTEXT:" + kinds)
        for b in extras:
            if b.get("type") == "text":
                say("CTX:" + " ".join(b["text"].split()))
    stop = "end_turn"
    call_n = 0
    for line in text.splitlines():
        cmd, _, rest = line.strip().partition(" ")
        if cmd == "say":
            say(rest.replace("\\n", "\n"))
        elif cmd == "think":
            update({"sessionUpdate": "agent_thought_chunk", "content": {"type": "text", "text": rest}})
        elif cmd in ("plan", "plandone"):
            names = rest.split("|")
            statuses = ["completed", "in_progress", "pending"] if cmd == "plan" else ["completed"] * 3
            update({"sessionUpdate": "plan", "entries": [
                {"content": n, "priority": "medium", "status": statuses[min(i, 2)]}
                for i, n in enumerate(names)]})
        elif cmd == "write":
            path, _, body = rest.partition(" ")
            body = body.replace("\\n", "\n")
            call_n += 1
            cid = "call-%d" % call_n
            abs_path = path if os.path.isabs(path) else os.path.join(os.getcwd(), path)
            update({"sessionUpdate": "tool_call", "toolCallId": cid, "title": "Edit " + path,
                    "kind": "edit", "status": "in_progress",
                    "locations": [{"path": abs_path, "line": 1}]})
            old = None
            try:
                with open(abs_path, encoding="utf-8") as f:
                    old = f.read()
            except OSError:
                pass
            resp = request("fs/write_text_file", {"sessionId": session_id, "path": abs_path, "content": body})
            status = "failed" if "error" in resp else "completed"
            content = [{"type": "diff", "path": abs_path, "oldText": old, "newText": body}]
            if "error" in resp:
                content = [{"type": "content", "content": {"type": "text", "text": "ERROR:" + resp["error"]["message"]}}]
            update({"sessionUpdate": "tool_call_update", "toolCallId": cid, "status": status, "content": content})
            if "error" in resp:
                say("WRITE-ERROR:" + resp["error"]["message"])
        elif cmd == "read":
            abs_path = rest if os.path.isabs(rest) else os.path.join(os.getcwd(), rest)
            resp = request("fs/read_text_file", {"sessionId": session_id, "path": abs_path})
            if "error" in resp:
                say("READ-ERROR:" + resp["error"]["message"])
            else:
                update({"sessionUpdate": "agent_message_chunk",
                        "content": {"type": "text", "text": "READ:" + resp["result"]["content"]}})
        elif cmd in ("ask", "askread"):
            call_n += 1
            cid = "call-%d" % call_n
            kind = "read" if cmd == "askread" else "edit"
            update({"sessionUpdate": "tool_call", "toolCallId": cid, "title": rest, "kind": kind,
                    "status": "pending"})
            resp = request("session/request_permission", {
                "sessionId": session_id,
                "toolCall": {"toolCallId": cid, "title": rest, "kind": kind},
                "options": [
                    {"optionId": "yes", "name": "Allow", "kind": "allow_once"},
                    {"optionId": "always", "name": "Always allow", "kind": "allow_always"},
                    {"optionId": "no", "name": "Reject", "kind": "reject_once"},
                ]})
            outcome = resp.get("result", {}).get("outcome", {})
            answer = outcome.get("optionId", outcome.get("outcome", "?"))
            update({"sessionUpdate": "tool_call_update", "toolCallId": cid,
                    "status": "completed" if answer in ("yes", "always") else "failed"})
            say("PERMISSION:" + answer)
            if pending_cancel[0]:
                stop = "cancelled"
                break
        elif cmd == "slow":
            for i in range(2000):
                update({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "."}})
                try:
                    msg = inbox.get(timeout=0.02)
                except queue.Empty:
                    msg = None
                if msg is not None:
                    handle_side(msg)
                if pending_cancel[0]:
                    break
            stop = "cancelled" if pending_cancel[0] else "end_turn"
        elif cmd == "crash":
            sys.stderr.write("fake agent: boom\n")
            sys.stderr.flush()
            os._exit(3)
        elif cmd == "garbage":
            sys.stdout.write("this is not json\n")
            sys.stdout.flush()
        elif cmd == "stop":
            stop = rest or "end_turn"
        elif line.strip():
            say(line.strip())
    send({"id": rid, "result": {"stopReason": stop}})


def main():
    threading.Thread(target=reader, daemon=True).start()
    while True:
        msg = deferred.pop(0) if deferred else inbox.get()
        if msg is None:
            return
        method = msg.get("method")
        rid = msg.get("id")
        if method == "initialize":
            send({"id": rid, "result": {
                "protocolVersion": 1,
                "agentCapabilities": {
                    "loadSession": False,
                    "promptCapabilities": {
                        "image": False, "audio": False,
                        "embeddedContext": os.environ.get("FAKE_ACP_NO_EMBED") != "1"}},
                "agentInfo": {"name": "fake", "title": "Fake Agent"},
                "authMethods": []}})
        elif method == "session/new":
            send({"id": rid, "result": {"sessionId": session_id}})
        elif method == "session/prompt":
            run_prompt(rid, msg.get("params", {}))
        elif method == "session/cancel":
            pass
        elif rid is not None:
            send({"id": rid, "error": {"code": -32601, "message": "unknown " + str(method)}})


if __name__ == "__main__":
    main()
