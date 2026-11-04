#!/usr/bin/env python3
"""Tiny fake language server for lector-lsp tests."""
import json, os, sys, time, argparse

ap = argparse.ArgumentParser()
ap.add_argument("--init-delay", type=float, default=0)
ap.add_argument("--hang-hover", action="store_true")
ap.add_argument("--crash-marker")      # crash on first hover if marker absent
ap.add_argument("--always-crash", action="store_true")  # exit right after initialize
ap.add_argument("--log")
ap.add_argument("--probe-config", action="store_true")
ap.add_argument("--editing", action="store_true")  # formatting, actions, rename, diagnostics
args = ap.parse_args()

inp, out = sys.stdin.buffer, sys.stdout.buffer

def log(s):
    if args.log:
        with open(args.log, "a") as f:
            f.write(s + "\n")

def send(msg):
    b = json.dumps(msg).encode()
    out.write(b"Content-Length: %d\r\n\r\n" % len(b) + b)
    out.flush()

def read():
    n = None
    while True:
        line = inp.readline()
        if not line:
            return None
        line = line.strip()
        if not line:
            break
        k, _, v = line.partition(b":")
        if k.lower() == b"content-length":
            n = int(v)
    return json.loads(inp.read(n))

def rng(a, b, c, d):
    return {"start": {"line": a, "character": b}, "end": {"line": c, "character": d}}

def utf16(s):
    return len(s.encode("utf-16-le")) // 2

def publish(uri):
    if not args.editing:
        return
    diags = []
    for i, line in enumerate(docs[uri].split("\n")):
        for word, sev in (("bad", 1), ("warn", 2)):
            start = line.find(word)
            if start >= 0:
                a = utf16(line[:start])
                diags.append({"range": rng(i, a, i, a + len(word)), "severity": sev,
                              "message": "%s word" % word, "source": "fake", "code": "F%d" % sev})
    send({"jsonrpc": "2.0", "method": "textDocument/publishDiagnostics",
          "params": {"uri": uri, "version": versions.get(uri), "diagnostics": diags}})

def trim_edits(uri, rows):
    out = []
    for i, line in enumerate(docs.get(uri, "").split("\n")):
        if rows and not (rows[0] <= i <= rows[1]):
            continue
        stripped = line.rstrip(" \t")
        if stripped != line:
            out.append({"range": rng(i, utf16(stripped), i, utf16(line)), "newText": ""})
    return out

def word_at(uri, pos):
    lines = docs.get(uri, "").split("\n")
    if pos["line"] >= len(lines):
        return None
    line = lines[pos["line"]]
    c = pos["character"]
    a = c
    while a > 0 and (line[a - 1].isalnum() or line[a - 1] == "_"):
        a -= 1
    b = c
    while b < len(line) and (line[b].isalnum() or line[b] == "_"):
        b += 1
    if a == b:
        return None
    return pos["line"], a, b, line[a:b]

def occurrences(text, word, new):
    edits = []
    for i, line in enumerate(text.split("\n")):
        start = 0
        while True:
            j = line.find(word, start)
            if j < 0:
                break
            edits.append({"range": rng(i, utf16(line[:j]), i, utf16(line[:j]) + utf16(word)), "newText": new})
            start = j + len(word)
    return edits

log("start")
docs = {}
versions = {}
root_uri = "file:///tmp"


def uri_to_path(uri):
    """file:// URI to a local path (Windows: file:///C:/x -> C:\\x)."""
    from urllib.parse import urlparse
    from urllib.request import url2pathname  # also percent-decodes
    return url2pathname(urlparse(uri).path)


