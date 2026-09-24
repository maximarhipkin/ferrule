"""normalize-dates: iso.txt has the same dates in order, as YYYY-MM-DD (slash dates are day first)."""
import sys

want = ["2026-03-04", "2026-04-03", "2026-03-04", "2026-03-04", "2025-12-31", "2026-01-05",
        "2026-07-09", "2024-02-29", "2026-09-01", "2025-08-12"]
try:
    got = [l.strip() for l in open("iso.txt") if l.strip()]
except OSError as e:
    sys.exit(f"iso.txt: {e}")
if got != want:
    bad = [f"line {i + 1}: {g!r}, want {w!r}" for i, (g, w) in enumerate(zip(got, want)) if g != w]
    sys.exit(f"{len(got)} lines, want {len(want)}; " + "; ".join(bad[:3]))
print("ok")
