cat > duration.py <<'PY'
import re

_UNITS = (("d", 86400), ("h", 3600), ("m", 60), ("s", 1))
_RE = re.compile(r"(?:(\d+(?:\.\d+)?)d)?\s*(?:(\d+(?:\.\d+)?)h)?\s*(?:(\d+(?:\.\d+)?)m)?\s*(?:(\d+(?:\.\d+)?)s)?")


def parse_duration(text: str) -> int:
    t = text.strip().lower()
    if re.fullmatch(r"\d+", t):
        return int(t)
    m = _RE.fullmatch(t)
    if not t or not m or not any(m.groups()):
        raise ValueError(f"not a duration: {text!r}")
    return int(round(sum(float(g) * s for g, (_, s) in zip(m.groups(), _UNITS) if g)))
PY
