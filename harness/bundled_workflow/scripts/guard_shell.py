#!/usr/bin/env python3
"""PreToolUse guard for risky shell commands. Reads hook JSON from stdin. Exit 2 to block."""
import json
import re
import shlex
import sys


def command_from(payload):
    ti = payload.get("tool_input") or {}
    return (ti.get("command") or "").strip()


def deny(msg):
    print(msg, file=sys.stderr)
    sys.exit(2)


def main():
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)
    cmd = command_from(payload)
    if not cmd:
        sys.exit(0)
    lower = cmd.lower()
    risky = [
        (r"\brm\s+(-[^\s]*\s+)*-[^\s]*r[^\s]*\s+(/|\~|\\\\)", "recursive delete of root or home"),
        (r"\bmkfs\b", "filesystem format"),
        (r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;", "fork bomb pattern"),
        (r"\bgit\s+push\b[^\n]*\s+--force\b[^\n]*\b(main|master)\b", "force push to default branch"),
        (r">\s*/dev/[sh]d[a-z]", "raw disk write"),
        (r"\bcurl\b[^\n]*\|\s*(ba)?sh\b", "pipe download into shell"),
        (r"\bwget\b[^\n]*\|\s*(ba)?sh\b", "pipe download into shell"),
        (r"\bshutdown\b", "shutdown command"),
        (r"\breboot\b", "reboot command"),
    ]
    for pat, label in risky:
        if re.search(pat, lower, re.I):
            deny(f"blocked shell: {label}")
    if re.search(r"\bformat\s+[a-z]:", lower):
        deny("blocked shell: format drive")
    if re.search(r"\bdel\s+/[fq]\s+", lower) or re.search(r"\brd\s+/s\b", lower):
        if re.search(r"[c-z]:\\", lower, re.I) or "\\windows" in lower:
            deny("blocked shell: destructive Windows tree delete")
    try:
        parts = shlex.split(cmd)
    except ValueError:
        parts = cmd.split()
    if parts and parts[0] in ("rm", "unlink") and "-r" in "".join(parts):
        if any(p in ("/", "~", "..") for p in parts):
            deny("blocked shell: broad recursive delete")
    sys.exit(0)


if __name__ == "__main__":
    main()