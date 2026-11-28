"""The scripted model picks each harness's shell tool and shapes its call (roadmap 6.3)."""

from __future__ import annotations

import json
import unittest
import urllib.request

from bench.forbidden import scripted


def post(url: str, payload: dict) -> dict:
    req = urllib.request.Request(url, data=json.dumps(payload).encode(), headers={"content-type": "application/json"})
    with urllib.request.urlopen(req, timeout=10) as r:
        return json.load(r)


class PickTool(unittest.TestCase):
    def test_a_chat_tool_with_a_string_command(self) -> None:
        payload = {"tools": [
            {"type": "function", "function": {"name": "read_file", "parameters": {"properties": {"path": {"type": "string"}}}}},
            {"type": "function", "function": {"name": "bash", "parameters": {
                "properties": {"command": {"type": "string"}, "timeout": {"type": "integer"}},
                "required": ["command", "timeout"]}}},
        ]}
        tool = scripted.pick_shell_tool(payload)
        self.assertEqual(tool["name"], "bash")
        self.assertEqual(scripted.tool_arguments(tool, "echo hi"), {"command": "echo hi", "timeout": 60})

    def test_a_responses_tool_with_an_argv_command(self) -> None:
        payload = {"tools": [{"type": "function", "name": "shell", "parameters": {
            "properties": {"command": {"type": "array", "items": {"type": "string"}}}}}]}
        tool = scripted.pick_shell_tool(payload)
        self.assertEqual(scripted.tool_arguments(tool, "echo hi"), {"command": ["bash", "-lc", "echo hi"]})

    def test_no_shell_tool_is_none(self) -> None:
        self.assertIsNone(scripted.pick_shell_tool({"tools": [{"type": "function", "function": {"name": "read_file"}}]}))


class Script(unittest.TestCase):
    def setUp(self) -> None:
        self.srv = scripted.start()
        self.srv.load(["echo one", "echo two"])
        self.tools = [{"type": "function", "function": {"name": "bash", "parameters": {"properties": {"command": {"type": "string"}}}}}]

    def tearDown(self) -> None:
        self.srv.shutdown()

    def test_each_tool_result_advances_the_script_then_it_stops(self) -> None:
        url = self.srv.url + "/chat/completions"
        msgs = [{"role": "user", "content": "go"}]
        r = post(url, {"messages": msgs, "tools": self.tools})
        call = r["choices"][0]["message"]["tool_calls"][0]
        self.assertEqual(json.loads(call["function"]["arguments"]), {"command": "echo one"})
        msgs += [r["choices"][0]["message"], {"role": "tool", "tool_call_id": call["id"], "content": "one"}]
        r = post(url, {"messages": msgs, "tools": self.tools})
        self.assertEqual(json.loads(r["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"]), {"command": "echo two"})
        msgs += [r["choices"][0]["message"], {"role": "tool", "tool_call_id": "x", "content": "secret-canary"}]
        r = post(url, {"messages": msgs, "tools": self.tools})
        self.assertEqual(r["choices"][0]["message"]["content"], scripted.DONE_TEXT)
        self.assertTrue(self.srv.saw("secret-canary"))

    def test_the_responses_api_gets_a_function_call(self) -> None:
        tools = [{"type": "function", "name": "shell", "parameters": {"properties": {"command": {"type": "string"}}}}]
        r = post(self.srv.url + "/responses", {"input": [{"role": "user", "content": "go"}], "tools": tools})
        item = r["output"][0]
        self.assertEqual((item["type"], item["name"]), ("function_call", "shell"))
        self.assertEqual(json.loads(item["arguments"]), {"command": "echo one"})

    def test_without_tools_the_command_comes_in_a_fenced_block(self) -> None:
        r = post(self.srv.url + "/chat/completions", {"messages": [{"role": "user", "content": "go"}]})
        self.assertIn("```bash\necho one\n```", r["choices"][0]["message"]["content"])


if __name__ == "__main__":
    unittest.main()
