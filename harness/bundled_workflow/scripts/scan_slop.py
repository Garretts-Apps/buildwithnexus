#!/usr/bin/env python3
"""Report Unicode that does not belong in a source tree.

Prints one finding per line: file:line:U+XXXX

The default and --report modes only print. They do not edit files
and they exit 0 even when they find something. --ci exits 1 only for
invisible characters: U+200B-U+200D, U+2060, a U+FEFF that is not the
first character, bidi controls, and tag characters U+E0000-U+E007F.
No-break spaces, line separators, smart quotes, and dashes are
printed in report mode and do not fail --ci.

This file spells those characters as integers so the kit stays clean.
"""
import argparse
import subprocess
import sys
import tempfile
from pathlib import Path

INVISIBLE = {0x200B, 0x200C, 0x200D, 0x2060}
BIDI = set(range(0x202A, 0x202F)) | set(range(0x2066, 0x206A))
TAG_FIRST = 0xE0000
TAG_LAST = 0xE007F
SPACES = {0x00A0, 0x202F, 0x2028, 0x2029}
STYLE = {0x2018, 0x2019, 0x201C, 0x201D, 0x2013, 0x2014, 0x2026}
BOM = 0xFEFF

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


def fails_ci(codepoint):
    if codepoint in STYLE or codepoint in SPACES:
        return False
    return True


def finding_line(path, line, codepoint):
    return f"{path}:{line}:U+{codepoint:04X}"


def scan_text(text):
    """Yield (line, codepoint) for every tracked character in text."""
    line = 1
    for index, char in enumerate(text):
        codepoint = ord(char)
        if char == "\n":
            line += 1
            continue
        if codepoint == BOM:
            if index != 0:
                yield line, codepoint
            continue
        if codepoint in INVISIBLE or codepoint in BIDI or codepoint in SPACES or codepoint in STYLE:
            yield line, codepoint
            continue
        if TAG_FIRST <= codepoint <= TAG_LAST:
            yield line, codepoint


def looks_binary(raw):
    return b"\0" in raw[:8192]


def iter_files(root):
    root = root.resolve()
    if root.is_file():
        if not root.is_symlink():
            yield root
        return
    for path in sorted(root.rglob("*")):
        if not path.is_file() or path.is_symlink():
            continue
        if any(part in SKIP_DIRS for part in path.relative_to(root).parts):
            continue
        yield path


def scan_tree(roots):
    findings = []
    for root in roots:
        base = root.resolve()
        anchor = base if base.is_dir() else base.parent
        for path in iter_files(base):
            raw = path.read_bytes()
            if looks_binary(raw):
                continue
            try:
                text = raw.decode("utf-8")
            except UnicodeDecodeError:
                continue
            try:
                rel = path.relative_to(anchor).as_posix()
            except ValueError:
                rel = path.as_posix()
            for line, codepoint in scan_text(text):
                findings.append((rel, line, codepoint, fails_ci(codepoint)))
    findings.sort()
    return findings


def self_test():
    script = str(Path(__file__).resolve())
    errors = []
    with tempfile.TemporaryDirectory() as tmp:
        root = Path(tmp)
        code = root / "app.py"
        code.write_text("ok\n" + "x" + chr(0x200B) + "\n", encoding="utf-8")
        prose = root / "README.md"
        prose.write_text("a " + chr(0x2014) + " b\n", encoding="utf-8")
        space = root / "note.md"
        space.write_text("a" + chr(0x00A0) + "b\n", encoding="utf-8")
        bom = root / "bom.txt"
        bom.write_text(chr(0xFEFF) + "ok\nmid " + chr(0xFEFF) + "\n", encoding="utf-8")
        bidi = root / "dir.js"
        bidi.write_text("a" + chr(0x202E) + "b\n", encoding="utf-8")
        tag = root / "tag.ts"
        tag.write_text("a" + chr(0xE0001) + "b\n", encoding="utf-8")
        narrow = root / "cfg.toml"
        narrow.write_text("a" + chr(0x2028) + "b\n", encoding="utf-8")

        report = subprocess.run(
            [sys.executable, script, "--report"],
            cwd=tmp,
            capture_output=True,
            text=True,
        )
        if report.returncode != 0:
            errors.append(f"report mode exited {report.returncode}")
        got = [line for line in report.stdout.splitlines() if line]
        expect = [
            "README.md:1:U+2014",
            "app.py:2:U+200B",
            "bom.txt:2:U+FEFF",
            "cfg.toml:1:U+2028",
            "dir.js:1:U+202E",
            "note.md:1:U+00A0",
            "tag.ts:1:U+E0001",
        ]
        if got != expect:
            errors.append("report lines:\n" + "\n".join(got))

        ci = subprocess.run(
            [sys.executable, script, "--ci"],
            cwd=tmp,
            capture_output=True,
            text=True,
        )
        if ci.returncode != 1:
            errors.append(f"ci mode exited {ci.returncode}")
        ci_lines = [line for line in ci.stdout.splitlines() if line]
        ci_expect = [
            "app.py:2:U+200B",
            "bom.txt:2:U+FEFF",
            "dir.js:1:U+202E",
            "tag.ts:1:U+E0001",
        ]
        if ci_lines != ci_expect:
            errors.append("ci lines:\n" + "\n".join(ci_lines))

        clean = root / "clean"
        clean.mkdir()
        (clean / "ok.py").write_text("print(1)\n", encoding="utf-8")
        quiet = subprocess.run(
            [sys.executable, script, "--ci", str(clean)],
            cwd=tmp,
            capture_output=True,
            text=True,
        )
        if quiet.returncode != 0 or quiet.stdout.strip():
            errors.append("clean tree should be silent")

    if errors:
        print("\n".join(errors), file=sys.stderr)
        return 2
    print("self-test ok")
    return 0


def main(argv):
    parser = argparse.ArgumentParser(description="Report stray Unicode in a tree.")
    parser.add_argument("--ci", action="store_true", help="exit 1 on invisible Unicode")
    parser.add_argument("--report", action="store_true", help="print findings and exit 0")
    parser.add_argument("--self-test", action="store_true", help="check the detector")
    parser.add_argument("paths", nargs="*", type=Path)
    args = parser.parse_args(argv)
    if args.self_test:
        return self_test()
    roots = args.paths or [Path.cwd()]
    missing = [str(path) for path in roots if not path.exists()]
    if missing:
        print("missing: " + ", ".join(missing), file=sys.stderr)
        return 2
    findings = scan_tree(roots)
    shown = findings if not args.ci else [item for item in findings if item[3]]
    for rel, line, codepoint, _fail in shown:
        print(finding_line(rel, line, codepoint))
    if args.ci and shown:
        print(f"{len(shown)} invisible Unicode finding(s)", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
