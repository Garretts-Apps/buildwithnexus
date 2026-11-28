#!/usr/bin/env python3
"""Run the longitudinal benchmark (roadmap 6.1, analysis 9.2).

A manifest pins the repository, the base commit, the task sequence, the
harnesses and the model. For each harness and each run, the runner:

1. clones the repository at the base commit into a fresh work tree;
2. for task n, writes the task's requirement and runs the harness on the
   harness's own tree after task n-1 (never the historical tree);
3. puts the hidden tests of tasks 1..n into the tree (the tests each
   historical change added or modified), runs the test command, and scores
   this task's tests, every earlier task's tests (regressions) and the
   repository's own suite;
4. records the harness's own report (tokens, tool calls, denials, exit
   code) when it prints bwn's `--json` events, the wall time, and a git
   bundle of the tree after the task.

Raw per-task records, the bundles and the manifest (with its SHA-256) go
under `bench/results/<timestamp>/longitudinal/`, and `summary.md` is
generated from them: per harness, the median and range over runs of tasks
passed, regressions and tokens, and the share of runs that reach the last
task with zero regressions. Held-out tasks run like the others and are
reported separately, so a harness tuned to the visible set shows it.

    python3 bench/longitudinal/runner.py manifest.json [--out bench/results]

Manifest (JSON):

    {
      "repo": "/path/or/url", "base": "<sha>",
      "test_command": "pytest -q {tests}",       # {tests}: hidden test files
      "suite_command": "pytest -q",               # the repository's own suite
      "tasks": [{"id": "t1", "requirement": "...", "tests": ["tests/test_x.py"],
                 "from_commit": "<sha>", "held_out": false}, ...],
      "harnesses": [{"name": "bwn", "command": "bwn --json run --permission-mode auto -- {requirement}"}],
      "model": {"base_url": "...", "model": "..."},
      "runs": 3, "task_timeout_secs": 1800
    }

Each task's hidden tests are taken from `from_commit` (the historical
change), so the harness never sees them while it works.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import shlex
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path


def sh(cmd: list[str] | str, cwd: Path, timeout: int = 600, env: dict | None = None) -> subprocess.CompletedProcess:
    return subprocess.run(
        cmd,
        cwd=cwd,
        shell=isinstance(cmd, str),
        capture_output=True,
        text=True,
        # A harness must not wait on a pipe it was never given (bwn waits 3 s for one).
        stdin=subprocess.DEVNULL,
        timeout=timeout,
        env=env,
    )


def git(cwd: Path, *args: str) -> str:
    out = sh(["git", *args], cwd)
    if out.returncode != 0:
        raise RuntimeError(f"git {' '.join(args)}: {out.stderr.strip()}")
    return out.stdout


def hidden_test_files(repo: Path, task: dict) -> dict[str, str]:
    """The task's hidden tests as they were in its historical change."""
    files = {}
    for path in task.get("tests", []):
        files[path] = git(repo, "show", f"{task['from_commit']}:{path}")
    return files


def bwn_report(stdout: str) -> dict:
    """Tokens, tool calls and denials from bwn's `--json` events, if any."""
    rep = {"tokens_in": None, "tokens_out": None, "tool_calls": 0, "denials": None, "outcome": None}
    for line in stdout.splitlines():
        try:
            e = json.loads(line)
        except json.JSONDecodeError:
            continue
        if e.get("type") == "tool_call":
            rep["tool_calls"] += 1
        elif e.get("type") == "result":
            rep["tokens_in"] = e.get("tokens_in")
            rep["tokens_out"] = e.get("tokens_out")
            rep["denials"] = e.get("denied")
            rep["outcome"] = e.get("outcome")
    return rep


# Tests run without bytecode caches: a harness that rewrites a module within
# the same second as an earlier run would otherwise be tested against the
# stale cache.
TEST_ENV = {**__import__("os").environ, "PYTHONDONTWRITEBYTECODE": "1"}


def change_size(tree: Path, exclude: set[str]) -> dict:
    """Lines the task's commit added and removed, outside its hidden tests."""
    added = removed = files = 0
    for row in sh(["git", "diff", "--numstat", "HEAD~1", "HEAD"], tree).stdout.splitlines():
        a, r, path = (row.split("\t") + ["", "", ""])[:3]
        if path in exclude or not a.isdigit():
            continue
        added, removed, files = added + int(a), removed + int(r), files + 1
    return {"lines_added": added, "lines_removed": removed, "files_changed": files}


def rework(tree: Path, recent: list[str]) -> int:
    """Lines this task removed or rewrote that one of the `recent` task
    commits (the previous three) wrote."""
    if not recent:
        return 0
    count = 0
    diff = sh(["git", "diff", "-U0", "HEAD~1", "HEAD"], tree).stdout
    path = None
    for line in diff.splitlines():
        if line.startswith("--- a/"):
            path = line[6:]
        elif line.startswith("--- "):
            path = None
        elif line.startswith("@@") and path:
            old = line.split()[1].lstrip("-")
            start, _, n = old.partition(",")
            n = int(n) if n else 1
            if n == 0:
                continue
            blame = sh(["git", "blame", "--porcelain", "-L", f"{start},{int(start) + n - 1}", "HEAD~1", "--", path], tree).stdout
            count += sum(1 for b in blame.splitlines() if b.split(" ")[0] in recent)
    return count


