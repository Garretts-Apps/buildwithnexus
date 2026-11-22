#!/usr/bin/env python3
"""Rank simple entropy hotspots in a source tree. Report only; stdlib; Windows-safe."""
import argparse
import hashlib
import subprocess
import sys
from collections import Counter, defaultdict
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
    "out",
    ".turbo",
    ".cache",
}
TEXT_SUFFIX = {
    ".py",
    ".rs",
    ".go",
    ".js",
    ".jsx",
    ".ts",
    ".tsx",
    ".java",
    ".cs",
    ".md",
    ".yaml",
    ".yml",
    ".json",
    ".toml",
    ".css",
    ".scss",
    ".html",
    ".sh",
}
LINE_BLOCK = 5
MIN_BLOCK_LEN = 40
LARGE_FILE = 400


def iter_sources(root: Path):
    root = root.resolve()
    if root.is_file():
        yield root
        return
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        if any(p in SKIP_DIRS for p in path.parts):
            continue
        if path.suffix.lower() not in TEXT_SUFFIX:
            continue
        if looks_binary(path):
            continue
        yield path


def looks_binary(path: Path) -> bool:
    try:
        chunk = path.read_bytes()[:8192]
    except OSError:
        return True
    return b"\0" in chunk


def read_lines(path: Path):
    try:
        return path.read_text(encoding="utf-8", errors="replace").splitlines()
    except OSError:
        return []


def large_files(root: Path):
    for path in iter_sources(root):
        lines = read_lines(path)
        if len(lines) >= LARGE_FILE:
            rel = path.relative_to(root).as_posix()
            yield ("LARGE_FILE", rel, len(lines))


def duplicate_blocks(root: Path):
    seen = {}
    dups = []
    for path in iter_sources(root):
        lines = read_lines(path)
        rel = path.relative_to(root).as_posix()
        for i in range(len(lines) - LINE_BLOCK + 1):
            block = [ln.strip() for ln in lines[i : i + LINE_BLOCK]]
            if not any(block):
                continue
            text = "\n".join(block)
            if len(text) < MIN_BLOCK_LEN:
                continue
            digest = hashlib.sha256(text.encode("utf-8")).hexdigest()[:16]
            key = (digest, text[:60])
            if digest in seen and seen[digest] != rel:
                dups.append(("DUP_BLOCK", rel, f"{seen[digest]} vs {rel}:{i + 1}"))
            else:
                seen[digest] = rel
    return dups


def git_churn(root: Path, limit: int = 15):
    try:
        out = subprocess.run(
            [
                "git",
                "-C",
                str(root),
                "log",
                "--since=90.days",
                "--pretty=format:",
                "--name-only",
            ],
            capture_output=True,
            text=True,
            timeout=60,
        )
    except (OSError, subprocess.TimeoutExpired):
        return []
    if out.returncode != 0:
        return []
    counts = Counter(p for p in out.stdout.splitlines() if p.strip())
    ranked = counts.most_common(limit)
    return [("GIT_CHURN", path, count) for path, count in ranked]


def scan(root: Path):
    findings = []
    findings.extend(large_files(root))
    findings.extend(duplicate_blocks(root))
    findings.extend(git_churn(root))
    return findings


def main():
    parser = argparse.ArgumentParser(description="Report entropy hotspots.")
    parser.add_argument("paths", nargs="*", type=Path, default=[Path.cwd()])
    parser.add_argument("--report", action="store_true", help="print and exit 0")
    args = parser.parse_args()
    all_findings = []
    for path in args.paths:
        if not path.exists():
            print(f"missing: {path}", file=sys.stderr)
            return 2
        base = path if path.is_dir() else path.parent
        all_findings.extend(scan(base))
    def sort_key(item):
        kind, where, detail = item
        if kind == "LARGE_FILE":
            return (0, -int(detail), where)
        if kind == "GIT_CHURN":
            return (1, -int(detail), where)
        return (2, 0, where)

    for kind, where, detail in sorted(all_findings, key=sort_key):
        print(f"{kind}\t{where}\t{detail}")
    return 0


if __name__ == "__main__":
    sys.exit(main())