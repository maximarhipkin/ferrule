python3 - <<'PY'
import csv
rows = list(csv.reader(open("contacts.csv", newline="")))
seen, out = set(), [rows[0]]
for r in rows[1:]:
    e = r[1].strip().lower()
    if e not in seen:
        seen.add(e)
        out.append([r[0], e, r[2]])
csv.writer(open("unique.csv", "w", newline="")).writerows(out)
PY
