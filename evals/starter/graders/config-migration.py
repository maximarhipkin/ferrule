"""config-migration: every services/*.ini follows the four rules given at the
start of the prompt, keeps its comments and other keys, and MIGRATION.md counts them."""
import os
import subprocess
import sys
import tempfile

here = os.path.dirname(os.path.abspath(__file__))
gen = os.path.join(here, "..", "fixtures", "config-migration", "gen.py")


def parse(path):
    sections, comments, cur = {}, [], None
    for line in open(path):
        line = line.strip()
        if line.startswith("#"):
            comments.append(line)
        elif line.startswith("[") and line.endswith("]"):
            cur = line[1:-1]
            sections.setdefault(cur, {})
        elif "=" in line:
            k, v = line.split("=", 1)
            sections.setdefault(cur, {})[k.strip()] = v.strip()
    return sections, comments


with tempfile.TemporaryDirectory() as orig:
    subprocess.run([sys.executable, gen], cwd=orig, check=True)
    names = sorted(os.listdir(os.path.join(orig, "services")))
    for name in names:
        old, old_comments = parse(os.path.join(orig, "services", name))
        path = os.path.join("services", name)
        if not os.path.exists(path):
            sys.exit(f"{path} is gone")
        new, new_comments = parse(path)
        o = dict(old["service"])
        want = {k: v for k, v in o.items() if k not in ("timeout", "host", "port", "legacy_mode")}
        want["timeout_ms"] = str(int(o["timeout"]) * 1000)
        want["url"] = f"http://{o['host']}:{o['port']}"
        want["owner"] = "platform-team"
        got = new.get("service", {})
        if got != want:
            diff = sorted(set(got.items()) ^ set(want.items()))[:4]
            sys.exit(f"{path} [service]: got {got}, want {want} (differs in {diff})")
        for sec in old:
            if sec != "service" and new.get(sec) != old[sec]:
                sys.exit(f"{path} [{sec}] changed: {new.get(sec)} (was {old[sec]}); only [service] migrates")
        missing = [c for c in old_comments if c not in new_comments]
        if missing:
            sys.exit(f"{path}: {len(missing)} comment line(s) lost, e.g. {missing[0][:80]!r}")
    try:
        m = open("MIGRATION.md").read().strip()
    except OSError as e:
        sys.exit(f"MIGRATION.md: {e}")
    if m != f"migrated: {len(names)} files":
        sys.exit(f"MIGRATION.md is {m!r}, want 'migrated: {len(names)} files'")
print("ok")
