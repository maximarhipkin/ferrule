"""dedupe-contacts: unique.csv keeps the first row per email (case/space-insensitive), email normalized."""
import csv
import sys

want = [["name", "email", "phone"], ["Dana Levi", "dana@example.com", "050-1111111"],
        ["Cohen, Avi", "avi.cohen@example.com", "052-2222222"], ["Noa Bar", "noa@example.org", ""],
        ['Ron "Ronnie" Shaked', "ron@example.net", "054-3333333"], ["Eli", "eli@example.com", "055-4444444"]]
try:
    got = [r for r in csv.reader(open("unique.csv", newline="")) if r]
except OSError as e:
    sys.exit(f"unique.csv: {e}")
if got != want:
    bad = next(((g, w) for g, w in zip(got, want) if g != w), None)
    sys.exit(f"{len(got) - 1} rows, want {len(want) - 1}" + (f"; got {bad[0]}, want {bad[1]}" if bad else ""))
print("ok")
