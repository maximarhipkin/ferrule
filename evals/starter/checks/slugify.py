"""slugify: the house slug rules. The task's prompt only says "lowercase words
joined by hyphens"; the rest is found by running this check."""
import sys

sys.path.insert(0, ".")
try:
    from slug import slugify
except Exception as e:  # noqa: BLE001
    sys.exit(f"can't import slugify from slug.py: {e}")

cases = [
    ("Hello World", "hello-world"),
    ("  Rust   is  fast  ", "rust-is-fast"),
    ("C'est la vie!", "cest-la-vie"),
    ("Déjà vu, again", "deja-vu-again"),
    ("Top 10 tips: 2026 edition", "top-10-tips-2026-edition"),
    ("rock & roll", "rock-and-roll"),
    ("--Already--dashed--", "already-dashed"),
    ("a" * 100, "a" * 60),
    ("one two three four five six seven eight nine ten eleven twelve", "one-two-three-four-five-six-seven-eight-nine-ten-eleven"),
]
bad = []
for title, want in cases:
    try:
        got = slugify(title)
    except Exception as e:  # noqa: BLE001
        got = f"<{type(e).__name__}: {e}>"
    if got != want:
        bad.append(f"slugify({title!r}) = {got!r}, want {want!r}")
if bad:
    print("\n".join(bad))
    print("rules: ASCII only (accents folded: é -> e), apostrophes dropped, '&' is 'and', "
          "other punctuation separates words, no leading/trailing/double hyphens, "
          "at most 60 characters, cut at a word boundary when possible")
    sys.exit(1)
print("ok")
