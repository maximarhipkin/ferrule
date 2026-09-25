python3 - <<'PY'
import glob, re
pairs = [("get_price", "price_for"), ("calc_tax", "tax_for"), ("apply_coupon", "apply_discount")]
for p in glob.glob("**/*.py", recursive=True):
    t = open(p).read()
    for o, n in pairs:
        t = re.sub(rf"\b{o}\b", n, t)
    if p.replace("\\", "/") == "shop/__init__.py":  # glob gives shop\__init__.py on Windows
        t += '\nAPI_VERSION = "2.0"\n'
    open(p, "w").write(t)
open("CHANGELOG.md", "w").write("".join(f"- renamed {o} -> {n}\n" for o, n in pairs))
PY
