#!/usr/bin/env python3
"""Minimal MCP stdio server for ferrule-mcp's hermetic tests.

Speaks newline-delimited JSON-RPC 2.0. tools/list paginates two-at-a-time
via a numeric cursor. tools/call supports: echo, add, boom (isError),
slow (never responds, for timeout/concurrency tests), crash (exits without
responding), ping_first (sends a server->client ping reusing the call's own
id before answering, to catch id-space confusion), write (creates a file,
reporting a refusal as text rather than failing), env (reads a variable),
grow (adds the tool named in its arguments to the list, then sends
notifications/tools/list_changed before answering).

`mock_mcp.py --warm <file>` is a warm-up run instead: it appends the value
of FERRULE_TEST_WARM to <file> and exits.
"""
import sys, json, os

TOOLS = [
    {"name": "echo", "description": "Echo text back", "inputSchema": {"type": "object", "properties": {"text": {"type": "string"}}}},
    {"name": "add", "description": "Add two numbers", "inputSchema": {"type": "object", "properties": {"a": {"type": "number"}, "b": {"type": "number"}}}},
    {"name": "boom", "description": "Always fails", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "slow", "description": "Never responds", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "crash", "description": "Exits without responding", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "ping_first", "description": "Pings the client before answering", "inputSchema": {"type": "object", "properties": {}}},
    {"name": "write", "description": "Write a file", "inputSchema": {"type": "object", "properties": {"path": {"type": "string"}}}},
    {"name": "env", "description": "Read an environment variable", "inputSchema": {"type": "object", "properties": {"name": {"type": "string"}}}},
]


def send(obj):
    sys.stdout.write(json.dumps(obj) + "\n")
    sys.stdout.flush()


def main():
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        msg = json.loads(line)
        method, mid, params = msg.get("method"), msg.get("id"), msg.get("params") or {}
        if method == "initialize":
            send({"jsonrpc": "2.0", "id": mid, "result": {"protocolVersion": "2025-06-18", "capabilities": {}, "serverInfo": {"name": "mock", "version": "0.1"}}})
        elif method == "notifications/initialized":
            continue
        elif method == "tools/list":
            cursor = int(params.get("cursor") or 0)
            page = TOOLS[cursor:cursor + 2]
            result = {"tools": page}
            if cursor + 2 < len(TOOLS):
                result["nextCursor"] = str(cursor + 2)
            send({"jsonrpc": "2.0", "id": mid, "result": result})
        elif method == "tools/call":
            name, args = params.get("name"), params.get("arguments") or {}
            if name == "echo":
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": args.get("text", "")}], "isError": False}})
            elif name == "add":
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": str(args.get("a", 0) + args.get("b", 0))}], "isError": False}})
            elif name == "boom":
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "boom failed"}], "isError": True}})
            elif name == "slow":
                continue  # never answer; keep serving other requests
            elif name == "ping_first":
                send({"jsonrpc": "2.0", "id": mid, "method": "ping"})
                pong = json.loads(sys.stdin.readline())
                ok = pong.get("id") == mid and pong.get("result") == {} and "method" not in pong
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "pong-ok" if ok else "pong-bad"}], "isError": False}})
            elif name == "write":
                try:
                    with open(args["path"], "w") as f:
                        f.write("mcp")
                    text = "wrote"
                except OSError as e:
                    text = "refused: %s" % e
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": text}], "isError": False}})
            elif name == "env":
                value = os.environ.get(args.get("name", ""), "<unset>")
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": value}], "isError": False}})
            elif name == "grow":
                TOOLS.append({"name": args["name"], "description": args.get("description", ""), "inputSchema": {"type": "object", "properties": {}}})
                send({"jsonrpc": "2.0", "method": "notifications/tools/list_changed"})
                send({"jsonrpc": "2.0", "id": mid, "result": {"content": [{"type": "text", "text": "grown"}], "isError": False}})
            elif name == "crash":
                os._exit(1)
            else:
                send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "unknown tool"}})
        elif mid is not None:
            send({"jsonrpc": "2.0", "id": mid, "error": {"code": -32601, "message": "method not found"}})


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--warm":
        with open(sys.argv[2], "a") as f:
            f.write(os.environ.get("FERRULE_TEST_WARM", "<unset>") + "\n")
        sys.exit(0)
    main()
