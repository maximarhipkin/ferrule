"""inventory-merge: inventory.csv merges the dumps by the four rules given at the start of the prompt."""
import csv
import glob
import json
import sys

qty, price, name, dead = {}, {}, {}, set()
for path in glob.glob("warehouses/*.json"):
    for r in json.load(open(path))["records"]:
        s = r["sku"]
        if r.get("discontinued"):
            dead.add(s)
        qty[s] = qty.get(s, 0) + r["qty"]
        price[s] = min(price.get(s, r["price"]), r["price"])
        name[s] = r["name"]
if not qty:
    sys.exit("the dumps are missing: can't grade")
want = [["sku", "name", "qty", "price"]] + [
    [s, name[s], str(qty[s]), f"{price[s]:.2f}"] for s in sorted(qty) if s not in dead]
try:
    got = [r for r in csv.reader(open("inventory.csv")) if r]
except OSError as e:
    sys.exit(f"inventory.csv: {e}")
if got[:1] != want[:1]:
    sys.exit(f"header is {got[:1]}, want {want[0]}")
if got != want:
    bad = next((g, w) for g, w in zip(got + [[]] * len(want), want + [[]] * len(got)) if g != w)
    sys.exit(f"{len(got) - 1} rows, want {len(want) - 1}; first difference: got {bad[0]}, want {bad[1]}")
print("ok")
