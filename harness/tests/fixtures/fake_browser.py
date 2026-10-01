#!/usr/bin/env python3
"""Stands in for `xdg-open` / `open` in the MCP OAuth tests.

Invoked as `<opener> URL`: appends the URL to $FAKE_BROWSER_LOG, then loads
it in a detached child (following redirects, as a browser would) and exits at
once, like a real opener. The child appends "status <code> <body>" for the
page it lands on, which is the loopback callback's answer.
"""
import os
import subprocess
import sys
import urllib.error
import urllib.request

LOG = os.environ.get("FAKE_BROWSER_LOG")


def note(line):
    if LOG:
        with open(LOG, "a") as f:
            f.write(line + "\n")


if len(sys.argv) >= 3 and sys.argv[1] == "--fetch":
    try:
        with urllib.request.urlopen(sys.argv[2], timeout=20) as r:
            note("status %d %s" % (r.status, r.read().decode(errors="replace")[:200]))
    except urllib.error.HTTPError as e:
        note("status %d %s" % (e.code, e.read().decode(errors="replace")[:200]))
    except Exception as e:  # noqa: BLE001 - recorded for the test to read
        note("error %s" % e)
    sys.exit(0)

url = sys.argv[1] if len(sys.argv) > 1 else ""
note("open " + url)
subprocess.Popen(
    [sys.executable, os.path.abspath(__file__), "--fetch", url],
    stdin=subprocess.DEVNULL,
    stdout=subprocess.DEVNULL,
    stderr=subprocess.DEVNULL,
    start_new_session=True,
)
