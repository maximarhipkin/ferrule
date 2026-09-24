"""sales-summary: summary.json totals non-refunded revenue per region, and names the top one."""
import csv
import json
import sys

want = {}
for row in csv.DictReader(open("sales.csv")):
    if row["status"] == "refunded":
        continue
    want[row["region"]] = want.get(row["region"], 0) + int(row["units"]) * float(row["unit_price"])
want = {k: round(v, 2) for k, v in want.items()}
try:
    got = json.load(open("summary.json"))
except (OSError, ValueError) as e:
    sys.exit(f"summary.json: {e}")
if set(got) != {"regions", "top_region"}:
    sys.exit(f"summary.json keys are {sorted(got)}, want ['regions', 'top_region']")
regions = got["regions"]
if set(regions) != set(want):
    sys.exit(f"regions {sorted(regions)}, want {sorted(want)}")
for k, v in want.items():
    if not isinstance(regions[k], (int, float)) or abs(regions[k] - v) > 0.011:
        sys.exit(f"region {k}: {regions[k]!r}, want {v}")
top = max(want, key=want.get)
if got["top_region"] != top:
    sys.exit(f"top_region {got['top_region']!r}, want {top!r}")
print("ok")
