python3 - <<'PY'
import datetime as dt, glob, json, os
errors = {}
for p in glob.glob("logs/*.log"):
    svc = os.path.basename(p).split(".")[0]
    for line in open(p):
        f = line.split()
        if len(f) > 3 and f[2] == "ERROR":
            t = dt.datetime.strptime(f[0] + " " + f[1], "%Y-%m-%d %H:%M:%S")
            errors.setdefault(f[3][4:], {}).setdefault(svc, t)
req = next(r for r, e in errors.items() if len(e) == 3)
first = min(errors[req].values()) - dt.timedelta(hours=3)
json.dump({"request_id": req, "first_error_utc": first.strftime("%Y-%m-%dT%H:%M:%SZ"),
           "services": sorted(errors[req])}, open("incident.json", "w"))
PY
