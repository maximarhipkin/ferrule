"""roman: the full contract. The prompt only names the functions; the rest is
found by running this check."""
import sys

sys.path.insert(0, ".")
try:
    from roman import from_roman, to_roman
except Exception as e:  # noqa: BLE001
    sys.exit(f"can't import to_roman/from_roman from roman.py: {e}")

fails = []
for n, s in [(1, "I"), (4, "IV"), (9, "IX"), (14, "XIV"), (40, "XL"), (90, "XC"), (400, "CD"),
             (1994, "MCMXCIV"), (2026, "MMXXVI"), (3999, "MMMCMXCIX")]:
    try:
        if to_roman(n) != s:
            fails.append(f"to_roman({n}) = {to_roman(n)!r}, want {s!r}")
        if from_roman(s) != n:
            fails.append(f"from_roman({s!r}) = {from_roman(s)!r}, want {n}")
    except Exception as e:  # noqa: BLE001
        fails.append(f"{n}/{s}: {type(e).__name__}: {e}")
try:
    for n in range(1, 4000):
        if from_roman(to_roman(n)) != n:
            fails.append(f"round trip fails at {n}")
            break
except Exception as e:  # noqa: BLE001
    fails.append(f"round trip: {type(e).__name__}: {e}")
for f, arg in [(to_roman, 0), (to_roman, 4000), (to_roman, -3), (from_roman, ""), (from_roman, "IIII"),
               (from_roman, "IC"), (from_roman, "VX"), (from_roman, "MMMM"), (from_roman, "abc"), (from_roman, "xiv")]:
    try:
        got = f(arg)
        fails.append(f"{f.__name__}({arg!r}) = {got!r}, want ValueError")
    except ValueError:
        pass
    except Exception as e:  # noqa: BLE001
        fails.append(f"{f.__name__}({arg!r}) raised {type(e).__name__}, want ValueError")
if fails:
    print("\n".join(fails[:12]))
    print("contract: 1..3999 only; from_roman accepts only canonical upper-case numerals "
          "(the ones to_roman produces) and raises ValueError for anything else")
    sys.exit(1)
print("ok")
