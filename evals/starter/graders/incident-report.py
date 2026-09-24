"""incident-report: incident.json names the request that failed in all three
services, its first ERROR in UTC (the logs are UTC+3), and the services."""
import datetime as dt
import glob
import json
import os
import sys

errors = {}
for path in glob.glob("logs/*.log"):
    svc = os.path.basename(path).split(".")[0]
    for line in open(path):
        parts = line.split()
        if len(parts) > 3 and parts[2] == "ERROR":
            req = parts[3].split("=", 1)[1]
            t = dt.datetime.strptime(f"{parts[0]} {parts[1]}", "%Y-%m-%d %H:%M:%S")
            e = errors.setdefault(req, {})
            e[svc] = min(t, e.get(svc, t))
all3 = [r for r, e in errors.items() if len(e) == 3]
if len(all3) != 1:
    sys.exit(f"the logs are missing or changed ({len(all3)} candidates): can't grade")
req = all3[0]
first = min(errors[req].values()) - dt.timedelta(hours=3)
want = {"request_id": req, "first_error_utc": f"{first:%Y-%m-%dT%H:%M:%SZ}",
        "services": ["gateway", "orders", "payments"]}
try:
    got = json.load(open("incident.json"))
except (OSError, ValueError) as e:
    sys.exit(f"incident.json: {e}")
for k, v in want.items():
    if got.get(k) != v:
        sys.exit(f"incident.json {k} = {got.get(k)!r}, want {v!r}")
print("ok")
