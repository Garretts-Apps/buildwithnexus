#!/usr/bin/env python3
"""Report likely secret patterns in text files. Stdlib only; read-only."""
import argparse
import re
import sys
from pathlib import Path

SKIP_DIRS = {
    ".git",
    "node_modules",
    "dist",
    "build",
    "target",
    "vendor",
    ".venv",
    "venv",
    "__pycache__",
    "coverage",
    ".next",
}
PATTERNS = [
    (r"\bsk-[A-Za-z0-9]{20,}\b", "api_key_shape"),
    (r"\bAKIA[0-9A-Z]{16}\b", "aws_access_key"),
    (r"\bghp_[A-Za-z0-9]{20,}\b", "github_token"),
    (r"(?i)-----BEGIN (?:RSA |EC |OPENSSH )?PRIVATE KEY-----", "private_key"),
    (r"(?i)(password|api[_-]?key|secret|token)\s*[:=]\s*['\"]?\S{8,}", "labeled_secret"),
]


def iter_files(root: Path):
    root = root.resolve()
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        if any(p in SKIP_DIRS for p in path.parts):
            continue
        try:
            if b"\0" in path.read_bytes()[:4096]:
                continue
        except OSError:
            continue
        yield path


def scan(root: Path):
    findings = []
    for path in iter_files(root):
        try:
            text = path.read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        rel = path.relative_to(root).as_posix()
        for i, line in enumerate(text.splitlines(), 1):
            for pat, label in PATTERNS:
                if re.search(pat, line):
                    findings.append((rel, i, label))
    return findings


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--report", action="store_true")
    parser.add_argument("paths", nargs="*", type=Path)
    args = parser.parse_args()
    roots = args.paths or [Path.cwd()]
    all_f = []
    for root in roots:
        if not root.exists():
            print(f"missing: {root}", file=sys.stderr)
            return 2
        base = root if root.is_dir() else root.parent
        all_f.extend(scan(base))
    for rel, line, label in sorted(all_f):
        print(f"{rel}:{line}:{label}")
    return 0


if __name__ == "__main__":
    sys.exit(main())