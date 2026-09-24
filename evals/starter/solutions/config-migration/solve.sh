python3 - <<'PY'
import glob
files = sorted(glob.glob("services/*.ini"))
for p in files:
    lines = open(p).read().split("\n")
    out, sec, kv = [], None, {}
    for line in lines:
        s = line.strip()
        if s.startswith("[") and s.endswith("]"):
            sec = s[1:-1]
        if sec == "service" and "=" in s and not s.startswith("#"):
            k, v = [x.strip() for x in s.split("=", 1)]
            kv[k] = v
            if k == "timeout":
                out.append(f"timeout_ms = {int(v) * 1000}")
                continue
            if k == "host":
                out.append("url = HOSTPORT")
                continue
            if k in ("port", "legacy_mode"):
                continue
        if s == "[cache]":
            out.insert(len(out) - 1 if out and out[-1] == "" else len(out), "owner = platform-team")
        out.append(line)
    text = "\n".join(out).replace("url = HOSTPORT", f"url = http://{kv['host']}:{kv['port']}")
    open(p, "w").write(text)
open("MIGRATION.md", "w").write(f"migrated: {len(files)} files\n")
PY
