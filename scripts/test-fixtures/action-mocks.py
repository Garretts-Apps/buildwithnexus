"""Mock model and GitHub API for the GitHub Action's CI job (ci.yml, job
`action`). Python standard library only.

  model   OpenAI-compatible chat completions on --model-port. The reply depends
          on the last message: "create <file>" writes it, a review task gets
          one blocking and one minor finding, a tool result gets a one-line
          summary, anything else a short answer.
  github  GET and POST /repos/<owner>/<repo>/issues/<n>/comments and PATCH
          /repos/<owner>/<repo>/issues/comments/<id> on --github-port, kept in
          memory, written by github-actions[bot]; GET /user answers 403, as
          it does for the workflow's own token. Every request is appended to <log>/github.jsonl and every
          model request to <log>/model.jsonl.

usage: action-mocks.py --model-port N --github-port N --log DIR
"""
import argparse
import http.server
import json
import os
import re
import socketserver
import threading

ARGS = None
LOCK = threading.Lock()
COMMENTS = []


def log(name, entry):
    with LOCK, open(os.path.join(ARGS.log, name), "a") as f:
        f.write(json.dumps(entry) + "\n")


def last_user_text(msgs):
    for m in reversed(msgs):
        if m.get("role") == "user":
            c = m.get("content")
            return c if isinstance(c, str) else " ".join(p.get("text", "") for p in c or [] if isinstance(p, dict))
    return ""


def reply_for(body):
    msgs = body.get("messages", [])
    if msgs and msgs[-1].get("role") == "tool":
        first = str(msgs[-1].get("content", "")).splitlines()[:1]
        return {"text": "Done: " + (first[0] if first else "the tool ran") + "."}
    task = last_user_text(msgs)
    if "Review this change" in task:
        return {"text": "Looked at the diff.\n"
                        "- [blocking] hello.txt:1 — the greeting names nobody\n"
                        "- [minor] hello.txt:1 — say who wrote it"}
    m = re.search(r"create ([\w.-]+)", task)
    if m:
        return {"tool": ("write_file", {"path": m.group(1), "content": "hello\n"})}
    return {"text": "Nothing to do here."}


class Base(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def send_json(self, code, obj):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)

    def body(self):
        n = int(self.headers.get("Content-Length", 0))
        return json.loads(self.rfile.read(n) or b"{}")


class Model(Base):
    def do_GET(self):
        self.send_json(200, {"object": "list", "data": [{"id": "mock-coder", "object": "model"}]})

    def do_POST(self):
        body = self.body()
        r = reply_for(body)
        log("model.jsonl", {"path": self.path, "stream": body.get("stream"), "reply": r})
        usage = {"prompt_tokens": 40, "completion_tokens": 10}
        if "tool" in r:
            name, args = r["tool"]
            call = {"id": "call_1", "type": "function", "function": {"name": name, "arguments": json.dumps(args)}}
            message, finish = {"role": "assistant", "content": None, "tool_calls": [call]}, "tool_calls"
        else:
            message, finish = {"role": "assistant", "content": r["text"]}, "stop"
        if not body.get("stream"):
            return self.send_json(200, {"choices": [{"index": 0, "message": message, "finish_reason": finish}], "usage": usage})
        delta = dict(message)
        if "tool_calls" in delta:
            delta["tool_calls"] = [dict(c, index=0) for c in delta["tool_calls"]]
        chunks = [{"choices": [{"index": 0, "delta": delta}]},
                  {"choices": [{"index": 0, "delta": {}, "finish_reason": finish}]},
                  {"choices": [], "usage": usage}]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for c in chunks:
            self.wfile.write(b"data: " + json.dumps(c).encode() + b"\n\n")
        self.wfile.write(b"data: [DONE]\n\n")


class GitHub(Base):
    def record(self, body=None):
        log("github.jsonl", {"method": self.command, "path": self.path,
                             "auth": self.headers.get("Authorization", ""), "body": body})

    def authorized(self):
        if not self.headers.get("Authorization", "").startswith("Bearer "):
            self.send_json(401, {"message": "Requires authentication"})
            return False
        return True

    def do_GET(self):
        self.record()
        if not self.authorized():
            return
        if self.path == "/user":
            return self.send_json(403, {"message": "Resource not accessible by integration"})
        if re.fullmatch(r"/repos/[^/]+/[^/]+/issues/\d+/comments(\?.*)?", self.path):
            with LOCK:
                return self.send_json(200, list(COMMENTS))
        self.send_json(404, {"message": "Not Found"})

    def do_POST(self):
        body = self.body()
        self.record(body)
        if not self.authorized():
            return
        if re.fullmatch(r"/repos/[^/]+/[^/]+/issues/\d+/comments", self.path):
            with LOCK:
                c = {"id": len(COMMENTS) + 1, "body": body.get("body", ""),
                     "user": {"login": "github-actions[bot]", "type": "Bot"}}
                COMMENTS.append(c)
            return self.send_json(201, c)
        self.send_json(404, {"message": "Not Found"})

    def do_PATCH(self):
        body = self.body()
        self.record(body)
        if not self.authorized():
            return
        m = re.fullmatch(r"/repos/[^/]+/[^/]+/issues/comments/(\d+)", self.path)
        with LOCK:
            c = next((c for c in COMMENTS if m and c["id"] == int(m.group(1))), None)
            if c:
                c["body"] = body.get("body", "")
                return self.send_json(200, c)
        self.send_json(404, {"message": "Not Found"})


class Server(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True


def main():
    global ARGS
    p = argparse.ArgumentParser()
    p.add_argument("--model-port", type=int, required=True)
    p.add_argument("--github-port", type=int, required=True)
    p.add_argument("--log", required=True)
    ARGS = p.parse_args()
    os.makedirs(ARGS.log, exist_ok=True)
    github = Server(("127.0.0.1", ARGS.github_port), GitHub)
    threading.Thread(target=github.serve_forever, daemon=True).start()
    Server(("127.0.0.1", ARGS.model_port), Model).serve_forever()


if __name__ == "__main__":
    main()
