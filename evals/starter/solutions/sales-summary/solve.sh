python3 - <<'PY'
import csv, json
t = {}
for r in csv.DictReader(open("sales.csv")):
    if r["status"] != "refunded":
        t[r["region"]] = t.get(r["region"], 0) + int(r["units"]) * float(r["unit_price"])
t = {k: round(v, 2) for k, v in t.items()}
json.dump({"regions": t, "top_region": max(t, key=t.get)}, open("summary.json", "w"), indent=2)
PY
