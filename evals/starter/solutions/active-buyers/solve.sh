python3 - <<'PY'
import collections, json
n = collections.Counter()
for line in open("events.jsonl"):
    if line.strip():
        e = json.loads(line)
        if e["type"] == "purchase" and e["ts"].startswith("2026-03"):
            n[e["user"]] += 1
open("buyers.txt", "w").write("".join(u + "\n" for u in sorted(u for u, c in n.items() if c >= 3)))
PY
