"""active-buyers: users with 3+ purchase events in March 2026 (UTC), sorted."""
import collections
import json
import sys

n = collections.Counter()
for line in open("events.jsonl"):
    if line.strip():
        e = json.loads(line)
        if e["type"] == "purchase" and e["ts"].startswith("2026-03"):
            n[e["user"]] += 1
want = sorted(u for u, c in n.items() if c >= 3)
try:
    got = [l.strip() for l in open("buyers.txt") if l.strip()]
except OSError as e:
    sys.exit(f"buyers.txt: {e}")
if got != want:
    sys.exit(f"buyers.txt has {len(got)} ids, want {len(want)}; "
             f"extra {sorted(set(got) - set(want))[:5]}, missing {sorted(set(want) - set(got))[:5]}"
             + ("; not sorted" if sorted(got) != got else ""))
print("ok")
