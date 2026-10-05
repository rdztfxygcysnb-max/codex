#!/usr/bin/env python3
"""Minimal Chat Completions relay for the CI isolation test.

Serves one canned SSE response on POST /v1/chat/completions so the codex
client can complete a full turn without any real provider.
"""
import http.server
import json
import socketserver

CHUNKS = [
    {"choices": [{"delta": {"content": "hi"}}]},
    {"choices": [{"delta": {}, "finish_reason": "stop"}]},
    {"choices": [], "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}},
]


class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", 0))
        self.rfile.read(length)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for chunk in CHUNKS:
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def log_message(self, *args):
        pass


if __name__ == "__main__":
    socketserver.TCPServer.allow_reuse_address = True
    with socketserver.TCPServer(("127.0.0.1", 8899), Handler) as httpd:
        httpd.serve_forever()
