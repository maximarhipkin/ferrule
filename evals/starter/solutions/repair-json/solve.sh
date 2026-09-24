cat > config.json <<'J'
{
  "name": "orders",
  "port": 8080,
  "debug": false,
  "hosts": ["a.internal", "b.internal"],
  "limits": {"rps": 250, "burst": 500},
  "motd": "it's \"fine\"",
  "ratio": 0.5,
  "owner": null
}
J
