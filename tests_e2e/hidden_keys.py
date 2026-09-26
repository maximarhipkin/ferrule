"""Does the agent's read_file/write_file reach ferrule's saved keys when the
workspace contains the data dir? A mock model asks for it via tool calls.

Linux only, stdlib only. Runs target/debug/ferrule, or $FERRULE_BIN.
"""
import json, os, subprocess, sys, tempfile, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

BIN = os.environ.get("FERRULE_BIN") or os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "target", "debug", "ferrule")
PLAN = [
    ("read_file", {"path": ".local/share/ferrule/private/secrets.env"}),
    ("list_dir", {"path": ".local/share/ferrule/private"}),
    ("write_file", {"path": ".local/share/ferrule/private/secrets.env", "content": "EVIL=1\n"}),
    ("write_file", {"path": ".local/share/ferrule/proxy/keys/ca.key", "content": "planted"}),
    ("shell", {"command": "echo hi > top-level.txt && echo wrote-top"}),
    ("shell", {"command": "mkdir -p proj && echo x > proj/a.txt && echo wrote-sub"}),
]
results = []


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_GET(self):
        self.send_response(200); self.end_headers(); self.wfile.write(b'{"data":[{"id":"m"}]}')

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        msgs = body["messages"]
        tool_msgs = [m for m in msgs if m.get("role") == "tool"]
        if tool_msgs:
            results.append(tool_msgs[-1].get("content"))
        n = len(tool_msgs)
        if n < len(PLAN):
            name, args = PLAN[n]
            msg = {"role": "assistant", "content": None, "tool_calls": [
                {"id": f"c{n}", "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}]}
            finish = "tool_calls"
        else:
            msg = {"role": "assistant", "content": "done"}
            finish = "stop"
        out = json.dumps({"id": "x", "object": "chat.completion", "model": "m",
                          "choices": [{"index": 0, "message": msg, "finish_reason": finish}],
                          "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(out)))
        self.end_headers()
        self.wfile.write(out)


srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
home = tempfile.mkdtemp(prefix="ferrule-hidden-")
data = os.path.join(home, ".local/share/ferrule/private")
os.makedirs(data, mode=0o700)
with open(os.path.join(data, "secrets.env"), "w") as f:
    f.write("M_KEY=sk-SUPERSECRET\n")
os.chmod(os.path.join(data, "secrets.env"), 0o600)
cfg = os.path.join(home, ".config/ferrule/config.toml")
os.makedirs(os.path.dirname(cfg))
open(cfg, "w").write(f'default_provider = "m"\n[providers.m]\nbase_url = "http://127.0.0.1:{port}/v1"\n'
                     f'api_key_env = "M_KEY"\nmodel = "m"\n')
env = {"PATH": "/usr/bin:/bin", "HOME": home, "XDG_CONFIG_HOME": os.path.join(home, ".config"),
       "XDG_DATA_HOME": os.path.join(home, ".local/share"), "NO_PROXY": "localhost,127.0.0.1,::1"}
p = subprocess.run([BIN, "run", "go", "--workspace", home], env=env, capture_output=True, text=True, timeout=120)
srv.shutdown()
print("exit", p.returncode, p.stderr[-600:])
for (name, args), res in zip(PLAN, results):
    print(f"--- {name} {args}\n    → {str(res)[:220]!r}")
leak = any("SUPERSECRET" in str(r) for r in results)
tampered = "EVIL" in open(os.path.join(data, "secrets.env")).read()
# The proxy makes its own CA there (since M33 the egress policy starts it
# without secrets too), so what counts is the model's content in it.
ca_key = os.path.join(home, ".local/share/ferrule/proxy/keys/ca.key")
planted = os.path.exists(ca_key) and "planted" in open(ca_key).read()
print("LEAK:", leak, "| TAMPERED:", tampered, "| PLANTED CA KEY:", planted)
sys.exit(1 if (leak or tampered or planted) else 0)
