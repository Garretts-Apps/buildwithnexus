#!/usr/bin/env python3
"""Exercise the real TUI in a Unix PTY, without an API key or model server.

Usage: python3 scripts/test-terminal-input.py target/debug/buildwithnexus
"""

import errno
import fcntl
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import struct
import subprocess
import sys
import tempfile
import termios
import threading
import time
import unittest


BINARY = str(Path(sys.argv.pop(1)).resolve())


def tiny_png(path, width=4, height=2):
    """Write a valid RGB PNG without any imaging library."""
    import struct as st
    import zlib

    def chunk(kind, data):
        body = kind + data
        return st.pack(">I", len(data)) + body + st.pack(">I", zlib.crc32(body) & 0xFFFFFFFF)

    raw = b"".join(b"\x00" + bytes([255, 0, 0] * width) for _ in range(height))
    png = b"\x89PNG\r\n\x1a\n"
    png += chunk(b"IHDR", st.pack(">IIBBBBB", width, height, 8, 2, 0, 0, 0))
    png += chunk(b"IDAT", zlib.compress(raw))
    png += chunk(b"IEND", b"")
    path.write_bytes(png)


class MockModel:
    """An OpenAI-compatible model on loopback. `reply(body)` returns the
    answer to each chat request: {"text": ...} or {"calls": [(name, args)]}.
    With `key`, a request without that bearer key gets 401. Every request is
    kept as (method, path, headers, body)."""

    def __init__(self, reply, key=None):
        self.requests = []
        model = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def send_json(self, obj, status=200):
                data = json.dumps(obj).encode()
                self.send_response(status)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def refused(self):
                if key is None or self.headers.get("Authorization") == f"Bearer {key}":
                    return False
                self.send_json({"error": {"message": "invalid api key"}}, 401)
                return True

            def do_GET(self):
                model.requests.append(("GET", self.path, dict(self.headers), None))
                if not self.refused():
                    self.send_json({"object": "list",
                                    "data": [{"id": "mock-model", "object": "model"}]})

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"{}")
                model.requests.append(("POST", self.path, dict(self.headers), body))
                if self.refused():
                    return
                answer = reply(body)
                if "status" in answer:
                    self.send_json({"error": {"message": answer["error"]}}, answer["status"])
                    return
                if "calls" in answer:
                    calls = [{"index": i, "id": f"call_{i}", "type": "function",
                              "function": {"name": name, "arguments": json.dumps(args)}}
                             for i, (name, args) in enumerate(answer["calls"])]
                    message = {"role": "assistant", "content": None, "tool_calls": calls}
                    delta, finish = {"role": "assistant", "tool_calls": calls}, "tool_calls"
                else:
                    message = {"role": "assistant", "content": answer["text"]}
                    delta, finish = {"content": answer["text"]}, "stop"
                usage = {"prompt_tokens": 50, "completion_tokens": 10}
                if not body.get("stream"):
                    self.send_json({"choices": [{"index": 0, "message": message,
                                                 "finish_reason": finish}], "usage": usage})
                    return
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.end_headers()
                for chunk in ({"choices": [{"index": 0, "delta": delta}]},
                              {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
                              {"choices": [], "usage": usage}):
                    self.wfile.write(b"data: " + json.dumps(chunk).encode() + b"\n\n")
                self.wfile.write(b"data: [DONE]\n\n")

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}/v1"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def close(self):
        self.server.shutdown()
        self.server.server_close()

    def posts(self):
        return [r for r in self.requests if r[0] == "POST"]

    def origin(self):
        return self.url.rsplit("/", 1)[0]

    def keys_sent(self):
        return [value for r in self.requests for name, value in r[2].items()
                if name.lower() == "authorization"]


class ChatModel:
    """An OpenAI-compatible model on a free loopback port. `reply(messages)`
    returns ("text", str) or ("tool", name, args); each reply streams as
    server-sent events. Every chat request's messages are kept in `requests`."""

    def __init__(self, reply):
        self.requests = []
        model = self

        class Handler(http.server.BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def send_body(self, kind, body):
                self.send_response(200)
                self.send_header("Content-Type", kind)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body)

            def do_GET(self):
                self.send_body("application/json", b'{"object":"list","data":[]}')

            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = json.loads(self.rfile.read(length) or b"{}")
                if not self.path.endswith("/chat/completions"):
                    self.send_error(404)
