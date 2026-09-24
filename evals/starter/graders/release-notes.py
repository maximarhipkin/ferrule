"""release-notes: RELEASE_NOTES.md follows the five rules given at the start of the prompt."""
import glob
import re
import sys

feats, fixes = [], []
for path in sorted(glob.glob("logs/*.log")):
    author = pr = None
    for line in open(path):
        if line.startswith("Author: "):
            author = line.split(": ", 1)[1].strip()
        elif line.startswith("PR: #"):
            pr = line.split("#", 1)[1].strip()
        m = re.match(r"    (feat|fix)\((api|cli)\): (.+)$", line.rstrip("\n"))
        if m and author != "bot-renovate":
            (feats if m.group(1) == "feat" else fixes).append(f"- {m.group(3)} (#{pr})")
if not feats or not fixes:
    sys.exit("the logs are missing: can't grade")

try:
    text = open("RELEASE_NOTES.md").read()
except OSError as e:
    sys.exit(f"RELEASE_NOTES.md: {e}")
lines = [l.rstrip() for l in text.splitlines()]
if not lines or lines[0] != "# Acme 4.2.0 — Codename Heron":
    sys.exit(f"first line is {lines[0] if lines else ''!r}, want '# Acme 4.2.0 — Codename Heron'")


def section(name):
    try:
        i = lines.index(f"## {name}")
    except ValueError:
        sys.exit(f"no '## {name}' section")
    out = []
    for l in lines[i + 1:]:
        if l.startswith("## "):
            break
        if l.startswith("- "):
            out.append(l)
    return out


for name, want in (("Features", sorted(feats)), ("Fixes", sorted(fixes))):
    got = section(name)
    if got != want:
        extra = [l for l in got if l not in want][:2]
        missing = [l for l in want if l not in got][:2]
        sys.exit(f"## {name}: {len(got)} bullets, want {len(want)}"
                 + (f"; unexpected {extra}" if extra else "")
                 + (f"; missing {missing}" if missing else "")
                 + ("; not sorted" if not extra and not missing else ""))
print("ok")
