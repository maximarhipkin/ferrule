#!/usr/bin/env python3
"""The dashboard's smoke test (docs/m24-dashboard-2.md §4), steps 2-10.

`dashboard-smoke.sh` and `dashboard-smoke.ps1` build ferrule and run this.
Standard library only. It starts the starter suite's mock model and a fake
Telegram Bot API, runs `ferrule gateway` on a temp config and data dir,
asks for `/dashboard` from the owner's chat, signs in with the link, opens
it through the cloudflared tunnel when cloudflared is installed, fetches
the live OpenRouter catalog once, then closes the dashboard and cleans up.
It never reads or writes the owner's own config or data dir, and needs no
API key or bot token.

    python3 scripts/dashboard_smoke.py --bin target/release/ferrule
"""

import argparse
import json
import os
import queue
import re
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OWNER = 42
PROXY_VARS = ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy", "all_proxy"]
results = []


def step(name, status, detail=""):
    results.append((name, status))
    line = f"{status:<4}  {name}"
    if detail:
        line += f" — {detail}"
    print(line, flush=True)


# ---- the fake Telegram Bot API -------------------------------------------

class Telegram:
    def __init__(self):
        self.updates = queue.Queue()
        self.sent = []
        self.next = 0
        self.lock = threading.Lock()
        tg = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                n = int(self.headers.get("Content-Length", 0) or 0)
                raw = self.rfile.read(n) if n else b""
                if "/getUpdates" in self.path:
                    out = []
                    try:
                        out.append(tg.updates.get(timeout=0.5))
                        while True:
                            out.append(tg.updates.get_nowait())
                    except queue.Empty:
                        pass
                    body = {"ok": True, "result": out}
                else:
                    try:
                        msg = json.loads(raw or b"{}")
                    except ValueError:
                        msg = {}
                    with tg.lock:
                        tg.sent.append(msg)
                        body = {"ok": True, "result": {"message_id": 1000 + len(tg.sent)}}
                data = json.dumps(body).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            do_GET = do_POST

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def say(self, text):
        self.next += 1
        self.updates.put({
            "update_id": self.next,
            "message": {"message_id": self.next, "chat": {"id": OWNER, "type": "private"},
                        "from": {"id": OWNER, "username": "owner"},
                        "text": text, "date": int(time.time())},
        })

    def wait_for(self, needle, since, secs):
        deadline = time.time() + secs
        while time.time() < deadline:
            with self.lock:
                for i, m in enumerate(self.sent[since:], since):
                    if str(m.get("chat_id")) == str(OWNER) and needle in str(m.get("text", "")):
                        return i + 1, m["text"]
            time.sleep(0.1)
        return None, None


# ---- HTTP -----------------------------------------------------------------

LOCAL = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def http(url, method="GET", headers=None, body=None, opener=LOCAL, timeout=15):
    data = json.dumps(body).encode() if body is not None else None
    req = urllib.request.Request(url, data=data, method=method, headers=headers or {})
    if data is not None:
        req.add_header("Content-Type", "application/json")
    try:
        with opener.open(req, timeout=timeout) as r:
            return r.status, dict(r.headers), r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read().decode("utf-8", "replace")


