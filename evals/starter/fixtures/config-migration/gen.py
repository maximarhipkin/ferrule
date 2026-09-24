"""Writes services/*.ini: six v1 service configs with long review comments.
Deterministic; the grader re-runs it to know the originals."""
import os
import random

rng = random.Random(1402)
os.makedirs("services", exist_ok=True)
names = ["billing", "catalog", "gateway", "orders", "payments", "search"]
notes = [
    "keep retries as they are: the upstream rate limiter counts them",
    "the pool size was tuned under the March load test; don't touch it",
    "this host moves in Q3; the url must still point at it until then",
    "timeouts here are generous on purpose, the batch jobs are slow",
    "legacy_mode only ever toggled the old XML codec, long removed",
    "owner changes need a ticket in the platform queue first",
]
for name in names:
    out = [f"# {name} service configuration (v1)", ""]
    for i in range(80):
        out.append(f"# review {2023 + i % 3}-{1 + i % 12:02d}-{1 + i % 27:02d}: {rng.choice(notes)}; "
                   f"checked by {rng.choice(['ops', 'sre', 'dev', 'qa'])} on the {name} runbook, page {i + 1}; "
                   "raise any change at the weekly platform review before it ships")
    out += ["", "[service]", f"name = {name}",
            f"host = 10.{rng.randint(0, 9)}.{rng.randint(0, 255)}.{rng.randint(1, 254)}",
            f"port = {rng.choice([8080, 8443, 9000, 7001])}",
            "# decision: keep this timeout even if the batch jobs get faster",
            f"timeout = {rng.choice([5, 10, 30, 45, 120])}",
            f"legacy_mode = {rng.choice(['true', 'false'])}",
            f"retries = {rng.randint(1, 5)}",
            f"pool_size = {rng.choice([8, 16, 32])}",
            "", "[cache]", "# the cache's own timeout is not part of the migration",
            f"timeout = {rng.choice([60, 300, 900])}", f"size_mb = {rng.choice([128, 256, 512])}", ""]
    for i in range(20):
        out.append(f"# appendix {i + 1}: {rng.choice(notes)} — recorded during the {name} review, "
                   "see the platform wiki for the full discussion and the sign-off list, "
                   "and keep this appendix with the file when it moves")
    with open(f"services/{name}.ini", "w") as f:
        f.write("\n".join(out) + "\n")
