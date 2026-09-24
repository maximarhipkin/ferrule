"""fix-median: median() is right on even and odd lists, and the tests gained an even case."""
import ast
import subprocess
import sys

sys.path.insert(0, ".")
from stats import median  # noqa: E402

cases = {(1, 2, 3, 4): 2.5, (4, 1, 3, 2): 2.5, (5, 1, 3): 3, (7,): 7, (1, 3): 2, (2, 2, 4, 10): 3}
for xs, want in cases.items():
    got = median(list(xs))
    if got != want:
        sys.exit(f"median({list(xs)}) = {got!r}, want {want}")

tree = ast.parse(open("test_stats.py").read())
tests = [n.name for n in ast.walk(tree) if isinstance(n, ast.FunctionDef) and n.name.startswith("test")]
if len(tests) < 3:
    sys.exit(f"test_stats.py has {len(tests)} tests; an even-length median test was to be added")
r = subprocess.run([sys.executable, "-m", "unittest", "-q"], capture_output=True, text=True)
if r.returncode != 0:
    sys.exit("the tests fail:\n" + r.stderr[-800:])
print("ok")
