#!/usr/bin/env python3
"""M44's memory measurements (docs/m44-managed-mode.md §6.1).

Standard library only, Linux only. It starts a fake OpenAI-compatible
model (its first reply is one `shell` tool call, its second is "done") and
a fake Telegram Bot API, then runs `ferrule gateway` on a temp config and
data dir, in managed mode (a policy file, the page up on boot), with
Telegram polling and the HTTP API on. It measures, from
/proc/<pid>/status:

  idle              VmRSS, 30 s after /healthz first answers
  turn peak         VmHWM after one turn through the HTTP API
  turn peak (tree)  the peak of VmRSS summed over the gateway and its
                    children, sampled every 50 ms during the turn

and prints them, and the binary's size, as a Markdown table.

    python3 scripts/m44_measure.py --bin target/release/ferrule
"""

import argparse
import json
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

PROXY_VARS = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]
OPENER = urllib.request.build_opener(urllib.request.ProxyHandler({}))
COMMAND = "echo m44 && head -c 20000000 /dev/zero | wc -c"


def serve(handler):
    server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    return server, server.server_address[1]


def send(h, code, body):
    data = json.dumps(body).encode()
    h.send_response(code)
    h.send_header("Content-Type", "application/json")
    h.send_header("Content-Length", str(len(data)))
    h.end_headers()
    h.wfile.write(data)


class Model(BaseHTTPRequestHandler):
    """Non-streaming. A conversation with no tool result yet gets the shell
    call; one that has one gets the text."""

    def log_message(self, *a):
        pass

    def do_GET(self):
        send(self, 200, {"data": []})

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        req = json.loads(self.rfile.read(n) or b"{}")
        done = any(m.get("role") == "tool" for m in req.get("messages", []))
        if done:
            msg, finish = {"role": "assistant", "content": "done"}, "stop"
        else:
            msg = {"role": "assistant", "content": None, "tool_calls": [{
                "id": "call_" + os.urandom(4).hex(), "type": "function",
                "function": {"name": "shell", "arguments": json.dumps({"command": COMMAND})}}]}
            finish = "tool_calls"
        send(self, 200, {
            "id": "m44", "object": "chat.completion", "model": "mock",
            "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5},
        })


class Telegram(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0) or 0)
        self.rfile.read(n)
        if "/getUpdates" in self.path:
            time.sleep(0.5)
            return send(self, 200, {"ok": True, "result": []})
        send(self, 200, {"ok": True, "result": {"message_id": 1}})

    do_GET = do_POST


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def get(url, headers=None, timeout=10):
    req = urllib.request.Request(url, headers=headers or {})
    try:
        with OPENER.open(req, timeout=timeout) as r:
            return r.status, r.read().decode()
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode()
    except OSError:
        return 0, ""


def status_kb(pid, field):
    try:
        for line in Path(f"/proc/{pid}/status").read_text().splitlines():
            if line.startswith(field + ":"):
                return int(line.split()[1])
    except (OSError, ValueError):
        pass
    return 0


def tree(pid):
    """The pid and every descendant, from /proc/*/stat's parent field."""
    kids = {}
    for p in Path("/proc").iterdir():
        if not p.name.isdigit():
            continue
        try:
            stat = (p / "stat").read_text()
            ppid = int(stat[stat.rindex(")") + 2:].split()[1])
        except (OSError, ValueError):
            continue
        kids.setdefault(ppid, []).append(int(p.name))
    out, todo = [], [pid]
    while todo:
        cur = todo.pop()
        out.append(cur)
        todo.extend(kids.get(cur, []))
    return out


