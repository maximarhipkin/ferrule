"""Writes logs/1.log .. logs/5.log: the commit review record for 4.2.0.
Generated (not checked in) so the suite stays small; deterministic."""
import os
import random

rng = random.Random(42)
os.makedirs("logs", exist_ok=True)
authors = ["alice", "bob", "chen", "dana", "bot-renovate", "eli"]
types = ["feat", "fix", "chore", "docs", "refactor", "test"]
scopes = ["api", "cli", "web", "db", "api", "cli"]
verbs = ["add", "support", "handle", "speed up", "validate", "stream", "cache", "retry", "log", "reject"]
things = ["pagination", "empty carts", "unicode names", "large uploads", "expired tokens", "timezones",
          "partial refunds", "webhook storms", "stale sessions", "gzip bodies", "long filenames", "rate limits"]
where = ["in /orders", "in the export command", "on login", "in search", "for admins", "during sync",
         "in the importer", "on checkout", "in reports", "for guests"]
filler = ("Reviewed against the staging data set; the change is covered by the integration suite and was "
          "exercised by hand on the release candidate. Rollback is a plain revert; the feature flag, if any, "
          "stays on for a week and the dashboards are watched for errors. ")
pr = 3100
for n in range(1, 6):
    lines = []
    for _ in range(22):
        pr += rng.randint(1, 7)
        t, s = rng.choice(types), rng.choice(scopes)
        msg = f"{rng.choice(verbs)} {rng.choice(things)} {rng.choice(where)}"
        sha = "%040x" % rng.getrandbits(160)
        lines += [f"commit {sha}", f"Author: {rng.choice(authors)}", f"PR: #{pr}", "",
                  f"    {t}({s}): {msg}", ""]
        for _ in range(rng.randint(3, 5)):
            lines.append("    " + filler.strip())
        lines.append("")
    with open(f"logs/{n}.log", "w") as f:
        f.write("\n".join(lines))
