#!/usr/bin/env python3
"""Minimal MCP server over stdio for the test suite.

Speaks newline-delimited JSON-RPC 2.0: initialize, notifications/initialized,
ping, tools/list (two pages joined by a nextCursor), tools/call,
resources/list, resources/read, prompts/list and prompts/get. The listed
tool surface is exactly two tools — `echo` (mutating by default) and `add`
(annotated readOnlyHint) — so discovery tests can assert an exact count.
One resource (`file:///note.txt`) and one prompt (`review`) are published.
Two hidden tools exist only for tools/call: `fail` (isError result) and
`sleep` (hangs for `seconds`, for timeout tests).
"""
import json
import sys
import time

PAGE1 = [
    {
        "name": "echo",
        "description": "Echo text back to the caller",
        "inputSchema": {
            "type": "object",
            "properties": {"text": {"type": "string"}},
            "required": ["text"],
        },
    }
]
PAGE2 = [
    {
        "name": "add",
        "description": "Add two integers",
        "inputSchema": {
            "type": "object",
            "properties": {"a": {"type": "integer"}, "b": {"type": "integer"}},
            "required": ["a", "b"],
        },
        "annotations": {"readOnlyHint": True},
    }
]


def send(msg):
    sys.stdout.write(json.dumps(msg) + "\n")
    sys.stdout.flush()


def reply(rid, result):
    send({"jsonrpc": "2.0", "id": rid, "result": result})


def error(rid, code, message):
    send({"jsonrpc": "2.0", "id": rid, "error": {"code": code, "message": message}})


def text(s, is_error=False):
    out = {"content": [{"type": "text", "text": s}]}
    if is_error:
        out["isError"] = True
    return out


sys.stderr.write("fake-mcp: starting\n")
sys.stderr.flush()

for raw in sys.stdin:
    raw = raw.strip()
    if not raw:
        continue
    try:
        msg = json.loads(raw)
    except ValueError:
        continue
    method = msg.get("method")
    rid = msg.get("id")
    params = msg.get("params") or {}
    if method == "initialize":
        reply(
            rid,
            {
                "protocolVersion": params.get("protocolVersion", "2025-06-18"),
                "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                "serverInfo": {"name": "fake-mcp", "version": "0.1"},
            },
        )
        # A server-initiated request the client must tolerate mid-handshake.
        send({"jsonrpc": "2.0", "id": "srv-ping", "method": "ping"})
        send({"jsonrpc": "2.0", "method": "notifications/message",
              "params": {"level": "info", "data": "hello from fake-mcp"}})
    elif method == "notifications/initialized":
        pass
    elif method == "ping":
        reply(rid, {})
    elif method == "tools/list":
        cursor = params.get("cursor")
        if cursor is None:
            reply(rid, {"tools": PAGE1, "nextCursor": "page-2"})
        elif cursor == "page-2":
            reply(rid, {"tools": PAGE2})
        else:
            error(rid, -32602, "unknown cursor")
    elif method == "tools/call":
        name = params.get("name")
        args = params.get("arguments") or {}
        if name == "echo":
            reply(rid, text("echo: " + str(args.get("text", ""))))
        elif name == "add":
            reply(rid, text(str(int(args.get("a", 0)) + int(args.get("b", 0)))))
        elif name == "fail":
            reply(rid, text("boom", is_error=True))
        elif name == "image":
            reply(rid, {"content": [
                {"type": "text", "text": "before"},
                {"type": "image", "data": "AAAA", "mimeType": "image/png"},
                {"type": "text", "text": "after"},
            ]})
        elif name == "sleep":
            time.sleep(float(args.get("seconds", 5)))
            reply(rid, text("woke up"))
        else:
            error(rid, -32602, "unknown tool: %s" % name)
    elif method == "resources/list":
        reply(rid, {"resources": [{
            "uri": "file:///note.txt",
            "name": "note",
            "description": "A short note",
            "mimeType": "text/plain",
        }]})
    elif method == "resources/templates/list":
        error(rid, -32601, "method not found")
    elif method == "resources/read":
        uri = params.get("uri")
        if uri == "file:///note.txt":
            reply(rid, {"contents": [{
                "uri": uri,
                "mimeType": "text/plain",
                "text": "hello from the resource",
            }]})
        elif uri == "file:///blob.bin":
            reply(rid, {"contents": [{
                "uri": uri,
                "mimeType": "application/octet-stream",
                "blob": "YQ==",
            }]})
        else:
            error(rid, -32002, "resource not found")
    elif method == "prompts/list":
        reply(rid, {"prompts": [{
            "name": "review",
            "description": "Review a diff",
            "arguments": [{
                "name": "diff",
                "description": "the diff",
                "required": True,
            }],
        }]})
    elif method == "prompts/get":
        if params.get("name") != "review":
            error(rid, -32602, "unknown prompt")
        else:
            diff = (params.get("arguments") or {}).get("diff", "")
            reply(rid, {"messages": [{
                "role": "user",
                "content": {"type": "text", "text": "Review this:\n" + diff},
            }]})
    elif method == "exit":
        break
    elif rid is not None:
        error(rid, -32601, "method not found")