def mib(kb):
    return f"{kb / 1024:.1f} MiB"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True, help="the ferrule binary")
    ap.add_argument("--settle", type=int, default=30, help="seconds to wait before the idle reading")
    ap.add_argument("--sandbox", default="os", choices=["os", "container"],
                    help="the policy's sandbox key (default os: Landlock and seccomp on the shell)")
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    args = ap.parse_args()
    ferrule = str(Path(args.bin).resolve())
    tmp = Path(tempfile.mkdtemp(prefix="ferrule-m44-measure-"))
    for d in ["work", "data", "home"]:
        (tmp / d).mkdir()
    env = {k: v for k, v in os.environ.items() if k not in PROXY_VARS}
    env["NO_PROXY"] = "localhost,127.0.0.1,::1"
    gw = None
    try:
        _, model_port = serve(Model)
        _, tg_port = serve(Telegram)
        http_port, dash_port = free_port(), free_port()
        config = tmp / "ferrule.toml"
        config.write_text(f'''default_provider = "mock"

[providers.mock]
base_url = "http://127.0.0.1:{model_port}/v1"
api_key_env = "FERRULE_M44_KEY"
model = "mock"

[agent]
stream = false

[gateway]
telegram_token_env = "FERRULE_M44_TG"
telegram_base_url = "http://127.0.0.1:{tg_port}"
telegram_allowed_chats = [42]

[gateway.http]
port = {http_port}

[skills]
enabled = false

''', encoding="utf-8")
        # Managed mode is what starts the page on boot, so /healthz answers
        # without anyone opening the dashboard from a chat.
        policy = tmp / "policy.toml"
        policy.write_text(f'reason = "m44 measurement"\nsandbox = "{args.sandbox}"\n', encoding="utf-8")
        env.update({
            "FERRULE_CONFIG": str(config),
            "FERRULE_DATA_DIR": str(tmp / "data"),
            "FERRULE_M44_KEY": "not-a-key",
            "FERRULE_M44_TG": "M44TOKEN",
            "FERRULE_MANAGED": "1",
            "FERRULE_POLICY": str(policy),
            "FERRULE_BOT_ID": "b_measure",
            "FERRULE_DASHBOARD_BIND": "127.0.0.1",
            "FERRULE_DASHBOARD_PORT": str(dash_port),
        })
        for var in ["HOME", "XDG_CONFIG_HOME", "XDG_DATA_HOME"]:
            env[var] = str(tmp / "home")

        made = subprocess.run([ferrule, "channels", "keys", "add", "m44"], env=env,
                              capture_output=True, text=True, cwd=tmp / "work")
        key = next((w for w in made.stdout.split() if w.startswith("frk_")), None)
        if not key:
            sys.exit(f"FAIL  no key from `channels keys add`: {made.stdout}{made.stderr}")

        log = open(tmp / "gateway.log", "w")
        gw = subprocess.Popen([ferrule, "gateway"], cwd=tmp / "work", env=env,
                              stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        # /healthz answers on the dashboard's port once the page is up.
        deadline, up = time.time() + 60, False
        while time.time() < deadline and gw.poll() is None:
            if get(f"http://127.0.0.1:{dash_port}/healthz")[0] in (200, 503) \
                    and get(f"http://127.0.0.1:{http_port}/")[0] != 0:
                up = True
                break
            time.sleep(0.2)
        if not up:
            sys.exit(f"FAIL  the gateway didn't come up in 60 s; see {tmp / 'gateway.log'}")
        print(f"gateway up (pid {gw.pid}); waiting {args.settle} s for the idle reading", file=sys.stderr)
        time.sleep(args.settle)
        idle = status_kb(gw.pid, "VmRSS")
        hwm_before = status_kb(gw.pid, "VmHWM")

        # Reset the high-water mark, so VmHWM below is the turn's peak.
        reset = False
        try:
            Path(f"/proc/{gw.pid}/clear_refs").write_text("5")
            reset = status_kb(gw.pid, "VmHWM") <= status_kb(gw.pid, "VmRSS") + 64
        except OSError:
            pass

        peak_tree, stop = [0], threading.Event()

        def sample():
            while not stop.is_set():
                total = sum(status_kb(p, "VmRSS") for p in tree(gw.pid))
                peak_tree[0] = max(peak_tree[0], total)
                time.sleep(0.05)

        sampler = threading.Thread(target=sample, daemon=True)
        sampler.start()
        req = urllib.request.Request(
            f"http://127.0.0.1:{http_port}/v1/messages", method="POST",
            data=json.dumps({"text": "run it"}).encode(),
            headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"})
        started = time.time()
        with OPENER.open(req, timeout=120) as r:
            answer = r.read().decode()
        took = time.time() - started
        stop.set()
        sampler.join()
        if "done" not in answer:
            sys.exit(f"FAIL  the turn didn't finish: {answer[:300]}")
        peak = status_kb(gw.pid, "VmHWM")
        after = status_kb(gw.pid, "VmRSS")

        binary = Path(ferrule).stat().st_size
        rows = [
            ("idle RSS (Telegram polling, HTTP API listening, no turn)", mib(idle)),
            ("gateway peak over one turn (VmHWM)" +
             ("" if reset else f", not reset, so the peak since start; was {mib(hwm_before)}"), mib(peak)),
            ("whole tree peak over one turn (gateway and the shell, 50 ms samples)", mib(peak_tree[0])),
            ("RSS after the turn", mib(after)),
            ("the turn took", f"{took:.2f} s"),
            ("release binary", f"{binary / 1024 / 1024:.1f} MiB ({binary} bytes)"),
        ]
        print("| measurement | value |\n|---|---|")
        for name, value in rows:
            print(f"| {name} | {value} |")
        print(f"\nsandbox: {'; '.join(l for l in Path(tmp / 'gateway.log').read_text().splitlines() if 'sandbox' in l.lower())[:300] or 'no line about it in the log'}")
    finally:
        if gw and gw.poll() is None:
            gw.terminate()
            try:
                gw.wait(15)
            except subprocess.TimeoutExpired:
                gw.kill()
        if args.keep:
            print(f"kept {tmp}", file=sys.stderr)
        else:
            shutil.rmtree(tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
