cat > wordfreq.py <<'PY'
import collections
import re
import sys


def main(path, n):
    words = re.findall(r"[a-z']+", open(path, encoding="utf-8").read().lower())
    words = [w.strip("'") for w in words]
    counts = collections.Counter(w for w in words if w)
    for w, c in sorted(counts.items(), key=lambda kv: (-kv[1], kv[0]))[:n]:
        print(f"{w} {c}")


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]))
PY
