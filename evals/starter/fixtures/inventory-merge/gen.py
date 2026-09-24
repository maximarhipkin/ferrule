"""Writes warehouses/w1.json .. w5.json: stock dumps with long audit notes.
Deterministic."""
import json
import os
import random

rng = random.Random(5150)
os.makedirs("warehouses", exist_ok=True)
skus = [f"SKU-{i:04d}" for i in rng.sample(range(1000, 9999), 40)]
names = {s: f"{rng.choice(['Floor', 'Trunk', 'Dash', 'Seat', 'Door'])} {rng.choice(['mat', 'cover', 'liner', 'tray', 'guard'])} {rng.choice(['A', 'B', 'X', 'Pro', 'Lite'])}" for s in skus}
dead = set(rng.sample(skus, 5))
for w in range(1, 6):
    recs = []
    for s in rng.sample(skus, 32):
        rec = {"sku": s, "name": names[s], "qty": rng.randint(0, 80),
               "price": round(rng.uniform(9, 260), 2),
               "audit": f"counted by {rng.choice(['ops', 'night shift', 'contractor'])} on shelf "
                        f"{rng.randint(1, 40)}{rng.choice('ABCDEF')}; label reprinted; no damage found; "
                        "pallet re-wrapped and moved to the pick face after the recount",
               "history": [f"{d}: cycle count matched the system figure, signed off by the shift lead" for d in ("Jan", "Apr", "Jul")],
               "handling": "store flat, away from heaters; stack at most eight high; rotate stock first in, first out; "
                           "damaged units go to the returns cage with a red tag, never back to the pick face; "
                           "recount after any move between zones and record it in the shift log"}
        if s in dead and rng.random() < 0.5:
            rec["discontinued"] = True
        recs.append(rec)
    with open(f"warehouses/w{w}.json", "w") as f:
        json.dump({"warehouse": f"W{w}", "records": recs}, f, indent=2)