def wait_file(path, secs, proc):
    deadline = time.time() + secs
    while time.time() < deadline:
        if path.exists() and path.stat().st_size > 0:
            return True
        if proc.poll() is not None:
            return False
        time.sleep(0.2)
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", required=True, help="the ferrule binary")
    ap.add_argument("--keep", action="store_true", help="keep the temp dir")
    args = ap.parse_args()
    ferrule = str(Path(args.bin).resolve())
    tmp = Path(tempfile.mkdtemp(prefix="ferrule-dashboard-smoke-"))
    for d in ["work", "data", "home"]:
        (tmp / d).mkdir()
    procs = []
    local_env = {k: v for k, v in os.environ.items() if k not in PROXY_VARS}
    local_env["NO_PROXY"] = "localhost,127.0.0.1,::1"
    try:
        # 2. The mock model.
        mock = subprocess.Popen(
            [sys.executable, str(ROOT / "evals/starter/mock/model.py"), "--port", "0"],
            stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True, env=local_env)
        procs.append(mock)
        first = mock.stdout.readline()
        m = re.search(r"(http://127\.0\.0\.1:\d+/v1)", first)
        if not m:
            step("mock model", "FAIL", f"it said {first!r}")
            return
        step("mock model", "PASS", m.group(1))
        model_url = m.group(1)

        # 3. The fake Telegram.
        tg = Telegram()
        step("mock telegram", "PASS", tg.url)

        # 4. The temp config.
        cloudflared = shutil.which("cloudflared")
        config = tmp / "ferrule.toml"
        config.write_text(f'''default_provider = "mock"

[providers.mock]
base_url = "{model_url}"
api_key_env = "FERRULE_SMOKE_KEY"
model = "mock"

[gateway]
telegram_token_env = "FERRULE_SMOKE_TG"
telegram_base_url = "{tg.url}"
telegram_allowed_chats = [{OWNER}]

[trust]
owner_chat = {OWNER}

[dashboard]
remote = "{"tunnel" if cloudflared else "off"}"

[skills]
enabled = false

[sandbox]
mode = "off"
''', encoding="utf-8")
        env = dict(local_env)
        env.update({
            "FERRULE_CONFIG": str(config),
            "FERRULE_DATA_DIR": str(tmp / "data"),
            "FERRULE_SMOKE_KEY": "not-a-key",
            "FERRULE_SMOKE_TG": "SMOKETOKEN",
        })
        for var in ["HOME", "USERPROFILE", "XDG_CONFIG_HOME", "XDG_DATA_HOME", "APPDATA", "LOCALAPPDATA"]:
            env[var] = str(tmp / "home")
        step("temp config", "PASS", str(config))

        # 5. The gateway.
        log = open(tmp / "gateway.log", "w")
        gw = subprocess.Popen([ferrule, "gateway"], cwd=tmp / "work", env=env,
                              stdin=subprocess.DEVNULL, stdout=log, stderr=subprocess.STDOUT)
        procs.append(gw)
        marker = tmp / "data/gateway/dashboard.json"
        if not wait_file(marker, 60, gw):
            step("gateway", "FAIL", f"no {marker.name} in 60 s; see {tmp / 'gateway.log'}")
            args.keep = True
            return
        port = json.loads(marker.read_text())["port"]
        step("gateway", "PASS", f"the page on 127.0.0.1:{port}")

        # 6. The link.
        tg.say("/dashboard")
        n, said = tg.wait_for("Dashboard: ", 0, 90 if cloudflared else 30)
        if not said:
            step("/dashboard link", "FAIL", f"no link came; sent: {tg.sent}")
            return
        link = re.search(r"Dashboard: (\S+)", said).group(1)
        base, _, token = link.partition("#")
        base = base.rsplit("/login", 1)[0]
        tunnel = "trycloudflare.com" in base
        step("/dashboard link", "PASS", "a tunnel link" if tunnel else "a local link")

        # 7. The tunnel: a tunnel link only signs in on the tunnel's host,
        # so the page is fetched and the login goes through it.
        web = urllib.request.build_opener()
        local = f"http://127.0.0.1:{port}"
        if not cloudflared:
            step("tunnel", "SKIP", "install cloudflared to test the tunnel")
            site, opener = local, LOCAL
        elif not tunnel:
            step("tunnel", "FAIL", f"cloudflared is installed but the link is local; see {tmp / 'gateway.log'}")
            args.keep = True
            return
        else:
            site, opener = base, web
            deadline, got = time.time() + 60, None
            while time.time() < deadline:
                try:
                    got = http(base + "/", opener=web)
                    if got[0] == 200:
                        break
                except OSError as e:
                    got = (0, {}, str(e))
                time.sleep(2)
            if got and got[0] == 200 and "app.js" in got[2]:
                step("tunnel", "PASS", f"the page through {base}")
            else:
                step("tunnel", "FAIL", f"{base}: {got[0] if got else '-'} {got[2][:120] if got else ''}")
                return

        # 8. Log in with the link's token.
        s, h, body = http(site + "/api/login", "POST", {"Origin": site}, {"token": token}, opener)
        cookie = next((v for k, v in h.items() if k.lower() == "set-cookie"), "")
        csrf = json.loads(body).get("csrf") if s == 200 else None
        if s != 200 or "HttpOnly" not in cookie or not csrf:
            step("login", "FAIL", f"{s} {body[:200]}")
            return
        jar = {"Cookie": cookie.split(";")[0]}
        s1, _, page = http(site + "/", headers=jar, opener=opener)
        s2, _, health = http(site + "/api/health", headers=jar, opener=opener)
        s3, _, _ = http(site + "/api/health", opener=opener)
        if s1 == 200 and "app.js" in page and s2 == 200 and s3 == 401:
            step("login", "PASS", "an HttpOnly cookie and a CSRF token; /api/health answers it and refuses without it")
        else:
            step("login", "FAIL", f"page {s1}, health {s2} {health[:120]}, anonymous {s3}")

        # 9. The live catalog, once.
        cenv = dict(env)
        for k in PROXY_VARS:
            if k in os.environ:
                cenv[k] = os.environ[k]
        out = subprocess.run([ferrule, "model", "catalog", "--json", "--refresh"], env=cenv,
                             cwd=tmp / "work", capture_output=True, text=True, timeout=90)
        try:
            cat = json.loads(out.stdout)
            live = [x for x in cat["sources"] if x["from"] == "live" and x["models"] > 0
                    and "openrouter" in x["source"].lower()]
            if live and cat["rows"]:
                step("catalog", "PASS", f"{live[0]['models']} models from {live[0]['source']}")
            else:
                why = [f"{x['source']}: {x['from']}, {x['error'] or 'no models'}" for x in cat["sources"]
                       if "openrouter" in x["source"].lower()]
                step("catalog", "FAIL", "; ".join(why) or "no openrouter source")
        except (ValueError, KeyError) as e:
            step("catalog", "FAIL", f"{e}: {out.stderr.strip()[:300]}")

        # 10. Close it.
        tg.say("/dashboard off")
        _, closed = tg.wait_for("Dashboard closed", n or 0, 30)
        s, _, _ = http(site + "/api/health", headers=jar, opener=opener)
        if closed and s == 401:
            step("/dashboard off", "PASS", "the session is revoked")
        else:
            step("/dashboard off", "FAIL", f"said {closed!r}, the session answers {s}")
    finally:
        for p in reversed(procs):
            p.terminate()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
        if args.keep:
            print(f"kept {tmp}")
        else:
            shutil.rmtree(tmp, ignore_errors=True)
        failed = [n for n, s in results if s == "FAIL"]
        print(f"\n{sum(s == 'PASS' for _, s in results)} passed, {len(failed)} failed, "
              f"{sum(s == 'SKIP' for _, s in results)} skipped")
        sys.exit(1 if failed or not results else 0)


if __name__ == "__main__":
    main()
