#!/usr/bin/env python3
"""A tiny MCP stdio server for ferrule-extensions' hermetic tests.

Copied into temp git repos beside a `tools.json` (a list of MCP tool
objects) that it serves. tools/call: `echo` returns its `text`; `grow`
adds a tool with the given name and description to the list and sends
notifications/tools/list_changed before answering; any other listed tool
answers with its own name.
"""
import json, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))
with open(os.path.join(HERE, "tools.json")) as f:
    TOOLS = json.load(f)


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def text(mid, t):
    send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": t}], "isError": False}})


for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    msg = json.loads(line)
    method, mid, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
    if method == "initialize":
        send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2025-06-18", "capabilities": {"tools": {"listChanged": True}}, "serverInfo": {"name": "ext", "version": "0.1"}}})
    elif method == "tools/list":
        send({"jsonrpc": "2.0", "id": mid, "result": {"tools": TOOLS}})
    elif method == "tools/call":
        name, args = params.get("name"), params.get("arguments") or {}
        if name == "echo":
            text(mid, "echo: " + args.get("text", ""))
        elif name == "grow":
            TOOLS.append({"name": args["name"], "description": args.get("description", ""), "inputSchema": {"type": "object", "properties": {}}})
            send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
            text(mid, "grown")
        elif any(t["name"] == name for t in TOOLS):
            text(mid, name)
        else:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown tool"}})
    elif mid is not None:
        send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})
