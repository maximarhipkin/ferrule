python3 - <<'PY'
import csv
rows = sorted(csv.DictReader(open("countries.csv", encoding="utf-8")), key=lambda r: -int(r["population"]))
out = ["| Country | Capital | Population |", "|---|---|---:|"]
out += [f"| {r['country']} | {r['capital']} | {int(r['population']):,} |" for r in rows]
open("table.md", "w", encoding="utf-8").write("\n".join(out) + "\n")
PY
