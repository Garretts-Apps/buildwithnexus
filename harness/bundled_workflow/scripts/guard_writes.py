#!/usr/bin/env python3
"""PreToolUse guard: block writes outside the session working tree. Exit 2 to block."""
import json
import os
import sys
from pathlib import Path

BLOCKED = [
    "/etc",
    "/usr",
    "/bin",
    "/sbin",
    "/var",
    "/sys",
    "/proc",
    "/dev",
]
if os.name == "nt":
    BLOCKED.extend(
        [
            r"C:\Windows",
            r"C:\Program Files",
            r"C:\Program Files (x86)",
        ]
    )


def deny(msg):
    print(msg, file=sys.stderr)
    sys.exit(2)


def resolve_under(cwd: Path, target: Path) -> bool:
    try:
        cwd = cwd.resolve()
        target = target.resolve()
    except OSError:
        return False
    try:
        target.relative_to(cwd)
        return True
    except ValueError:
        return False


def main():
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)
    cwd_s = payload.get("cwd") or os.getcwd()
    cwd = Path(cwd_s)
    ti = payload.get("tool_input") or {}
    fp = ti.get("file_path") or ti.get("path")
    if not fp:
        sys.exit(0)
    path = Path(fp)
    if not path.is_absolute():
        path = cwd / path
    norm = os.path.normcase(str(path))
    for prefix in BLOCKED:
        if norm.startswith(os.path.normcase(prefix)):
            deny(f"blocked write outside project tree: {fp}")
    if not resolve_under(cwd, path):
        deny(f"blocked write outside cwd: {fp}")
    sys.exit(0)


if __name__ == "__main__":
    main()