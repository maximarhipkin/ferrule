"""todo-json-flag: --json prints the tasks as JSON; without it the output is unchanged."""
import json
import os
import subprocess
import sys

here = os.path.dirname(os.path.abspath(__file__))
orig = os.path.join(here, "..", "fixtures", "todo-json-flag", "todo.py")
want_json = [{"title": "Buy milk", "done": True},
             {"title": 'Call the "garage" about the brakes', "done": False},
             {"title": "Renew passport", "done": False}, {"title": "Pay rent", "done": True},
             {"title": "Book flights, then hotels", "done": False}]


def run(script, *args):
    env = {**os.environ, "PYTHONIOENCODING": "utf-8", "PYTHONDONTWRITEBYTECODE": "1"}
    return subprocess.run([sys.executable, script, *args], capture_output=True, text=True, env=env, timeout=30)


plain = run("todo.py")
if plain.returncode != 0:
    sys.exit(f"todo.py fails: {plain.stderr[-300:]}")
if plain.stdout != run(orig).stdout:
    sys.exit("the output without --json changed")
js = run("todo.py", "--json")
try:
    got = json.loads(js.stdout)
except ValueError:
    sys.exit(f"--json output isn't JSON: {js.stdout[:200]!r} {js.stderr[-200:]}")
if got != want_json:
    sys.exit(f"--json printed {got}, want {want_json}")
print("ok")
