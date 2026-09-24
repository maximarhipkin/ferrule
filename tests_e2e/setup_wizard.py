"""Drive `ferrule setup` in a real pty against a mock provider + Telegram.

Linux only; needs `pip install pexpect`. Runs target/debug/ferrule, or
$FERRULE_BIN: `cargo build -p ferrule-cli && python3 tests_e2e/setup_wizard.py`.
"""
import json, os, re, stat, subprocess, sys, tempfile, threading, time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import pexpect

BIN = os.environ.get("FERRULE_BIN") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "ferrule")
GOOD_TG = "123456789:AAHfiqksKZ8WmR2zSjiQ7_v4TMAKdiHm9T0"
BAD_TG = "123456789:BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB"
calls = []
state = {"updates_served": False}


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def reply(self, code, body):
        data = json.dumps(body).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def do_GET(self):
        calls.append(("GET", self.path, self.headers.get("authorization")))
        if self.path == "/v1/models":
            if self.headers.get("authorization") == "Bearer sk-good":
                return self.reply(200, {"data": [{"id": "gpt-5.2"}, {"id": "gpt-5-mini"}]})
            return self.reply(401, {"error": "bad key"})
        self.reply(404, {})

    def do_POST(self):
        n = int(self.headers.get("content-length") or 0)
        body = json.loads(self.rfile.read(n) or b"{}")
        m = re.match(r"^/bot([^/]+)/(\w+)$", self.path)
        if not m:
            return self.reply(404, {})
        token, method = m.groups()
        calls.append(("POST", method, body))
        if token != GOOD_TG:
            return self.reply(401, {"ok": False, "description": "Unauthorized"})
        if method == "getMe":
            return self.reply(200, {"ok": True, "result": {"username": "test_bot"}})
        if method == "getWebhookInfo":
            return self.reply(200, {"ok": True, "result": {"url": ""}})
        if method == "getUpdates":
            if not state["updates_served"]:
                state["updates_served"] = True
                return self.reply(200, {"ok": True, "result": [{"update_id": 500, "message": {
                    "message_id": 1, "text": "hi",
                    "chat": {"id": 42, "type": "private", "first_name": "Max", "username": "maxim"}}}]})
            time.sleep(min(body.get("timeout", 0), 1))
            return self.reply(200, {"ok": True, "result": []})
        if method == "sendMessage":
            return self.reply(200, {"ok": True, "result": {"message_id": 2}})
        self.reply(400, {"ok": False, "description": "unknown method"})


srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()

tmp = tempfile.mkdtemp(prefix="ferrule-e2e-")
cfg_path = os.path.join(tmp, "cfg", "config.toml")
os.makedirs(os.path.dirname(cfg_path))
with open(cfg_path, "w") as f:
    f.write(f'[gateway]\ntelegram_base_url = "http://127.0.0.1:{port}"\n')
env = {
    "PATH": "/usr/local/bin:/usr/bin:/bin",
    "HOME": tmp,
    "XDG_CONFIG_HOME": os.path.join(tmp, ".config"),
    "XDG_DATA_HOME": os.path.join(tmp, ".local/share"),
    "FERRULE_CONFIG": cfg_path,
    "NO_PROXY": "localhost,127.0.0.1,::1",
    "TERM": "xterm-256color",
    "LANG": "C.UTF-8",
}
secrets_file = os.path.join(tmp, ".local/share/ferrule/private/secrets.env")
UP, DOWN, ESC, CTRL_C = "\x1b[A", "\x1b[B", "\x1b", "\x03"
failures = []


def check(name, cond, detail=""):
    print(("PASS " if cond else "FAIL ") + name + (f"  [{detail}]" if detail and not cond else ""))
    if not cond:
        failures.append(name)


def spawn(args=("setup",)):
    log = open(os.path.join(tmp, f"pty-{len(os.listdir(tmp))}.log"), "w")
    c = pexpect.spawn(BIN, list(args), env=env, encoding="utf-8", timeout=20, dimensions=(50, 200))
    c.logfile_read = log
    return c


