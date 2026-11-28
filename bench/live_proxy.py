"""A local OpenAI-compatible endpoint that forwards to one live model and
records what each request cost (roadmap 6.4).

Every harness is pointed here, the way the efficiency bench points them at
`mock_openai.py`. Each request is forwarded to the upstream endpoint with the
model replaced by the pinned one, so every harness talks to the same model
whatever it was told. The token usage the upstream reports (`usage`, in the
JSON reply or the last event of a stream) is kept per request, so tokens per
task are measured the same way for every harness, not taken from each
harness's own accounting.

The upstream key is never in this process: the machine's egress proxy adds
it (the request carries a placeholder), or `LIVE_API_KEY` names it.
"""

from __future__ import annotations

import http.server
import json
import os
import ssl
import threading
import urllib.request


class LiveProxy(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self, upstream: str, model: str) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.upstream = upstream.rstrip("/")
        self.model = model
        self.requests: list[dict] = []
        self.lock = threading.Lock()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}/v1"


def _usage_from(body: bytes, streamed: bool) -> dict | None:
    if not streamed:
        try:
            return json.loads(body).get("usage")
        except (json.JSONDecodeError, AttributeError):
            return None
    usage = None
    for line in body.decode("utf-8", "replace").splitlines():
        if line.startswith("data:") and '"usage"' in line:
            try:
                u = json.loads(line[5:].strip()).get("usage")
            except json.JSONDecodeError:
                continue
            if u:
                usage = u
    return usage


class Handler(http.server.BaseHTTPRequestHandler):
    server: LiveProxy

    def log_message(self, fmt: str, *args) -> None:  # quiet
        return

    def do_GET(self) -> None:  # noqa: N802
        # Model lists: the pinned model only.
        body = json.dumps({"object": "list", "data": [{"id": self.server.model, "object": "model"}]}).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_POST(self) -> None:  # noqa: N802
        n = int(self.headers.get("Content-Length", "0") or 0)
        raw = self.rfile.read(n) if n else b""
        try:
            payload = json.loads(raw or b"{}")
        except json.JSONDecodeError:
            payload = {}
        payload["model"] = self.server.model
        streamed = bool(payload.get("stream"))
        if streamed and "chat/completions" in self.path:
            payload.setdefault("stream_options", {})["include_usage"] = True
        path = self.path.split("?", 1)[0]
        if path.startswith("/v1"):
            path = path[3:]
        req = urllib.request.Request(
            self.server.upstream + path,
            data=json.dumps(payload).encode(),
            method="POST",
            headers={
                "content-type": "application/json",
                # urllib's own user agent is throttled by some fronts.
                "user-agent": "bwn-bench/1.0",
                "authorization": f"Bearer {os.environ.get('LIVE_API_KEY', 'proxy-injected')}",
            },
        )
        ctx = ssl.create_default_context(cafile=os.environ.get("SSL_CERT_FILE") or None)
        status, headers, body = 502, {}, b""
        try:
            with urllib.request.urlopen(req, timeout=300, context=ctx) as r:
                status, headers, body = r.status, dict(r.headers), r.read()
        except urllib.error.HTTPError as e:
            status, headers, body = e.code, dict(e.headers), e.read()
        except OSError as e:
            body = json.dumps({"error": {"message": str(e)}}).encode()
        with self.server.lock:
            self.server.requests.append({
                "path": self.path,
                "status": status,
                "request_bytes": len(raw),
                "stream": streamed,
                "usage": _usage_from(body, streamed),
            })
        self.send_response(status)
        self.send_header("content-type", headers.get("Content-Type", "application/json"))
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def start(upstream: str, model: str) -> LiveProxy:
    srv = LiveProxy(upstream, model)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv
