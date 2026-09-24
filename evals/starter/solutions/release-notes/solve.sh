python3 - <<'PY'
import glob, re
out = {"feat": [], "fix": []}
for p in sorted(glob.glob("logs/*.log")):
    author = pr = None
    for line in open(p):
        if line.startswith("Author: "):
            author = line.split(": ", 1)[1].strip()
        elif line.startswith("PR: #"):
            pr = line.split("#", 1)[1].strip()
        m = re.match(r"    (feat|fix)\((api|cli)\): (.+)$", line.rstrip("\n"))
        if m and author != "bot-renovate":
            out[m.group(1)].append(f"- {m.group(3)} (#{pr})")
with open("RELEASE_NOTES.md", "w") as f:
    f.write("# Acme 4.2.0 — Codename Heron\n\n## Features\n\n")
    f.write("\n".join(sorted(out["feat"])) + "\n\n## Fixes\n\n")
    f.write("\n".join(sorted(out["fix"])) + "\n")
PY
