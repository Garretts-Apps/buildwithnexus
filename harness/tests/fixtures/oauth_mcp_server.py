#!/usr/bin/env python3
"""OAuth-protected Streamable HTTP MCP server for the test suite.

One process plays every role of the MCP authorization spec on 127.0.0.1:

  /mcp                                         MCP endpoint; 401 + WWW-Authenticate without a valid bearer
  /.well-known/oauth-protected-resource/mcp   protected-resource metadata (RFC 9728)
  /.well-known/oauth-authorization-server/auth  authorization-server metadata (RFC 8414, issuer .../auth)
  /auth/register                               dynamic client registration (RFC 7591)
  /auth/authorize                              auto-approves and redirects to the loopback redirect_uri
  /auth/token                                  authorization_code (PKCE S256 checked) and refresh_token grants
  /auth/revoke                                 token revocation (RFC 7009)
  /_admin/expire                               invalidates every access token, as a server-side expiry would

The first stdout line is "port <n>". Every request the tests assert on is
appended to the --log file as one JSON object per line.

Options:
  --log FILE            request log (JSONL)
  --no-registration     no registration_endpoint; only --static-client is accepted
  --static-client ID    a pre-registered public client allowed any loopback port
  --bad-state           authorize redirects back with a state it did not receive
  --ttl SECS            access token lifetime (expires_in), default 3600

MCP tools: `whoami` (names the client the token was issued to) and
`echo_header` (echoes the Authorization header back, so tests can prove the
client scrubs its own token out of tool results).
"""
import base64
import hashlib
import json
import secrets
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlencode, urlparse

ARGS = sys.argv[1:]


def opt(name, default=None):
    if name in ARGS:
        i = ARGS.index(name)
        return ARGS[i + 1] if i + 1 < len(ARGS) else default
    return default


LOG = opt("--log")
REGISTRATION = "--no-registration" not in ARGS
STATIC_CLIENT = opt("--static-client")
BAD_STATE = "--bad-state" in ARGS
TTL = int(opt("--ttl", "3600"))

LOCK = threading.Lock()
CLIENTS = {}  # client_id -> registration
CODES = {}  # code -> (client_id, redirect_uri, challenge, resource)
ACCESS = {}  # access token -> client_id
REFRESH = {}  # refresh token -> client_id


def log(entry):
    if not LOG:
        return
    with LOCK, open(LOG, "a") as f:
        f.write(json.dumps(entry) + "\n")


