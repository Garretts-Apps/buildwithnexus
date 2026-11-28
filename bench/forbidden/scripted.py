"""A scripted OpenAI-compatible model that makes shell tool calls (roadmap 6.3).

Each run gets a script: a list of shell commands. On every request the model
looks at the tools the harness offered, picks the one that runs shell
commands, counts the tool results already in the conversation, and calls
that tool with the next command, or finishes with plain text once the
script is done. It speaks chat completions and the Responses API, streaming
and not, so the same script reaches every harness that can be pointed at an
OpenAI-compatible endpoint.

The model does no reasoning, so the commands are the same for every
harness. Every request body is kept, so a test can look for a canary that
should never have reached the model.
"""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

DONE_TEXT = "SCRIPT_DONE"

# Shell-running tool names, most specific first. Matched case-insensitively
# against the tool names a harness offers.
SHELL_TOOL_NAMES = (
    "bash",
    "shell",
    "run_shell_command",
    "exec_command",
    "run_command",
    "execute_command",
    "terminal",
    "local_shell",
    "run_in_terminal",
    "command",
)
COMMAND_KEYS = ("command", "cmd", "script", "commands", "input")


def _tool_list(payload: dict) -> list[dict]:
    """Every offered tool as {name, parameters, kind}."""
    out = []
    for t in payload.get("tools") or []:
        if not isinstance(t, dict):
            continue
        if t.get("type") == "local_shell":
            out.append({"name": "local_shell", "parameters": None, "kind": "local_shell"})
            continue
        fn = t.get("function") if isinstance(t.get("function"), dict) else t
        name = fn.get("name")
        if name:
            kind = "custom" if t.get("type") == "custom" else "function"
            out.append({"name": name, "parameters": fn.get("parameters") or {}, "kind": kind})
    return out


def pick_shell_tool(payload: dict) -> dict | None:
    tools = _tool_list(payload)
    by_name = {t["name"].lower(): t for t in tools}
    for want in SHELL_TOOL_NAMES:
        if want in by_name:
            return by_name[want]
    for t in tools:
        if any(w in t["name"].lower() for w in ("bash", "shell", "exec", "terminal")):
            return t
    return None


def _default_for(schema: dict) -> object:
    if "default" in schema:
        return schema["default"]
    if schema.get("enum"):
        return schema["enum"][0]
    ty = schema.get("type")
    if isinstance(ty, list):
        ty = next((x for x in ty if x != "null"), "string")
    return {"string": "bench", "boolean": False, "integer": 60, "number": 60, "array": [], "object": {}}.get(ty, "bench")


def tool_arguments(tool: dict, command: str) -> dict | str:
    """Arguments for `tool` that run `command`, shaped by its own schema."""
    if tool["kind"] == "local_shell":
        return {"command": ["bash", "-lc", command]}
    if tool["kind"] == "custom":
        return command
    params = tool.get("parameters") or {}
    props = params.get("properties") or {}
    key = next((k for k in COMMAND_KEYS if k in props), None)
    if key is None:
        key = next((k for k, v in props.items() if (v or {}).get("type") in ("string", ["string"])), "command")
    schema = props.get(key) or {}
    value: object = command
    if schema.get("type") == "array":
        value = ["bash", "-lc", command]
    args: dict = {key: value}
    for req in params.get("required") or []:
        if req not in args:
            args[req] = _default_for(props.get(req) or {})
    # A description field, where offered, says what the call is for.
    for k in ("description", "explanation", "reason"):
        if k in props and k not in args:
            args[k] = "scripted benchmark step"
    return args


def tool_results_so_far(payload: dict) -> int:
    n = 0
    for m in payload.get("messages") or []:
        if isinstance(m, dict) and m.get("role") == "tool":
            n += 1
    for item in payload.get("input") or []:
        if isinstance(item, dict) and str(item.get("type", "")).endswith("call_output"):
            n += 1
    return n


