"""invoice-tests: all of test_invoice.py passes, and the tests weren't edited."""
import glob
import hashlib
import os
import shutil
import subprocess
import sys

for d in glob.glob("**/__pycache__", recursive=True):
    shutil.rmtree(d, ignore_errors=True)
here = os.path.dirname(os.path.abspath(__file__))
orig = os.path.join(here, "..", "fixtures", "invoice-tests", "test_invoice.py")
digest = lambda p: hashlib.sha256(open(p, "rb").read()).hexdigest()  # noqa: E731
if not os.path.exists("test_invoice.py") or digest("test_invoice.py") != digest(orig):
    sys.exit("test_invoice.py was changed: fix the code, not the tests")
r = subprocess.run([sys.executable, "-m", "unittest", "-q"], capture_output=True, text=True,
                   env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
if r.returncode != 0:
    lines = [l for l in r.stderr.splitlines() if l.startswith(("FAIL:", "ERROR:", "AssertionError"))]
    sys.exit("tests fail: " + "; ".join(lines[:4]))
print("ok")