def step(c, pattern, send=None, pause=0.15):
    c.expect(pattern)
    if send is not None:
        time.sleep(pause)
        c.send(send)


# ── Run 1: the guided first run ──────────────────────────────────────
c = spawn()
step(c, "Which model provider", "Another")
step(c, "Another OpenAI", "\r")
step(c, "A short name for it", "Bad Name\r")
step(c, "lowercase letters, digits", "\x7f" * 8 + "local\r")
step(c, r"Base URL", f"http://127.0.0.1:{port}/v1\r")
step(c, "Name to save its key under", "\r")
step(c, "local API key", "sk-bad\r")
step(c, "rejected", None)
step(c, "Try another key", "y\r")
step(c, "local API key", "sk-good\r")
step(c, r"the key works . 2 models available", None)
step(c, "Model", "5.2")
time.sleep(0.3)
c.send("\r")
step(c, r"saved `local` . gpt-5.2", None)
step(c, "Connect a Telegram bot", "y\r")
step(c, "Bot token", "notatoken\r")
step(c, "that isn't a bot token", "\x7f" * 9 + BAD_TG + "\r")
step(c, "Telegram rejected the token", None)
step(c, "Try another token", "y\r")
step(c, "Bot token", GOOD_TG + "\r")
step(c, "connected to @test_bot", None)
step(c, r"Message from Max \(@maxim\) \(chat 42\)", "y\r")
step(c, "allowed Max", None)
step(c, "Wait for another chat", "n\r")
step(c, "Add a token now", "y\r")
step(c, "A token for", "Something")
time.sleep(0.3)
c.send("\r")
step(c, "Variable name commands will use", "MY_TOKEN\r")
step(c, "Hosts it may be sent to", "https://x.com\r")
step(c, "just the host name", "\x7f" * 13 + "api.example.com, *.Example.com\r")
step(c, "MY_TOKEN value", "tok-1\r")
step(c, r"MY_TOKEN → api.example.com, \*.example.com", None)
step(c, "Add another", "n\r")
step(c, "Use the recommended one", "y\r")
i = c.expect([r"sandbox works here . [^\r]+", "commands will run unsandboxed"])
check("sandbox reported", True)
print("  sandbox:", c.match.group(0))
# The browser is offered only when this machine has Chrome and agent-browser.
if c.expect([r"Let the agent use [^\r]+\?", "No Chrome or Chromium found|isn't ready"]) == 0:
    time.sleep(0.15)
    c.send("n\r")
step(c, "Add an MCP server now", "n\r")
i = c.expect(["No background service here", "Run it in the background now"])
if i == 1:
    c.send("n\r")
step(c, "All set", None)
c.expect(pexpect.EOF)
c.close()
check("run 1 exits 0", c.exitstatus == 0, str(c.exitstatus))

text = open(cfg_path).read()
print("---- config.toml ----\n" + text + "---------------------")
import tomllib
cfg = tomllib.loads(text)
check("header comment", text.startswith("#"))
check("provider local", cfg.get("providers", {}).get("local", {}) == {
    "base_url": f"http://127.0.0.1:{port}/v1", "api_key_env": "LOCAL_API_KEY",
    "model": "gpt-5.2", "profile": "generic"}, str(cfg.get("providers")))