def is_loopback_redirect(uri):
    u = urlparse(uri)
    return u.scheme == "http" and u.hostname == "127.0.0.1" and u.path == "/callback"


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    @property
    def base(self):
        return "http://127.0.0.1:%d" % self.server.server_address[1]

    def send(self, code, body=b"", ctype="application/json", headers=()):
        if isinstance(body, (dict, list)):
            body = json.dumps(body).encode()
        elif isinstance(body, str):
            body = body.encode()
        self.send_response(code)
        if body:
            self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        for k, v in headers:
            self.send_header(k, v)
        self.end_headers()
        self.wfile.write(body)

    def body(self):
        n = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(n).decode() if n else ""

    def form(self):
        return {k: v[0] for k, v in parse_qs(self.body()).items()}

    def oauth_error(self, code, error, desc=""):
        self.send(code, {"error": error, "error_description": desc})

    # ── metadata and authorization ──────────────────────────────────────
    def do_GET(self):
        u = urlparse(self.path)
        q = {k: v[0] for k, v in parse_qs(u.query).items()}
        if u.path == "/.well-known/oauth-protected-resource/mcp":
            log({"kind": "prm"})
            return self.send(200, {
                "resource": self.base + "/mcp",
                "authorization_servers": [self.base + "/auth"],
                "scopes_supported": ["mcp:tools"],
                "bearer_methods_supported": ["header"],
            })
        if u.path == "/.well-known/oauth-authorization-server/auth":
            log({"kind": "as_metadata"})
            meta = {
                "issuer": self.base + "/auth",
                "authorization_endpoint": self.base + "/auth/authorize",
                "token_endpoint": self.base + "/auth/token",
                "revocation_endpoint": self.base + "/auth/revoke",
                "response_types_supported": ["code"],
                "grant_types_supported": ["authorization_code", "refresh_token"],
                "code_challenge_methods_supported": ["S256"],
                "token_endpoint_auth_methods_supported": ["none"],
            }
            if REGISTRATION:
                meta["registration_endpoint"] = self.base + "/auth/register"
            return self.send(200, meta)
        if u.path == "/auth/authorize":
            log({"kind": "authorize", "query": q})
            client = q.get("client_id", "")
            redirect = q.get("redirect_uri", "")
            known = CLIENTS.get(client)
            if known is not None:
                ok_redirect = redirect in known["redirect_uris"]
            else:
                ok_redirect = client == STATIC_CLIENT and is_loopback_redirect(redirect)
            if not ok_redirect:
                return self.send(400, "unknown client or redirect_uri", "text/plain")
            if q.get("response_type") != "code" or q.get("code_challenge_method") != "S256":
                return self.send(400, "code + S256 required", "text/plain")
            if q.get("resource") != self.base + "/mcp":
                return self.send(400, "wrong resource", "text/plain")
            code = "code-" + secrets.token_hex(8)
            with LOCK:
                CODES[code] = (client, redirect, q.get("code_challenge", ""), q.get("resource"))
            state = "forged-state" if BAD_STATE else q.get("state", "")
            loc = redirect + "?" + urlencode({"code": code, "state": state, "iss": self.base + "/auth"})
            return self.send(302, b"", headers=[("Location", loc)])
        return self.send(404, {"error": "not found"})

    def do_POST(self):
        u = urlparse(self.path)
        if u.path == "/auth/register":
            req = json.loads(self.body() or "{}")
            client = "dyn-" + secrets.token_hex(4)
            reg = {
                "client_id": client,
                "redirect_uris": req.get("redirect_uris", []),
                "token_endpoint_auth_method": "none",
                "grant_types": req.get("grant_types", []),
                "client_name": req.get("client_name"),
            }
            with LOCK:
                CLIENTS[client] = reg
            log({"kind": "register", "request": req, "client_id": client})
            return self.send(201, reg)
        if u.path == "/auth/token":
            return self.token(self.form())
        if u.path == "/auth/revoke":
            f = self.form()
            with LOCK:
                ACCESS.pop(f.get("token"), None)
                REFRESH.pop(f.get("token"), None)
            log({"kind": "revoke", "token": f.get("token"), "hint": f.get("token_type_hint")})
            return self.send(200)
        if u.path == "/_admin/expire":
            with LOCK:
                ACCESS.clear()
            log({"kind": "expire"})
            return self.send(200, {})
        if u.path == "/mcp":
            return self.mcp()
        return self.send(404, {"error": "not found"})

    def issue(self, client):
        access = "at-" + secrets.token_hex(24)
        refresh = "rt-" + secrets.token_hex(24)
        with LOCK:
            ACCESS[access] = client
            REFRESH[refresh] = client
        return {
            "access_token": access,
            "token_type": "Bearer",
            "expires_in": TTL,
            "refresh_token": refresh,
            "scope": "mcp:tools",
        }

    def token(self, f):
        grant = f.get("grant_type")
        client = f.get("client_id", "")
        log({"kind": "token", "grant": grant, "form": f})
        if f.get("resource") != self.base + "/mcp":
            return self.oauth_error(400, "invalid_target", "resource must name the MCP server")
        if grant == "authorization_code":
            with LOCK:
                entry = CODES.pop(f.get("code", ""), None)
            if entry is None:
                return self.oauth_error(400, "invalid_grant", "unknown code")
            want_client, redirect, challenge, _ = entry
            if client != want_client or f.get("redirect_uri") != redirect:
                return self.oauth_error(400, "invalid_grant", "client or redirect_uri mismatch")
            verifier = f.get("code_verifier", "")
            digest = hashlib.sha256(verifier.encode()).digest()
            if base64.urlsafe_b64encode(digest).rstrip(b"=").decode() != challenge:
                return self.oauth_error(400, "invalid_grant", "PKCE verification failed")
            return self.send(200, self.issue(client))
        if grant == "refresh_token":
            with LOCK:
                owner = REFRESH.pop(f.get("refresh_token", ""), None)
            if owner is None or owner != client:
                return self.oauth_error(400, "invalid_grant", "refresh token is not valid")
            return self.send(200, self.issue(client))
        return self.oauth_error(400, "unsupported_grant_type")

    # ── MCP over Streamable HTTP ────────────────────────────────────────
    def mcp(self):
        auth = self.headers.get("Authorization") or ""
        token = auth[7:] if auth.startswith("Bearer ") else ""
        with LOCK:
            client = ACCESS.get(token)
        raw = self.body()
        try:
            msg = json.loads(raw)
        except ValueError:
            msg = {}
        log({"kind": "mcp", "method": msg.get("method"), "authorization": auth,
             "status": 200 if client else 401})
        if client is None:
            challenge = 'Bearer resource_metadata="%s/.well-known/oauth-protected-resource/mcp", scope="mcp:tools"' % self.base
            if token:
                challenge += ', error="invalid_token"'
            return self.send(401, {"error": "unauthorized"},
                             headers=[("WWW-Authenticate", challenge)])
        rid = msg.get("id")
        method = msg.get("method")
        if rid is None:
            return self.send(202)
        if method == "initialize":
            result = {"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                      "serverInfo": {"name": "oauth-mcp", "version": "1.0"}}
        elif method == "tools/list":
            result = {"tools": [
                {"name": "whoami", "description": "Names the OAuth client",
                 "inputSchema": {"type": "object", "properties": {}}},
                {"name": "echo_header", "description": "Echoes the Authorization header",
                 "inputSchema": {"type": "object", "properties": {}}},
            ]}
        elif method == "tools/call":
            name = (msg.get("params") or {}).get("name")
            if name == "whoami":
                text = "hello " + client
            elif name == "echo_header":
                text = "you sent: " + auth
            else:
                text = "unknown tool"
            result = {"content": [{"type": "text", "text": text}]}
        else:
            return self.send(200, {"jsonrpc": "2.0", "id": rid,
                                   "error": {"code": -32601, "message": "method not found"}})
        return self.send(200, {"jsonrpc": "2.0", "id": rid, "result": result})


def main():
    server =ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    sys.stdout.write("port %d\n" % server.server_address[1])
    sys.stdout.flush()
    # Exit when the test that started us closes our stdin.
    threading.Thread(target=server.serve_forever, daemon=True).start()
    try:
        sys.stdin.read()
    except KeyboardInterrupt:
        pass
    server.shutdown()


if __name__ == "__main__":
    main()
