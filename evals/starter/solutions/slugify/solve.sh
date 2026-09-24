cat > slug.py <<'PY'
import re
import unicodedata


def slugify(title: str) -> str:
    s = unicodedata.normalize("NFKD", title).encode("ascii", "ignore").decode()
    s = s.lower().replace("'", "").replace("&", " and ")
    words = re.findall(r"[a-z0-9]+", s)
    out = ""
    for w in words:
        nxt = f"{out}-{w}" if out else w
        if len(nxt) > 60:
            return out or w[:60]
        out = nxt
    return out
PY
