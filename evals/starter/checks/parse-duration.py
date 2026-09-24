"""parse-duration: the full format. The prompt gives one example; the rest is
found by running this check."""
import sys

sys.path.insert(0, ".")
try:
    from duration import parse_duration
except Exception as e:  # noqa: BLE001
    sys.exit(f"can't import parse_duration from duration.py: {e}")

ok = [("1h30m", 5400), ("45s", 45), ("2d", 172800), ("1h 30m", 5400), ("1H30M", 5400),
      ("1.5h", 5400), ("90", 90), ("1d2h3m4s", 93784), ("0s", 0), ("  10m ", 600)]
bad_inputs = ["", "5x", "h", "1h1h", "-5m", "1m30"]
fails = []
for text, want in ok:
    try:
        got = parse_duration(text)
    except Exception as e:  # noqa: BLE001
        got = f"<{type(e).__name__}: {e}>"
    if got != want:
        fails.append(f"parse_duration({text!r}) = {got!r}, want {want}")
for text in bad_inputs:
    try:
        got = parse_duration(text)
        fails.append(f"parse_duration({text!r}) = {got!r}, want ValueError")
    except ValueError:
        pass
    except Exception as e:  # noqa: BLE001
        fails.append(f"parse_duration({text!r}) raised {type(e).__name__}, want ValueError")
if fails:
    print("\n".join(fails))
    print("format: units d/h/m/s, case-insensitive, optional spaces between parts, decimals "
          "allowed, a bare number is seconds, each unit at most once and in d-h-m-s order; "
          "anything else raises ValueError. Returns an int.")
    sys.exit(1)
print("ok")
