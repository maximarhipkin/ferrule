"""git-fix-commit: the typo is fixed in a new commit with the exact message, and the tree is clean."""
import subprocess
import sys


def git(*args):
    r = subprocess.run(["git", "-c", "core.hooksPath=/dev/null", *args], capture_output=True, text=True)
    return r.returncode, r.stdout.strip()


code, count = git("rev-list", "--count", "HEAD")
if code != 0 or count != "2":
    sys.exit(f"want exactly one new commit on top of the fixture's, the history has {count or 'none'}")
_, msg = git("log", "-1", "--format=%s")
if msg != "Fix greeting typo":
    sys.exit(f"the commit message is {msg!r}, want 'Fix greeting typo'")
_, status = git("status", "--porcelain")
if status:
    sys.exit(f"the working tree isn't clean:\n{status}")
_, shown = git("show", "HEAD:greet.py")
if 'Hello, {name}! Welcome back.' not in shown:
    sys.exit("the committed greet.py doesn't say 'Hello, {name}! Welcome back.'")
_, files = git("show", "--name-only", "--format=", "HEAD")
if files.split() != ["greet.py"]:
    sys.exit(f"the commit touches {files.split()}, want only greet.py")
print("ok")
