python3 - <<'PY'
src = open("stats.py").read()
src = src.replace("    return s[len(s) // 2]\n", """    n = len(s)
    if n % 2:
        return s[n // 2]
    return (s[n // 2 - 1] + s[n // 2]) / 2
""")
open("stats.py", "w").write(src)
t = open("test_stats.py").read()
t = t.replace("\n\nif __name__", """
    def test_median_even(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)


if __name__""", 1)
open("test_stats.py", "w").write(t)
PY