# Scanners at the benchmark's own versions, never the harness's: each runs
# when installed, and the record says when one isn't.
SCANNERS = {
    "gitleaks": ["gitleaks", "detect", "--no-banner", "--no-git", "--report-format", "json", "--report-path", "/dev/stdout", "--source", "."],
    "semgrep": ["semgrep", "scan", "--config", "auto", "--json", "--quiet", "."],
    "osv-scanner": ["osv-scanner", "--format", "json", "-r", "."],
}


def scan(tree: Path) -> dict:
    out = {}
    for name, cmd in SCANNERS.items():
        if not shutil.which(cmd[0]):
            out[name] = "not installed"
            continue
        try:
            text = sh(cmd, tree, timeout=900).stdout
            data = json.loads(text or "null")
        except (subprocess.TimeoutExpired, json.JSONDecodeError):
            out[name] = "no report"
            continue
        if isinstance(data, list):
            out[name] = len(data)
        elif isinstance(data, dict) and "results" in data:
            res = data["results"]
            out[name] = sum(len(r.get("packages", [])) for r in res) if name == "osv-scanner" else len(res)
        else:
            out[name] = 0
    return out


def python_public_names(text: str) -> set[str] | None:
    import ast
    try:
        tree = ast.parse(text)
    except SyntaxError:
        return None
    names = set()
    for node in tree.body:
        if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)) and not node.name.startswith("_"):
            names.add(node.name)
    return names


def api_diff(tree: Path, exclude: set[str]) -> dict:
    """Public names the task added and removed: top-level functions and
    classes of changed Python modules. Other languages aren't measured."""
    added, removed, other = [], [], 0
    for path in sh(["git", "diff", "--name-only", "HEAD~1", "HEAD"], tree).stdout.split():
        if path in exclude:
            continue
        if not path.endswith(".py"):
            other += 1
            continue
        before = python_public_names(sh(["git", "show", f"HEAD~1:{path}"], tree).stdout) or set()
        after = python_public_names(sh(["git", "show", f"HEAD:{path}"], tree).stdout) or set()
        added += [f"{path}:{n}" for n in sorted(after - before)]
        removed += [f"{path}:{n}" for n in sorted(before - after)]
    return {"api_added": added, "api_removed": removed, "api_unmeasured_files": other}


def complexity(tree: Path, exclude: set[str]) -> dict | str:
    """lizard's cyclomatic complexity of the changed files' functions: the
    highest and the mean, or why there is none."""
    if not shutil.which("lizard"):
        return "not installed"
    files = [f for f in sh(["git", "diff", "--name-only", "HEAD~1", "HEAD"], tree).stdout.split()
             if f not in exclude and (tree / f).is_file()]
    if not files:
        return {"functions": 0}
    out = sh(["lizard", "--csv", *files], tree).stdout
    ccn = []
    for row in out.splitlines():
        parts = row.split(",")
        if len(parts) > 1 and parts[1].isdigit():
            ccn.append(int(parts[1]))
    if not ccn:
        return {"functions": 0}
    return {"functions": len(ccn), "max_ccn": max(ccn), "mean_ccn": round(sum(ccn) / len(ccn), 2)}


def run_tests(tree: Path, command: str, files: list[str], timeout: int) -> bool | None:
    if not files:
        return None
    cmd = command.replace("{tests}", " ".join(shlex.quote(f) for f in files))
    return sh(cmd, tree, timeout=timeout, env=TEST_ENV).returncode == 0


