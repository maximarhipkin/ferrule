cat > roman.py <<'PY'
VALS = [(1000, "M"), (900, "CM"), (500, "D"), (400, "CD"), (100, "C"), (90, "XC"),
        (50, "L"), (40, "XL"), (10, "X"), (9, "IX"), (5, "V"), (4, "IV"), (1, "I")]


def to_roman(n: int) -> str:
    out = ""
    for v, s in VALS:
        while n >= v:
            out += s
            n -= v
    return out


def from_roman(s: str) -> int:
    d = {"I": 1, "V": 5, "X": 10, "L": 50, "C": 100, "D": 500, "M": 1000}
    total = 0
    for i, c in enumerate(s):
        v = d[c]
        total += -v if i + 1 < len(s) and d[s[i + 1]] > v else v
    return total
PY
