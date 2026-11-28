#!/usr/bin/env python3
"""Pick a 20-task sequence for the longitudinal benchmark (roadmap 1.13, 6.1).

The benchmark (docs/competitor-analysis.md, section 9.2) rebuilds tasks
from a repository's own history: consecutive merged changes, each turned
into a requirement whose hidden tests are the tests the change added or
modified. This script finds windows of a repository's first-parent history
that suit that, and scores them on what the design asks for:

- every change in the window touches test files (it has hidden tests);
- at least half the changes touch a file an earlier change in the window
  added or modified (the tasks depend on each other);
- a mix of sizes, with no change so large it is a release merge.

It prints the best windows as JSON. With `--manifest`, it prints a draft
manifest for `runner.py` from the best eligible window instead: each task
with its hidden test files and its historical commit, every fifth task held
out, and the commit's subject as a requirement to rewrite before a run.

    python3 bench/longitudinal/select_tasks.py /path/to/repo [--length 20] [--top 3]
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
from pathlib import Path

TEST = re.compile(r"(^|/)(tests?|spec|__tests__)(/|$)|_test\.|\.test\.|_spec\.|\.spec\.|(^|/)test_[^/]*$")


def changes(repo: Path, limit: int) -> list[dict]:
    """First-parent commits on HEAD, oldest first, with the files each changed."""
    out = subprocess.run(
        ["git", "-C", str(repo), "log", "--first-parent", "--no-merges", f"-n{limit}",
         "--format=%x00%H%x01%s", "--numstat"],
        capture_output=True, text=True, check=True,
    ).stdout
    result = []
    for block in out.split("\x00")[1:]:
        head, _, rest = block.partition("\n")
        sha, _, subject = head.partition("\x01")
        files, lines = [], 0
        for row in rest.strip().splitlines():
            parts = row.split("\t")
            if len(parts) == 3:
                added, removed, path = parts
                files.append(path)
                if added.isdigit() and removed.isdigit():
                    lines += int(added) + int(removed)
        result.append({"sha": sha, "subject": subject, "files": files, "lines": lines})
    return list(reversed(result))


def score(window: list[dict]) -> dict:
    seen: set[str] = set()
    dependent = with_tests = 0
    for c in window:
        files = set(c["files"])
        if files & seen:
            dependent += 1
        if any(TEST.search(f) for f in files):
            with_tests += 1
        seen |= files
    sizes = [c["lines"] for c in window]
    too_big = sum(s > 2000 for s in sizes)
    ok = with_tests == len(window) and dependent >= len(window) // 2 and too_big == 0
    return {
        "eligible": ok,
        "with_tests": with_tests,
        "dependent": dependent,
        "too_big": too_big,
        "median_lines": sorted(sizes)[len(sizes) // 2],
    }


def draft_manifest(repo: Path, cs: list[dict], best: dict | None) -> dict:
    """A runner manifest for `best`: its tasks with their hidden tests."""
    if best is None:
        return {"error": "no window of that length"}
    start = next(i for i, c in enumerate(cs) if c["sha"] == best["start"])
    window = cs[start : start + len(best["tasks"])]
    base = subprocess.run(
        ["git", "-C", str(repo), "rev-parse", f"{window[0]['sha']}^"],
        capture_output=True, text=True, check=True,
    ).stdout.strip()
    tasks = []
    for n, c in enumerate(window, 1):
        tasks.append({
            "id": f"t{n:02d}",
            "requirement": f"DRAFT, rewrite as a requirement with acceptance criteria: {c['subject']}",
            "tests": [f for f in c["files"] if TEST.search(f)],
            "from_commit": c["sha"],
            "held_out": n % 5 == 0,
        })
    return {
        "repo": str(repo), "base": base, "eligible": best["eligible"],
        "test_command": "SET ME, e.g. pytest -q {tests}", "suite_command": "",
        "tasks": tasks, "harnesses": [], "model": {}, "runs": 3, "task_timeout_secs": 1800,
    }


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("repo", type=Path)
    p.add_argument("--length", type=int, default=20)
    p.add_argument("--history", type=int, default=2000, help="How many recent commits to scan.")
    p.add_argument("--top", type=int, default=3)
    p.add_argument("--manifest", action="store_true", help="Print a draft runner manifest for the best window.")
    a = p.parse_args()
    cs = changes(a.repo, a.history)
    windows = []
    for i in range(0, max(0, len(cs) - a.length + 1)):
        w = cs[i : i + a.length]
        s = score(w)
        windows.append({"start": w[0]["sha"], "end": w[-1]["sha"], **s,
                        "tasks": [{"sha": c["sha"][:12], "subject": c["subject"]} for c in w]})
    windows.sort(key=lambda w: (w["eligible"], w["with_tests"], w["dependent"]), reverse=True)
    if a.manifest:
        print(json.dumps(draft_manifest(a.repo, cs, windows[0] if windows else None), indent=2))
        return
    print(json.dumps({"repo": str(a.repo), "scanned": len(cs), "windows": windows[: a.top]}, indent=2))


if __name__ == "__main__":
    main()