while True:
    m = read()
    if m is None:
        break
    method, mid, p = m.get("method"), m.get("id"), m.get("params")
    if method is None:
        log("response " + json.dumps(m.get("result")))
        continue
    log(method)
    if method == "initialize":
        root_uri = (p or {}).get("rootUri") or "file:///tmp"
        time.sleep(args.init_delay)
        caps = {"hoverProvider": True}
        if args.editing:
            caps.update({
                "definitionProvider": True,
                "documentFormattingProvider": True,
                "documentRangeFormattingProvider": True,
                "renameProvider": {"prepareProvider": True},
                "codeActionProvider": {"resolveProvider": True},
                "executeCommandProvider": {"commands": ["fake.applyEdit"]},
            })
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": caps}})
        if args.probe_config:
            send({"jsonrpc": "2.0", "id": 900, "method": "workspace/configuration",
                  "params": {"items": [{"section": "a"}, {"section": "b"}]}})
            send({"jsonrpc": "2.0", "id": 901, "method": "client/registerCapability", "params": {"registrations": []}})
        if args.always_crash:
            sys.exit(1)
    elif method == "textDocument/didOpen":
        d = p["textDocument"]
        docs[d["uri"]] = d["text"]
        versions[d["uri"]] = d["version"]
        log("open %s v%s %s" % (d["uri"].rsplit("/", 1)[-1], d["version"], d["languageId"]))
        publish(d["uri"])
    elif method == "textDocument/didChange":
        d = p["textDocument"]
        docs[d["uri"]] = p["contentChanges"][-1]["text"]
        versions[d["uri"]] = d["version"]
        log("change v%s" % d["version"])
        publish(d["uri"])
    elif method == "textDocument/formatting":
        send({"jsonrpc": "2.0", "id": mid, "result": trim_edits(p["textDocument"]["uri"], None)})
        log("format tab=%s spaces=%s" % (p["options"]["tabSize"], p["options"]["insertSpaces"]))
    elif method == "textDocument/rangeFormatting":
        r = p["range"]
        send({"jsonrpc": "2.0", "id": mid,
              "result": trim_edits(p["textDocument"]["uri"], (r["start"]["line"], r["end"]["line"]))})
    elif method == "textDocument/codeAction":
        uri = p["textDocument"]["uri"]
        ctx = p["context"]["diagnostics"]
        log("codeAction diags=%d trigger=%s" % (len(ctx), p["context"].get("triggerKind")))
        res = []
        if ctx:
            res.append({"title": "Replace with good", "kind": "quickfix", "isPreferred": True,
                        "diagnostics": ctx,
                        "edit": {"changes": {uri: [{"range": d["range"], "newText": "good"} for d in ctx]}}})
        res.append({"title": "Add header", "kind": "refactor.rewrite", "data": {"uri": uri}})
        res.append({"title": "Run command", "command": "fake.applyEdit", "arguments": [uri]})
        res.append({"title": "Cannot do", "kind": "refactor", "disabled": {"reason": "not here"}})
        send({"jsonrpc": "2.0", "id": mid, "result": res})
    elif method == "codeAction/resolve":
        a = dict(p)
        uri = a["data"]["uri"]
        a["edit"] = {"documentChanges": [{"textDocument": {"uri": uri, "version": versions.get(uri)},
                     "edits": [{"range": rng(0, 0, 0, 0), "newText": "// header\n"}]}]}
        send({"jsonrpc": "2.0", "id": mid, "result": a})
    elif method == "workspace/executeCommand":
        uri = p["arguments"][0]
        send({"jsonrpc": "2.0", "id": 1000, "method": "workspace/applyEdit",
              "params": {"label": "cmd", "edit": {"changes": {uri: [{"range": rng(0, 0, 0, 0), "newText": "cmd\n"}]}}}})
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "textDocument/prepareRename":
        w = word_at(p["textDocument"]["uri"], p["position"])
        if w is None:
            send({"jsonrpc": "2.0", "id": mid, "result": None})
        else:
            line, a, b, word = w
            send({"jsonrpc": "2.0", "id": mid, "result": {"range": rng(line, a, line, b), "placeholder": word}})
    elif method == "textDocument/rename":
        uri = p["textDocument"]["uri"]
        w = word_at(uri, p["position"])
        if w is None:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32602, "message": "nothing to rename"}})
            continue
        word = w[3]
        changes = [{"textDocument": {"uri": uri, "version": versions.get(uri)},
                    "edits": occurrences(docs.get(uri, ""), word, p["newName"])}]
        other = root_uri.rstrip("/") + "/other.rs"
        try:
            other_text = open(uri_to_path(other)).read()
        except OSError:
            other_text = ""
        if word in other_text:
            changes.append({"textDocument": {"uri": other, "version": None},
                            "edits": occurrences(other_text, word, p["newName"])})
        send({"jsonrpc": "2.0", "id": mid, "result": {"documentChanges": changes}})
    elif method == "textDocument/hover":
        if args.crash_marker and not os.path.exists(args.crash_marker):
            open(args.crash_marker, "w").close()
            sys.exit(1)
        if args.hang_hover:
            continue
        pos = p["position"]
        send({"jsonrpc": "2.0", "id": mid, "result": {
            "contents": {"kind": "markdown", "value": "hover %d:%d" % (pos["line"], pos["character"])},
            "range": {"start": pos, "end": {"line": pos["line"], "character": pos["character"] + 1}}}})
    elif method in ("textDocument/definition", "textDocument/references"):
        uri = p["textDocument"]["uri"]
        r = lambda l: {"start": {"line": l, "character": 0}, "end": {"line": l, "character": 3}}
        res = [{"uri": uri, "range": r(p["position"]["line"] + 1)}]
        if method.endswith("references"):
            res.append({"uri": uri, "range": r(0)})
        send({"jsonrpc": "2.0", "id": mid, "result": res})
    elif method == "textDocument/documentSymbol":
        r = lambda a, b: {"start": {"line": a, "character": 0}, "end": {"line": b, "character": 0}}
        send({"jsonrpc": "2.0", "id": mid, "result": [
            {"name": "Outer", "kind": 5, "range": r(0, 5), "selectionRange": r(0, 1),
             "children": [{"name": "inner", "kind": 6, "range": r(1, 2), "selectionRange": r(1, 2)}]}]})
    elif method == "workspace/symbol":
        send({"jsonrpc": "2.0", "id": mid, "result": [
            {"name": p["query"] + "_sym", "kind": 12, "containerName": "mod",
             "location": {"uri": root_uri.rstrip("/") + "/x.rs",
                          "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 4}}}}]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        sys.exit(0)
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
