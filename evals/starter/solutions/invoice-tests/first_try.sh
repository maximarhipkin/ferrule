python3 - <<'PY'
t = open("invoice.py").read()
t = t.replace("round(subtotal * (1 + tax) - discount, 2)", "round(subtotal * (1 - discount) * (1 + tax), 2)")
open("invoice.py", "w").write(t)
PY
