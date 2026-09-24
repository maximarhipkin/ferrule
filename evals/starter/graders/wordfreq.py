"""wordfreq: the top-N words with counts, case-insensitive, apostrophes kept, ties alphabetical."""
import os
import subprocess
import sys
import tempfile

text = ("Rock'n'roll is here. rock ROCK roll; Bob's bob BOB's, bob-bob! "
        "a b c a b a 'quoted' zeta zeta alpha alpha\n")
want = {3: "a 3\nbob 3\nalpha 2\n", 5: "a 3\nbob 3\nalpha 2\nb 2\nbob's 2\n"}
with tempfile.NamedTemporaryFile("w", suffix=".txt", delete=False) as f:
    f.write(text)
try:
    for n, w in want.items():
        r = subprocess.run([sys.executable, "wordfreq.py", f.name, str(n)], capture_output=True, text=True,
                           timeout=30, env={**os.environ, "PYTHONDONTWRITEBYTECODE": "1"})
        if r.returncode != 0:
            sys.exit(f"wordfreq.py FILE {n} failed: {r.stderr[-300:]}")
        if r.stdout.replace("\r\n", "\n").strip() != w.strip():
            sys.exit(f"wordfreq.py FILE {n} printed {r.stdout!r}, want {w!r}")
finally:
    os.unlink(f.name)
print("ok")
