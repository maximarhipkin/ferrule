cat > roman.py <<'PY'
VALS = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
        (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")]


def to_roman(n: int) -> str:
    if not isinstance(n, int) or not 1 <= n <= 3999:
        raise ValueError(f"out of range: {n!r}")
    out = ""
    for v, s in VALS:
        while n >= v:
            out += s
            n -= v
    return out


_TABLE = {to_roman(n): n for n in range(1, 4000)}


def from_roman(s: str) -> int:
    try:
        return _TABLE[s]
    except (KeyError, TypeError):
        raise ValueError(f"not a canonical Roman numeral: {s!r}") from None
PY
