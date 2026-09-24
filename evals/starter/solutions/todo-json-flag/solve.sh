python3 - <<'PY'
t = open("todo.py").read()
t = t.replace('import sys\n', 'import json\nimport sys\n', 1)
t = t.replace("def main(argv):\n", 'def main(argv):\n    if "--json" in argv:\n        print(json.dumps([{"title": t, "done": d} for t, d in load()]))\n        return\n')
open("todo.py", "w").write(t)
PY
