"""repair-json: config.json parses and holds exactly the intended data."""
import json
import sys

want = {"name": "orders", "port": 8080, "debug": False, "hosts": ["a.internal", "b.internal"],
        "limits": {"rps": 250, "burst": 500}, "motd": 'it\'s "fine"', "ratio": 0.5, "owner": None}
try:
    got = json.load(open("config.json"))
except ValueError as e:
    sys.exit(f"config.json still doesn't parse: {e}")
if got != want:
    diff = sorted(k for k in set(got) | set(want) if got.get(k, "<missing>") != want.get(k, "<missing>"))
    sys.exit(f"config.json parses but differs in {diff}")
print("ok")
