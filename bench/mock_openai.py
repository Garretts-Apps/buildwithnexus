#!/usr/bin/env python3
"""Local OpenAI-compatible mock. Every completion is the same canned text.

Speaks chat completions and the Responses API, streaming and not. Model
latency is one in-process write, the same for every client. This is not a
stand-in for a tool that cannot be pointed here.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CANNED = "BENCHMARK_OK"


def _chat_body() -> dict:
    return {
        "id": "chatcmpl-bench",
        "object": "chat.completion",
        "created": 0,
        "model": "bench",
        "choices": [
            {
                "index": 0,
                "message": {"role": "assistant", "content": CANNED},
                "finish_reason": "stop",
            }
        ],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2},
    }


def _response_body() -> dict:
    return {
        "id": "resp_bench",
        "object": "response",
        "status": "completed",
        "model": "bench",
        "output": [
            {
                "id": "msg_bench",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": CANNED, "annotations": []}],
            }
        ],
        "output_text": CANNED,
        "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2},
    }


def _models() -> dict:
    ids = ["bench", "gpt-4o-mini", "gpt-4o", "gpt-5.4-mini"]
    return {
        "object": "list",
        "data": [{"id": i, "object": "model", "created": 0, "owned_by": "bench"} for i in ids],
    }


class MockHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt: str, *args) -> None:
        server = self.server
        if getattr(server, "access_log", None):
            server.access_log.append({"path": self.path, "line": fmt % args})

    def _read_body(self) -> bytes:
        n = int(self.headers.get("Content-Length", "0") or 0)
        if n > 8_000_000:
            return self.rfile.read(8_000_000)
        return self.rfile.read(n) if n else b""

    def _json(self, code: int, obj: dict) -> None:
        raw = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _record(self, body: bytes) -> None:
        rec = getattr(self.server, "requests", None)
        if rec is None:
            return
        text = body.decode("utf-8", "replace")[:2000]
        rec.append(
            {
                "method": self.command,
                "path": self.path.split("?", 1)[0],
                "bytes": len(body),
                "body_prefix": text,
            }
        )

    def do_GET(self) -> None:  # noqa: N802
        self._record(b"")
        path = self.path.split("?", 1)[0].rstrip("/") or "/"
        if path.endswith("/models") or path.endswith("/models/bench"):
            self._json(200, _models())
            return
        self._json(200, {"ok": True, "mock": "openai-compatible"})

    def do_POST(self) -> None:  # noqa: N802
        body = self._read_body()
        self._record(body)
        path = self.path.split("?", 1)[0]
        try:
            payload = json.loads(body.decode() or "{}")
        except json.JSONDecodeError:
            payload = {}
        stream = bool(payload.get("stream"))
        if "responses" in path:
            if stream:
                self._stream_responses()
            else:
                self._json(200, _response_body())
            return
        if "chat/completions" in path or path.rstrip("/").endswith("/completions"):
            if stream:
                self._stream_chat()
            else:
                self._json(200, _chat_body())
            return
        # Unknown POST: still a chat-shaped canned reply so a client that
        # uses a slightly different path gets the same fixture text, and the
        # request log shows the path that was actually used.
        if stream:
            self._stream_chat()
        else:
            self._json(200, _chat_body())

    def _stream_chat(self) -> None:
        chunks = [
            {
                "id": "chatcmpl-bench",
                "object": "chat.completion.chunk",
                "choices": [
                    {
                        "index": 0,
                        "delta": {"role": "assistant", "content": CANNED},
                        "finish_reason": None,
                    }
                ],
            },
            {
                "id": "chatcmpl-bench",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            },
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for obj in chunks:
            self.wfile.write(b"data: " + json.dumps(obj).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()

    def _stream_responses(self) -> None:
        full = _response_body()
        events = [
            (
                "response.created",
                {"type": "response.created", "response": {**full, "status": "in_progress", "output": []}},
            ),
            (
                "response.output_text.delta",
                {
                    "type": "response.output_text.delta",
                    "item_id": "msg_bench",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": CANNED,
                },
            ),
            (
                "response.output_text.done",
                {
                    "type": "response.output_text.done",
                    "item_id": "msg_bench",
                    "output_index": 0,
                    "content_index": 0,
                    "text": CANNED,
                },
            ),
            ("response.completed", {"type": "response.completed", "response": full}),
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.end_headers()
        for name, obj in events:
            self.wfile.write(f"event: {name}\ndata: {json.dumps(obj)}\n\n".encode())
        self.wfile.flush()


class MockServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), MockHandler)
        self.requests: list[dict] = []
        self.access_log: list[dict] = []


def start() -> tuple[MockServer, str]:
    server = MockServer()
    thread = threading.Thread(target=server.serve_forever, name="mock-openai", daemon=True)
    thread.start()
    host, port = server.server_address
    return server, f"http://{host}:{port}/v1"


if __name__ == "__main__":
    srv, base = start()
    print(base, flush=True)
    threading.Event().wait()
