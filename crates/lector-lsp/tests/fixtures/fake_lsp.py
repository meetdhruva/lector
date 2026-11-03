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

log("start")
docs = {}
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
        time.sleep(args.init_delay)
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": {"hoverProvider": True}}})
        if args.probe_config:
            send({"jsonrpc": "2.0", "id": 900, "method": "workspace/configuration",
                  "params": {"items": [{"section": "a"}, {"section": "b"}]}})
            send({"jsonrpc": "2.0", "id": 901, "method": "client/registerCapability", "params": {"registrations": []}})
        if args.always_crash:
            sys.exit(1)
    elif method == "textDocument/didOpen":
        d = p["textDocument"]
        docs[d["uri"]] = d["text"]
        log("open %s v%s %s" % (d["uri"].rsplit("/", 1)[-1], d["version"], d["languageId"]))
    elif method == "textDocument/didChange":
        log("change v%s" % p["textDocument"]["version"])
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
             "location": {"uri": "file:///tmp/x.rs",
                          "range": {"start": {"line": 3, "character": 1}, "end": {"line": 3, "character": 4}}}}]})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        sys.exit(0)
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "result": None})
