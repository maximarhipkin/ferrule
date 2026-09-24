"""rename-api: the three renames everywhere, API_VERSION, the CHANGELOG lines,
and the tests still pass."""
import glob
import re
import shutil
import subprocess
import sys

# A same-size edit within a second leaves a stale .pyc behind.
for d in glob.glob("**/__pycache__", recursive=True):
    shutil.rmtree(d, ignore_errors=True)
sys.dont_write_bytecode = True

old = ["get_price", "calc_tax", "apply_coupon"]
new = ["price_for", "tax_for", "apply_discount"]
for path in glob.glob("**/*.py", recursive=True):
    text = open(path).read()
    for o in old:
        if re.search(rf"\b{o}\b", text):
            sys.exit(f"{path} still says {o}")
sys.path.insert(0, ".")
import shop  # noqa: E402

for n in new:
    if not callable(getattr(shop, n, None)):
        sys.exit(f"shop.{n} is missing")
if shop.price_for("hook", 2) != 31.0 or shop.tax_for("mat") != 20.4 or shop.apply_discount("cover", 1, "VIP25") != 186.75:
    sys.exit("the renamed functions changed behaviour")
if getattr(shop, "API_VERSION", None) != "2.0":
    sys.exit(f"shop.API_VERSION is {getattr(shop, 'API_VERSION', None)!r}, want '2.0'")
try:
    log = [l.strip() for l in open("CHANGELOG.md") if l.strip()]
except OSError as e:
    sys.exit(f"CHANGELOG.md: {e}")
want = [f"- renamed {o} -> {n}" for o, n in zip(old, new)]
if [l for l in log if l.startswith("- ")] != want:
    sys.exit(f"CHANGELOG.md bullets are {log}, want {want}")
r = subprocess.run([sys.executable, "-m", "unittest", "-q"], capture_output=True, text=True)
if r.returncode != 0:
    sys.exit("the tests fail:\n" + r.stderr[-600:])
print("ok")
