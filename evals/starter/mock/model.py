#!/usr/bin/env python3
"""A scripted stand-in for a model, for trying `ferrule eval` end to end
without an API key or a GPU (docs/eval.md). Standard library only.

    python3 evals/starter/mock/model.py [--port 8765]

It serves the OpenAI chat-completions API on http://127.0.0.1:PORT/v1, so it
plugs in like any provider:

    [providers.mock]
    base_url = "http://127.0.0.1:8765/v1"
    api_key_env = "MOCK_KEY"      # any non-empty value
    model = "mock"

It isn't a model: it plays the starter suite's reference solutions
(`solutions/<task>/`), with the same blind spots a real model has. It knows
the task from the workspace path in the system prompt, and per task:

  1. reads the files in `reads.txt`, one call each (the context-pressure
     tasks: this is what fills the window);
  2. if the request itself is no longer anywhere in what it was sent (the
     naive harness truncated it away), it says it's done without doing
     anything, since it no longer knows what to do;
  3. if there's a `first_try.sh`, it runs that plausible-but-wrong attempt
     first and says it's done;
  4. when told a check fails ("[ferrule] `…` fails"), it runs `solve.sh`;
  5. otherwise it runs `solve.sh` and says it's done.

So under the engineered harness (the request pinned through compaction, the
check run on finish) it passes everything; under the naive one it fails
exactly the context and verify tasks. That's the mechanism, not a measure of
any real model: for that, point the eval at one.
"""

import argparse
import json
import os
import re
import sys
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

SUITE_DIR = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
WORKSPACE = re.compile(r"Workspace: (.+?)\. ")
COMPACTION = "Summarize this agent session"
CHECK_FAILED = re.compile(r"^\[ferrule\] `.*` fails", re.S)

lock = threading.Lock()
state = {}  # workspace -> per-task progress


def text(m):
    c = m.get("content")
    if isinstance(c, list):
        return "".join(p.get("text", "") for p in c if isinstance(p, dict))
    return c or ""


def task_of(workspace):
    # <run dir>/<task>--<variant>[--<repeat>]/ws
    return os.path.basename(os.path.dirname(workspace)).split("--")[0]


def reply(content=None, calls=None):
    msg = {"role": "assistant", "content": content}
    if calls:
        msg["tool_calls"] = [
            {"id": f"call_{i}_{os.urandom(4).hex()}", "type": "function",
             "function": {"name": name, "arguments": json.dumps(args)}}
            for i, (name, args) in enumerate(calls)
        ]
    return msg, ("tool_calls" if calls else "stop")


def shell(script):
    return reply(calls=[("shell", {"command": f'sh "{script}"'})])


def decide(messages):
    """The next assistant message for this conversation."""
    first = text(messages[0]) if messages else ""
    if first.startswith(COMPACTION) or not any(m.get("role") == "system" for m in messages):
        return reply("Summary: files were being read and edited in the workspace; "
                     "continue with the request.")
    system = next(text(m) for m in messages if m.get("role") == "system")
    found = WORKSPACE.search(system)
    if not found:
        return reply("I don't know which workspace this is.")
    ws = found.group(1)
    task = task_of(ws)
    sol = os.path.join(SUITE_DIR, "solutions", task)
    visible = "\n".join(text(m) for m in messages if m.get("role") != "system")
    with lock:
        st = state.setdefault(ws, {"goal": None, "read": 0, "tried": False, "solved": False, "done": False})
        users = [text(m) for m in messages if m.get("role") == "user"]
        if st["goal"] is None and users:
            st["goal"] = users[0]
        last = messages[-1] if messages else {}

        if not os.path.isdir(sol):
            return reply(f"I have no solution for task {task!r}.")
        if last.get("role") == "user" and CHECK_FAILED.match(text(last)):
            if not st["solved"]:
                st["solved"] = True
                return shell(os.path.join(sol, "solve.sh"))
        if st["done"]:
            return reply(f"{task}: done." if st["solved"] or st["tried"] else f"{task}: nothing to do.")
        if last.get("role") == "tool" and (st["solved"] or st["tried"]):
            st["done"] = True
            return reply(f"{task}: done.")

        reads_file = os.path.join(sol, "reads.txt")
        reads = [l.strip() for l in open(reads_file)] if os.path.exists(reads_file) else []
        reads = [r for r in reads if r]
        if st["read"] < len(reads):
            path = reads[st["read"]]
            st["read"] += 1
            return reply(calls=[("read_file", {"path": path})])
        if st["goal"] and st["goal"] not in visible:
            st["done"] = True
            return reply("I've read the files, but I no longer have the request in front of me, "
                         "so there's nothing more I can do. Done.")
        first_try = os.path.join(sol, "first_try.sh")
        if os.path.exists(first_try) and not st["tried"]:
            st["tried"] = True
            return shell(first_try)
        if not st["solved"]:
            st["solved"] = True
            return shell(os.path.join(sol, "solve.sh"))
        st["done"] = True
        return reply(f"{task}: done.")


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def send(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        if self.path.rstrip("/").endswith("/models"):
            self.send(200, {"object": "list", "data": [{"id": "mock", "object": "model"}]})
        else:
            self.send(404, {"error": {"message": "not found"}})

    def do_POST(self):
        if not self.path.rstrip("/").endswith("/chat/completions"):
            return self.send(404, {"error": {"message": "not found"}})
        req = json.loads(self.rfile.read(int(self.headers.get("Content-Length", 0))) or b"{}")
        messages = req.get("messages", [])
        msg, finish = decide(messages)
        prompt_chars = sum(len(json.dumps(m)) for m in messages)
        self.send(200, {
            "id": "mock-" + os.urandom(4).hex(),
            "object": "chat.completion",
            "model": req.get("model", "mock"),
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": {"prompt_tokens": prompt_chars // 4 + 1,
                      "completion_tokens": len(json.dumps(msg)) // 4 + 1},
        })


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("--port", type=int, default=8765, help="0 picks a free one")
    args = ap.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"mock model listening on http://127.0.0.1:{server.server_address[1]}/v1", flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass


if __name__ == "__main__":
    sys.exit(main())
