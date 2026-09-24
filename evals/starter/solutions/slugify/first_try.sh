cat > slug.py <<'PY'
def slugify(title: str) -> str:
    return "-".join(title.lower().split())
PY
