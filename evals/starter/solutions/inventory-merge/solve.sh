python3 - <<'PY'
import csv, glob, json
q, p, n, dead = {}, {}, {}, set()
for f in glob.glob("warehouses/*.json"):
    for r in json.load(open(f))["records"]:
        s = r["sku"]
        if r.get("discontinued"):
            dead.add(s)
        q[s] = q.get(s, 0) + r["qty"]
        p[s] = min(p.get(s, r["price"]), r["price"])
        n[s] = r["name"]
with open("inventory.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["sku", "name", "qty", "price"])
    for s in sorted(q):
        if s not in dead:
            w.writerow([s, n[s], q[s], f"{p[s]:.2f}"])
PY