check("default provider", cfg.get("default_provider") == "local")
check("telegram base url kept", cfg["gateway"].get("telegram_base_url") == f"http://127.0.0.1:{port}")
check("telegram token env", cfg["gateway"].get("telegram_token_env") == "TELEGRAM_BOT_TOKEN")
check("allowed chats", cfg["gateway"].get("telegram_allowed_chats") == [42])
check("secret hosts", cfg.get("secrets", {}).get("MY_TOKEN") == ["api.example.com", "*.example.com"], str(cfg.get("secrets")))
check("sandbox", cfg.get("sandbox") == {"mode": "workspace-write", "network": True}, str(cfg.get("sandbox")))
sec = open(secrets_file).read()
check("secrets file keys", all(k in sec for k in ["LOCAL_API_KEY=", "TELEGRAM_BOT_TOKEN=", "MY_TOKEN="]), sec.replace("\n", " | ")[:0])
check("secrets not in config", "sk-good" not in text and GOOD_TG not in text and "tok-1" not in text)
check("secrets values", "sk-good" in sec and GOOD_TG in sec and "tok-1" in sec and "sk-bad" not in sec and BAD_TG not in sec)
check("secrets file 0600", stat.S_IMODE(os.stat(secrets_file).st_mode) == 0o600)
check("secrets dir 0700", stat.S_IMODE(os.stat(os.path.dirname(secrets_file)).st_mode) == 0o700)
sent = [b for (m, meth, b) in calls if m == "POST" and meth == "sendMessage"]
check("welcome sent to chat 42", len(sent) == 1 and sent[0].get("chat_id") == 42, str(sent))
acks = [b.get("offset") for (m, meth, b) in calls if m == "POST" and meth == "getUpdates"]
check("updates acked with offset 501", 501 in acks, str(acks))

# ── Run 2: the menu; change the model, comments survive ──────────────
before = open(cfg_path).read().replace(
    'profile = "generic"', 'profile = "generic"\n# my own note\nprice_input_per_mtok = 1.25 # hand-set', 1)
open(cfg_path, "w").write(before)
tomllib.loads(before)
c = spawn()
step(c, "What do you want to change", None)
c.expect(r"local . gpt-5.2")
time.sleep(0.2)
c.send("\r")  # Model provider
step(c, "Which provider", "\r")  # local
step(c, "local:", "\r")  # Change the model
step(c, "Model", "mini")
time.sleep(0.3)
c.send("\r")
step(c, r"gpt-5-mini", None)
step(c, "What do you want to change", None)
time.sleep(0.3)
c.send(ESC)
step(c, "All set", None)
c.expect(pexpect.EOF)
c.close()
check("run 2 exits 0", c.exitstatus == 0, str(c.exitstatus))
after = open(cfg_path).read()
check("model changed", tomllib.loads(after)["providers"]["local"]["model"] == "gpt-5-mini")
check("comments kept", "# my own note" in after and "# hand-set" in after, after)
check("price kept", tomllib.loads(after)["providers"]["local"].get("price_input_per_mtok") == 1.25)

# ── Run 3: Ctrl-C at the menu ────────────────────────────────────────
c = spawn()
step(c, "What do you want to change", None)
time.sleep(0.3)
c.send(CTRL_C)
step(c, "Setup stopped", None)
c.expect(pexpect.EOF)
c.close()
check("ctrl-c exits 0", c.exitstatus == 0, str(c.exitstatus))
check("ctrl-c leaves config", open(cfg_path).read() == after)

# ── No terminal ──────────────────────────────────────────────────────
p = subprocess.run(["setsid", BIN, "setup"], env=env, stdin=subprocess.DEVNULL, capture_output=True, text=True)
print("no-tty:", p.returncode, (p.stderr or p.stdout).strip()[:200])
check("no tty refuses", p.returncode != 0 and "terminal" in (p.stderr + p.stdout))

# ── doctor, config path, sandbox ─────────────────────────────────────
for args in (["doctor"], ["doctor", "--offline"], ["config", "path"], ["sandbox"]):
    p = subprocess.run([BIN, *args], env=env, capture_output=True, text=True, cwd=tmp)
    print(f"---- ferrule {' '.join(args)} → exit {p.returncode} ----\n{p.stdout}{p.stderr}")
    if args == ["doctor"]:
        check("doctor exit 0", p.returncode == 0)
        check("doctor never prints keys", "sk-good" not in p.stdout + p.stderr and GOOD_TG not in p.stdout + p.stderr)

# Broken default key → doctor fails.
env2 = dict(env, LOCAL_API_KEY="sk-wrong")
p = subprocess.run([BIN, "doctor"], env=env2, capture_output=True, text=True, cwd=tmp)
check("doctor fails on a rejected default key", p.returncode == 1, p.stdout[-300:])
print(p.stdout)

srv.shutdown()
print("tmp:", tmp)
print("FAILURES:", failures or "none")
sys.exit(1 if failures else 0)
