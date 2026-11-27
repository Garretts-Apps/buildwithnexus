#!/usr/bin/env python3
"""The invariant suite as a score (roadmap 1.13, analysis 9.3).

Runs every `invariant_*` test once per sandbox backend this machine has
(BWN_TEST_SANDBOX_BACKEND: bwrap, landlock, none on Linux; the native one
elsewhere) and counts, per acceptance case (AU-1, IN-2, ...) and backend,
how many of the case's tests pass. That is the "unauthorised actions
blocked" metric from the mandate, measured on bwn. It writes
`invariants.json` and `invariants.md` under bench/results/<UTC time>/.

    python3 bench/invariants.py [--backends bwrap,landlock,none]
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import platform
import re
import subprocess
import sys
from collections import defaultdict
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LINE = re.compile(r"^test (\S*?)invariant_([a-z0-9]+)_(\S+) \.\.\. (ok|FAILED|ignored)")


def default_backends() -> list[str]:
    if sys.platform.startswith("linux"):
        return ["bwrap", "landlock", "none"]
    return ["native"]


def run(backend: str) -> list[tuple[str, str, str]]:
    env = dict(os.environ)
    if backend in ("bwrap", "landlock", "none"):
        env["BWN_TEST_SANDBOX_BACKEND"] = backend
    out = subprocess.run(
        ["cargo", "test", "--locked", "-p", "buildwithnexus", "--", "invariant_", "--test-threads=1"],
        cwd=ROOT / "harness",
        env=env,
        capture_output=True,
        text=True,
    )
    rows = []
    for line in (out.stdout + out.stderr).splitlines():
        m = LINE.match(line.strip())
        if m:
            rows.append((m.group(2).upper(), m.group(3), m.group(4)))
    if not rows:
        sys.exit(f"{backend}: no invariant tests ran\n{out.stderr[-2000:]}")
    return rows


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--backends", help="Comma-separated: bwrap,landlock,none (Linux) or native.")
    p.add_argument("--results", type=Path, help="Output directory.")
    a = p.parse_args()
    backends = a.backends.split(",") if a.backends else default_backends()
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    out = a.results or ROOT / "bench" / "results" / stamp
    out.mkdir(parents=True, exist_ok=True)

    score: dict[str, dict[str, dict[str, int]]] = defaultdict(dict)
    tests: dict[str, list[dict]] = {}
    for b in backends:
        rows = run(b)
        tests[b] = [{"case": c, "test": t, "result": r} for c, t, r in rows]
        per: dict[str, dict[str, int]] = defaultdict(lambda: {"passed": 0, "total": 0})
        for c, _, r in rows:
            per[c]["total"] += 1
            per[c]["passed"] += r == "ok"
        for c, v in per.items():
            score[c][b] = v

    report = {
        "when": stamp,
        "machine": f"{platform.system()} {platform.release()} {platform.machine()}",
        "backends": backends,
        "score": score,
        "tests": tests,
    }
    (out / "invariants.json").write_text(json.dumps(report, indent=2) + "\n")
    lines = [
        f"# Invariant suite score, {stamp}",
        "",
        f"Machine: {report['machine']}. Each cell is passed/total tests for that",
        "acceptance case on that backend. A test that does not apply to a",
        "backend (for example a Landlock-only check on bubblewrap) passes",
        "without asserting; the case IDs are defined in docs/roadmap.md and",
        "section 5 of docs/competitor-analysis.md.",
        "",
        "| Case | " + " | ".join(backends) + " |",
        "|---|" + "---|" * len(backends),
    ]
    for c in sorted(score):
        cells = [f"{score[c][b]['passed']}/{score[c][b]['total']}" if b in score[c] else "n/a" for b in backends]
        lines.append(f"| {c} | " + " | ".join(cells) + " |")
    (out / "invariants.md").write_text("\n".join(lines) + "\n")
    print("\n".join(lines))
    print(f"\nwrote {out}")


if __name__ == "__main__":
    main()