def run_sequence(manifest: dict, harness: dict, run_no: int, out: Path) -> list[dict]:
    """One harness, one run of the whole sequence. Returns the task records."""
    repo = Path(manifest["repo"]) if Path(manifest["repo"]).exists() else None
    work = Path(tempfile.mkdtemp(prefix=f"bwn-long-{harness['name']}-{run_no}-"))
    tree = work / "tree"
    git(work, "clone", "-q", str(repo or manifest["repo"]), str(tree))
    git(tree, "checkout", "-q", manifest["base"])
    git(tree, "config", "user.name", "bench")
    git(tree, "config", "user.email", "bench@localhost")
    source = repo or tree
    timeout = int(manifest.get("task_timeout_secs", 1800))
    records = []
    earlier: list[str] = []
    for n, task in enumerate(manifest["tasks"], 1):
        hidden = hidden_test_files(source, task)
        # The harness works without this task's tests in the tree.
        for f in hidden:
            (tree / f).unlink(missing_ok=True)
        req = task["requirement"]
        cmd = harness["command"].replace("{requirement}", shlex.quote(req))
        started = time.monotonic()
        try:
            ran = sh(cmd, tree, timeout=timeout)
            exit_code, stdout = ran.returncode, ran.stdout
        except subprocess.TimeoutExpired:
            exit_code, stdout = None, ""
        wall = round(time.monotonic() - started, 3)
        # Then the hidden tests of tasks 1..n go in, and run.
        for f, text in hidden.items():
            (tree / f).parent.mkdir(parents=True, exist_ok=True)
            (tree / f).write_text(text)
        this_task = run_tests(tree, manifest["test_command"], list(hidden), timeout)
        # A regression is an earlier task's test that passed after its own
        # task and fails now; one that never passed isn't.
        regressions = [
            f for f in earlier if run_tests(tree, manifest["test_command"], [f], timeout) is False
        ]
        suite = (
            sh(manifest["suite_command"], tree, timeout=timeout, env=TEST_ENV).returncode == 0
            if manifest.get("suite_command")
            else None
        )
        if this_task:
            earlier.extend(hidden)
        sh(["git", "add", "-A"], tree)
        sh(["git", "commit", "-q", "-m", f"after {task['id']}", "--allow-empty"], tree)
        size = change_size(tree, set(hidden))
        commits = sh(["git", "log", "--format=%H", "-n", "4"], tree).stdout.split()
        reworked = rework(tree, commits[1:4] if n > 1 else [])
        findings = scan(tree)
        api = api_diff(tree, set(hidden))
        ccn = complexity(tree, set(hidden))
        bundle = out / f"{task['id']}.bundle"
        sh(["git", "bundle", "create", str(bundle), "HEAD"], tree)
        rec = {
            "task": task["id"],
            "n": n,
            "held_out": bool(task.get("held_out")),
            "harness": harness["name"],
            "run": run_no,
            "exit_code": exit_code,
            "wall_secs": wall,
            "tests_passed": this_task,
            "regressions": regressions,
            "suite_passed": suite,
            **size,
            "rework_lines": reworked,
            "scanner_findings": findings,
            "complexity": ccn,
            **api,
            **bwn_report(stdout),
        }
        (out / f"{task['id']}.json").write_text(json.dumps(rec, indent=2))
        records.append(rec)
    return records


def summarize(manifest: dict, all_records: dict[str, list[list[dict]]]) -> str:
    lines = [
        "# Longitudinal benchmark",
        "",
        f"Model: `{manifest.get('model', {}).get('model', '?')}`. "
        f"{len(manifest['tasks'])} tasks, {manifest.get('runs', 3)} runs per harness. "
        "Model output is not deterministic: medians and ranges over runs.",
        "",
        "| Harness | Tasks passed (visible) | Tasks passed (held out) | Regressions | Zero-regression runs | Rework lines | Tokens in |",
        "|---|---|---|---|---|---|---|",
    ]

    def med_range(xs: list[float]) -> str:
        xs = [x for x in xs if x is not None]
        if not xs:
            return "n/a"
        return f"{statistics.median(xs):g} ({min(xs):g}–{max(xs):g})"

    for name, runs in all_records.items():
        vis = [sum(1 for r in run if r["tests_passed"] and not r["held_out"]) for run in runs]
        held = [sum(1 for r in run if r["tests_passed"] and r["held_out"]) for run in runs]
        regs = [sum(len(r["regressions"]) for r in run) for run in runs]
        clean = sum(1 for run in runs if run and not any(r["regressions"] for r in run))
        toks = [
            sum(r["tokens_in"] or 0 for r in run) if any(r["tokens_in"] is not None for r in run) else None
            for run in runs
        ]
        rework = [sum(r.get("rework_lines", 0) for r in run) for run in runs]
        lines.append(
            f"| {name} | {med_range(vis)} | {med_range(held)} | {med_range(regs)} | {clean}/{len(runs)} | {med_range(rework)} | {med_range(toks)} |"
        )
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("manifest", type=Path)
    p.add_argument("--out", type=Path, default=Path("bench/results"))
    a = p.parse_args(argv)
    text = a.manifest.read_text()
    manifest = json.loads(text)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    root = a.out / stamp / "longitudinal"
    root.mkdir(parents=True, exist_ok=True)
    (root / "manifest.json").write_text(text)
    (root / "manifest.sha256").write_text(hashlib.sha256(text.encode()).hexdigest() + "\n")
    all_records: dict[str, list[list[dict]]] = {}
    for h in manifest["harnesses"]:
        for r in range(1, int(manifest.get("runs", 3)) + 1):
            out = root / h["name"] / f"run{r}"
            out.mkdir(parents=True, exist_ok=True)
            all_records.setdefault(h["name"], []).append(run_sequence(manifest, h, r, out))
    (root / "summary.md").write_text(summarize(manifest, all_records))
    print(root)
    return 0


if __name__ == "__main__":
    sys.exit(main())
