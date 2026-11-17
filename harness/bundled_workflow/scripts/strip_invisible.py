#!/usr/bin/env python3
"""PostToolUse: strip invisible Unicode from a written file. Stdlib only; Windows-safe."""
import json
import os
import sys
from pathlib import Path

STRIP = {0x200B, 0x200C, 0x200D, 0x2060, 0xFEFF}
BIDI = set(range(0x202A, 0x202F)) | set(range(0x2066, 0x206A))
TAG = set(range(0xE0000, 0xE007F + 1))


def scrub(text):
    out = []
    for ch in text:
        cp = ord(ch)
        if cp in STRIP or cp in BIDI or cp in TAG:
            continue
        if cp == 0xFEFF and not out:
            continue
        out.append(ch)
    return "".join(out)


def main():
    try:
        payload = json.load(sys.stdin)
    except json.JSONDecodeError:
        sys.exit(0)
    ti = payload.get("tool_input") or {}
    fp = ti.get("file_path") or ti.get("path")
    if not fp:
        sys.exit(0)
    cwd = Path(payload.get("cwd") or os.getcwd())
    path = Path(fp)
    if not path.is_absolute():
        path = cwd / path
    if not path.is_file():
        sys.exit(0)
    try:
        raw = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        sys.exit(0)
    cleaned = scrub(raw)
    if cleaned != raw:
        path.write_text(cleaned, encoding="utf-8", newline="")
    sys.exit(0)


if __name__ == "__main__":
    main()