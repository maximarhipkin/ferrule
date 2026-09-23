#!/usr/bin/env python3
"""Mock OpenAI-compatible LLM server for ferrule end-to-end smoke tests.

First request  -> assistant message with a tool_call (list_dir on ".")
Second request -> final text answer echoing what the tool returned
"""
import json
from http.server import BaseHTTPRequestHandler, HTTPServer

calls = {"n": 0}

class Handler(BaseHTTPRequestHandler):
    def do_POST(self):
        calls["n"] += 1
        length = int(self.headers.get("content-length", 0))
        body = json.loads(self.rfile.read(length) or b"{}")

        # sanity: every request must carry model + messages
        assert "model" in body and "messages" in body

        if calls["n"] == 1:
            resp = {
                "choices": [{"message": {
                    "content": None,
                    "reasoning_content": "I should list the workspace first.",
                    "tool_calls": [{
                        "id": "call_42", "type": "function",
                        "function": {"name": "list_dir", "arguments": '{"path": "."}'}
                    }]
                }}],
                "usage": {"prompt_tokens": 120, "completion_tokens": 30,
                          "prompt_tokens_details": {"cached_tokens": 100}}
            }
        else:
            # The tool result must be present in the conversation by now.
            roles = [m.get("role") for m in body["messages"]]
            assert "tool" in roles, f"no tool message in follow-up request: {roles}"
            tool_msg = next(m for m in body["messages"] if m.get("role") == "tool")
            resp = {
                "choices": [{"message": {
                    "content": f"Workspace listing received ({len(tool_msg.get('content',''))} chars). Done.",
                    "tool_calls": []
                }}],
                "usage": {"prompt_tokens": 200, "completion_tokens": 25,
                          "prompt_tokens_details": {"cached_tokens": 180}}
            }

        data = json.dumps(resp).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    def log_message(self, *a):
        pass

if __name__ == "__main__":
    server = HTTPServer(("127.0.0.1", 18921), Handler)
    print("mock llm on :18921", flush=True)
    server.serve_forever()
