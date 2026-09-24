"""csv-to-markdown: table.md is the countries, largest population first, with thousands separators."""
import csv
import sys

rows = list(csv.DictReader(open("countries.csv", encoding="utf-8")))
rows.sort(key=lambda r: -int(r["population"]))
want = [["Country", "Capital", "Population"]] + [[r["country"], r["capital"], f"{int(r['population']):,}"] for r in rows]
try:
    lines = [l.strip() for l in open("table.md", encoding="utf-8") if l.strip().startswith("|")]
except OSError as e:
    sys.exit(f"table.md: {e}")
cells = [[c.strip() for c in l.strip("|").split("|")] for l in lines]
if len(cells) < 2 or not all(set(c) <= set("-: ") for c in cells[1]):
    sys.exit("table.md has no Markdown table (header, then a |---| separator row)")
got = [cells[0]] + cells[2:]
if got != want:
    bad = next(((g, w) for g, w in zip(got, want) if g != w), (len(got), len(want)))
    sys.exit(f"table differs: got {bad[0]}, want {bad[1]}")
print("ok")