class ScriptedServer(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), Handler)
        self.script: list[str] = []
        self.bodies: list[bytes] = []
        self.calls: list[dict] = []
        self.no_shell_tool: list[list[str]] = []
        self.lock = threading.Lock()

    @property
    def url(self) -> str:
        return f"http://127.0.0.1:{self.server_address[1]}/v1"

    def load(self, script: list[str]) -> None:
        with self.lock:
            self.script = list(script)
            self.bodies = []
            self.calls = []
            self.no_shell_tool = []

    def saw(self, needle: str) -> bool:
        b = needle.encode()
        with self.lock:
            return any(b in body for body in self.bodies)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"
    server: ScriptedServer

    def log_message(self, fmt: str, *args) -> None:
        return

    def handle(self) -> None:
        try:
            super().handle()
        except (ConnectionResetError, BrokenPipeError):
            return

    def do_GET(self) -> None:  # noqa: N802
        ids = ["bench", "gpt-4o-mini", "gpt-4o", "gpt-5.4-mini"]
        self._json({"object": "list", "data": [{"id": i, "object": "model", "owned_by": "bench"} for i in ids]})

    def do_POST(self) -> None:  # noqa: N802
        n = int(self.headers.get("Content-Length", "0") or 0)
        body = self.rfile.read(n) if n else b""
        try:
            payload = json.loads(body or b"{}")
        except json.JSONDecodeError:
            payload = {}
        srv = self.server
        tool = pick_shell_tool(payload)
        done = tool_results_so_far(payload)
        with srv.lock:
            srv.bodies.append(body)
            step = srv.script[done] if done < len(srv.script) else None
            if tool is None and payload.get("tools"):
                srv.no_shell_tool.append([t["name"] for t in _tool_list(payload)])
            if step is not None and tool is not None:
                srv.calls.append({"step": done, "tool": tool["name"], "command": step})
        responses = "responses" in self.path
        stream = bool(payload.get("stream"))
        if tool is None and not payload.get("tools"):
            # A harness that offers no tools (Aider) runs shell commands it
            # finds in a fenced block, when its unattended flag says yes.
            # Steps are counted by the assistant turns already taken.
            done = sum(1 for m in payload.get("messages") or [] if isinstance(m, dict) and m.get("role") == "assistant")
            with srv.lock:
                step = srv.script[done] if done < len(srv.script) else None
                if step is not None:
                    srv.calls.append({"step": done, "tool": "fenced-block", "command": step})
            text = f"Running this:\n\n```bash\n{step}\n```\n" if step is not None else DONE_TEXT
            self._text(responses, stream, text)
            return
        if step is None or tool is None:
            self._text(responses, stream)
        else:
            self._call(responses, stream, tool, tool_arguments(tool, step), done)

    # Replies -----------------------------------------------------------

    def _json(self, obj: dict) -> None:
        raw = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def _sse(self, events: list[tuple[str | None, dict]], done_marker: bool) -> None:
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        self.end_headers()
        for name, obj in events:
            head = f"event: {name}\n" if name else ""
            self.wfile.write(f"{head}data: {json.dumps(obj)}\n\n".encode())
        if done_marker:
            self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True

    def _text(self, responses: bool, stream: bool, text: str = DONE_TEXT) -> None:
        if not responses:
            msg = {"role": "assistant", "content": text}
            if not stream:
                self._json(_chat(msg, "stop"))
                return
            self._sse([(None, _chunk({"role": "assistant", "content": text}, None)), (None, _chunk({}, "stop"))], True)
            return
        item = {
            "id": "msg_s",
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": text, "annotations": []}],
        }
        self._response([item], stream)

    def _call(self, responses: bool, stream: bool, tool: dict, args: dict | str, step: int) -> None:
        call_id = f"call_s{step}"
        arg_text = args if isinstance(args, str) else json.dumps(args)
        if not responses:
            tc = {"id": call_id, "type": "function", "function": {"name": tool["name"], "arguments": arg_text}}
            if not stream:
                self._json(_chat({"role": "assistant", "content": None, "tool_calls": [tc]}, "tool_calls"))
                return
            first = {"role": "assistant", "content": None, "tool_calls": [{"index": 0, **tc}]}
            self._sse([(None, _chunk(first, None)), (None, _chunk({}, "tool_calls"))], True)
            return
        if tool["kind"] == "local_shell":
            item = {"id": f"lsh_s{step}", "type": "local_shell_call", "call_id": call_id, "status": "completed",
                    "action": {"type": "exec", **args}}
        elif tool["kind"] == "custom":
            item = {"id": f"ctc_s{step}", "type": "custom_tool_call", "call_id": call_id, "name": tool["name"],
                    "input": arg_text, "status": "completed"}
        else:
            item = {"id": f"fc_s{step}", "type": "function_call", "call_id": call_id, "name": tool["name"],
                    "arguments": arg_text, "status": "completed"}
        self._response([item], stream)

    def _response(self, output: list[dict], stream: bool) -> None:
        full = {"id": "resp_s", "object": "response", "status": "completed", "model": "bench", "output": output,
                "usage": {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}}
        if not stream:
            self._json(full)
            return
        events: list[tuple[str | None, dict]] = [
            ("response.created", {"type": "response.created", "response": {**full, "status": "in_progress", "output": []}}),
        ]
        for i, item in enumerate(output):
            events.append(("response.output_item.added", {"type": "response.output_item.added", "output_index": i,
                                                          "item": {**item, "status": "in_progress"}}))
            if item["type"] == "function_call":
                events.append(("response.function_call_arguments.done", {
                    "type": "response.function_call_arguments.done", "item_id": item["id"], "output_index": i,
                    "arguments": item["arguments"]}))
            if item["type"] == "message":
                text = item["content"][0]["text"]
                events.append(("response.output_text.delta", {"type": "response.output_text.delta", "item_id": item["id"],
                                                              "output_index": i, "content_index": 0, "delta": text}))
            events.append(("response.output_item.done", {"type": "response.output_item.done", "output_index": i, "item": item}))
        events.append(("response.completed", {"type": "response.completed", "response": full}))
        self._sse(events, False)


def _chat(message: dict, finish: str) -> dict:
    return {"id": "chatcmpl-s", "object": "chat.completion", "created": 0, "model": "bench",
            "choices": [{"index": 0, "message": message, "finish_reason": finish}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}}


def _chunk(delta: dict, finish: str | None) -> dict:
    return {"id": "chatcmpl-s", "object": "chat.completion.chunk", "created": 0, "model": "bench",
            "choices": [{"index": 0, "delta": delta, "finish_reason": finish}]}


def start() -> ScriptedServer:
    srv = ScriptedServer()
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    return srv
