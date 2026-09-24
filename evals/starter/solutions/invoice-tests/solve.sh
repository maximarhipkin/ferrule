python3 - <<'PY'
import re
t = open("invoice.py").read()
t = re.sub(r"    return round\(subtotal .*\n", "    return round(subtotal * max(0.0, 1 - discount) * (1 + tax), 2)\n", t)
open("invoice.py", "w").write(t)
PY
