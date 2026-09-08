#!/usr/bin/env python3
"""Exercise the real TUI in a Unix PTY, without an API key or model server.

Usage: python3 scripts/test-terminal-input.py target/debug/buildwithnexus
"""

import errno
import fcntl
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
import time
import unittest


BINARY = str(Path(sys.argv.pop(1)).resolve())


class TerminalInputTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="bwn-terminal-test-")
        self.root = Path(self.temp.name)
        self.home = self.root / "home"
        self.home.mkdir()
        (self.home / "config.json").write_text(json.dumps({
            "provider": "ollama",
            "model": "test-model",
            "base_url": "http://127.0.0.1:9/v1",
            "permission": "readonly",
            "auto_update": "off",
        }))
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.proc = subprocess.Popen(
            [BINARY], stdin=slave, stdout=slave, stderr=slave,
            cwd=self.root, start_new_session=True,
            env={**os.environ, "NEXUS_HOME": str(self.home),
                 "TERM": "xterm-256color", "NO_COLOR": "1"},
        )
        os.close(slave)
        self.output = bytearray()
        self.addCleanup(self.close_terminal)
        self.wait_for(lambda: b"describe a task" in self.output, "startup")
        self.pump(0.1)

    def close_terminal(self):
        if self.proc.poll() is None:
            os.killpg(self.proc.pid, signal.SIGTERM)
        try:
            self.proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            os.killpg(self.proc.pid, signal.SIGKILL)
            self.proc.wait(timeout=3)
        os.close(self.master)
        self.temp.cleanup()

    def pump(self, duration):
        deadline = time.monotonic() + duration
        while time.monotonic() < deadline:
            if not select.select([self.master], [], [], max(0, deadline - time.monotonic()))[0]:
                break
            try:
                chunk = os.read(self.master, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    break
                raise
            if not chunk:
                break
            self.output.extend(chunk)
            if b"\x1b[6n" in chunk:
                os.write(self.master, b"\x1b[1;1R")

    def wait_for(self, predicate, label):
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            if predicate():
                return
            self.pump(0.05)
        self.fail(f"Timed out waiting for {label}: {bytes(self.output[-1500:])!r}")

    def send(self, text):
        os.write(self.master, text.encode())
        self.pump(0.1)

    def assert_submitted(self, expected):
        def saved():
            path = self.home / "history"
            return path.exists() and expected in [
                line.strip() for line in path.read_text().splitlines()
            ]
        self.wait_for(saved, f"submitted prompt {expected!r}")

    def test_mode_change_preserves_draft_and_cursor(self):
        self.send("/mouse statuz")
        self.send("\x1b[D")  # put the cursor before the final z
        self.send("\x1b[Z")  # Shift+Tab
        self.send("\x1b[3~s\r\r")  # replace z, accept the suggestion, then submit
        self.assert_submitted("/mouse status")

    def test_mode_change_preserves_multiline_draft(self):
        self.send("/mouse \\\r")
        self.send("status")
        self.send("\x1b[Z")
        self.send("\r")
        self.assert_submitted("/mouse  status")  # history flattens newlines

    def test_tab_completion_in_middle_of_command(self):
        self.send("/help")
        self.send("\x1b[D\x1b[D")  # /he|lp
        self.send("\t\r")
        self.assert_submitted("/help")

    def test_enter_completion_in_middle_of_command(self):
        self.send("/help")
        self.send("\x1b[D\x1b[D")
        self.send("\r")
        self.assert_submitted("/help")


if __name__ == "__main__":
    unittest.main()
