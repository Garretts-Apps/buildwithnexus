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


class TerminalHarness(unittest.TestCase):
    def extra_env(self):
        return {}

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
        # A fake $EDITOR for the Ctrl+G test: replaces the draft with two lines.
        self.editor = self.root / "editor.sh"
        self.editor.write_text("#!/bin/sh\nprintf 'line one\\nline two\\n' > \"$1\"\n")
        self.editor.chmod(0o755)
        env = {**os.environ, "NEXUS_HOME": str(self.home),
               "TERM": "xterm-256color", "NO_COLOR": "1",
               "EDITOR": str(self.editor)}
        env.pop("VISUAL", None)
        for key in ("TMUX", "KITTY_WINDOW_ID", "GHOSTTY_RESOURCES_DIR", "BWN_IMAGES"):
            env.pop(key, None)
        for key, value in self.extra_env().items():
            if value is None:
                env.pop(key, None)
            else:
                env[key] = value
        self.master, slave = pty.openpty()
        fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 30, 100, 0, 0))
        self.proc = subprocess.Popen(
            [BINARY], stdin=slave, stdout=slave, stderr=slave,
            cwd=self.root, start_new_session=True, env=env,
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


class TerminalInputTests(TerminalHarness):
    def test_pasted_image_path_becomes_attachment_token(self):
        # Drag-and-drop / paste of a screenshot path (with a space, as macOS
        # names them) → a quoted @token in the composer, ready to send.
        shot = self.root / "Screenshot 1.png"
        tiny_png(shot)
        self.send(f"\x1b[200~'{shot}'\x1b[201~")
        token = f'@"{shot}" '.encode()
        self.wait_for(lambda: token in self.output, "attachment token in composer")
        # Ordinary pasted text is left alone.
        self.send("\x1b[200~plain words\x1b[201~")
        self.wait_for(lambda: b"plain words" in self.output, "plain paste")

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

    def test_editor_keeps_newlines(self):
        self.send("draft")
        self.send("\x07")  # Ctrl+G opens $EDITOR, which writes two lines
        self.assert_submitted("line one line two")  # history flattens newlines
        # The transcript echoes the prompt as two rows, never flattened.
        self.wait_for(lambda: b"line two" in self.output, "multi-line echo")
        self.assertNotIn(b"line one line two", bytes(self.output))


class InlineImageTests(TerminalHarness):
    """The kitty graphics path, forced on: the PNG is uploaded once as a
    virtual placement and the transcript row carries Unicode placeholders."""

    def extra_env(self):
        return {"NO_COLOR": None, "COLORTERM": "truecolor", "BWN_IMAGES": "kitty"}

    def test_pasted_png_is_transmitted_with_placeholders(self):
        shot = self.root / "shot.png"
        tiny_png(shot, width=4, height=2)
        self.send(f"\x1b[200~{shot}\x1b[201~")
        self.wait_for(
            lambda: b"\x1b_Ga=T,f=100,t=d,q=2,U=1,i=1,c=" in self.output,
            "kitty transmit command",
        )
        out = bytes(self.output)
        self.assertIn("\U0010EEEE".encode(), out)  # placeholder cells
        self.assertIn("\u0305".encode(), out)  # row/column diacritic 0
        self.assertIn(b"shot.png", out)  # the header line names the file
        self.assertIn(b"4\xc3\x972", out)  # "4×2" pixel size
        # A 4×2 px image in one cell: c=1,r=1 (never upscaled).
        self.assertIn(b",c=1,r=1,m=0;", out)
        # Leaving the screen frees the upload (Esc first: clear the draft).
        self.send("\x1b")
        self.pump(0.2)
        self.send("/exit\r")
        self.wait_for(lambda: b"\x1b_Ga=d,d=A,q=2\x1b\\" in self.output, "delete-all on exit")


class CliArgumentTests(unittest.TestCase):
    def run_cli(self, *args):
        with tempfile.TemporaryDirectory(prefix="bwn-cli-test-") as home:
            return subprocess.run(
                [BINARY, *args], capture_output=True, text=True, timeout=10,
                env={**os.environ, "NEXUS_HOME": home, "NO_COLOR": "1"},
            )

    def test_unknown_option_exits_2_without_launching(self):
        result = self.run_cli("--modle", "x")
        self.assertEqual(result.returncode, 2)
        self.assertIn("buildwithnexus: unknown option '--modle'; see --help",
                      result.stderr)
        self.assertEqual(result.stdout, "")

    def test_known_flags_still_work(self):
        result = self.run_cli("--version")
        self.assertEqual(result.returncode, 0)
        self.assertTrue(result.stdout.startswith("buildwithnexus "))


if __name__ == "__main__":
    unittest.main()
