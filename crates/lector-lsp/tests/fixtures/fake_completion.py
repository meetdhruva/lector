#!/usr/bin/env python3
"""Scripted fake language server for the completion / signature help tests.

--script FILE  JSON: {"capabilities": {...}, "<method>": [step, ...]}; each step is
               {"result": ..., "error": {code, message}, "delay": seconds, "ignore_cancel": bool}.
               Steps are consumed in order per method; the last one repeats.
--log FILE     every message received (except didOpen/didChange text) as one JSON line.
"""
import json, sys, threading, argparse

ap = argparse.ArgumentParser()
ap.add_argument("--script", required=True)
ap.add_argument("--log", required=True)
args = ap.parse_args()
script = json.load(open(args.script, encoding="utf-8"))
inp, out = sys.stdin.buffer, sys.stdout.buffer
lock = threading.Lock()
pending = {}


def log(entry):
    with lock:
        with open(args.log, "a", encoding="utf-8") as f:
            f.write(json.dumps(entry) + "\n")


def send(msg):
    b = json.dumps(msg).encode()
    with lock:
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


def reply(mid, step):
    pending.pop(mid, None)
    if "error" in step:
        send({"jsonrpc": "2.0", "id": mid, "error": step["error"]})
    else:
        send({"jsonrpc": "2.0", "id": mid, "result": step.get("result")})


counters = {}


def next_step(method):
    steps = script.get(method) or [{"result": None}]
    i = counters.get(method, 0)
    counters[method] = i + 1
    return steps[min(i, len(steps) - 1)]


while True:
    m = read()
    if m is None:
        break
    method, mid, p = m.get("method"), m.get("id"), m.get("params")
    if method is None:
        continue
    entry = {"method": method, "id": mid, "params": p}
    if method in ("textDocument/didOpen", "textDocument/didChange"):
        d = p["textDocument"]
        entry = {"method": method, "version": d.get("version")}
    log(entry)
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"capabilities": script.get("capabilities", {})}})
    elif method == "$/cancelRequest":
        t = pending.pop(p["id"], None)
        if t is not None:
            t[0].cancel()
            if not t[1].get("ignore_cancel"):
                send({"jsonrpc": "2.0", "id": p["id"], "error": {"code": -32800, "message": "cancelled"}})
    elif method == "shutdown":
        send({"jsonrpc": "2.0", "id": mid, "result": None})
    elif method == "exit":
        sys.exit(0)
    elif mid is not None:
        step = next_step(method)
        delay = step.get("delay", 0)
        if delay:
            timer = threading.Timer(delay, reply, (mid, step))
            pending[mid] = (timer, step)
            timer.start()
        else:
            reply(mid, step)
