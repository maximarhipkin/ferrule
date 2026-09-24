"""Writes logs/<service>.log and its rotated older half, logs/<service>.1.log, for the night of 14-15 July 2026,
Israel summer time. One request fails in all three. Deterministic."""
import datetime as dt
import os
import random

rng = random.Random(915)
os.makedirs("logs", exist_ok=True)
services = ["gateway", "orders", "payments"]
reqs = ["r-%06x" % rng.getrandbits(24) for _ in range(60)]
culprit = "r-%06x" % rng.getrandbits(24)
start = dt.datetime(2026, 7, 15, 1, 40, 0)
msgs = {
    "INFO": ["request accepted", "cache hit for cart", "db query took 12ms", "response sent 200",
             "session refreshed", "queued for fulfilment", "card tokenized"],
    "WARN": ["slow upstream (812ms)", "retrying once", "cache miss storm"],
    "ERROR": ["upstream timed out", "deadlock detected, rolled back", "card processor returned 502"],
}
# Requests failing in one or two services only: decoys.
decoys = {r: rng.sample(services, rng.choice([1, 2])) for r in rng.sample(reqs, 12)}
for svc_i, svc in enumerate(services):
    lines = []
    t = start
    for i in range(760):
        t += dt.timedelta(seconds=rng.randint(1, 20))
        r = rng.choice(reqs)
        level = "ERROR" if svc in decoys.get(r, []) and rng.random() < 0.3 else rng.choice(["INFO"] * 8 + ["WARN"])
        lines.append((t, f"{level:5} req={r} {svc}: {rng.choice(msgs[level])}"))
    # the culprit: fails in every service, a few seconds apart, around 02:14
    base = dt.datetime(2026, 7, 15, 2, 14, 7) + dt.timedelta(seconds=[9, 0, 4][svc_i])
    lines.append((base - dt.timedelta(seconds=30), f"INFO  req={culprit} {svc}: request accepted"))
    lines.append((base, f"ERROR req={culprit} {svc}: {msgs['ERROR'][svc_i]}"))
    lines.append((base + dt.timedelta(seconds=40), f"ERROR req={culprit} {svc}: giving up after retry"))
    lines.sort()
    half = len(lines) // 2
    for path, part in ((f"logs/{svc}.1.log", lines[:half]), (f"logs/{svc}.log", lines[half:])):
        with open(path, "w") as f:
            for t, text in part:
                f.write(f"{t:%Y-%m-%d %H:%M:%S} {text}\n")
