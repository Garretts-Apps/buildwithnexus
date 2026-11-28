#!/usr/bin/env python3
"""SusVibes through bwn, with security gates off and on (roadmap 6.2).

For each task the runner starts the task's own image (the project at
/project with its dependencies installed), puts a static bwn binary and,
for the gated arm, gitleaks inside, replaces the project's git history with
a single baseline commit (so the model can't look up the historical fix),
and runs bwn on SusVibes's own prompt (evaluation_harness/common.py) against
one live model through `bench/live_proxy.py`. The patch is the diff from the
baseline. Predictions are written in SusVibes's format and scored by the
SusVibes evaluator (`--evaluate`), which runs the task's functional and
security tests in the same image.

bwn runs with `--permission-mode auto --sandbox off`. The task's container is
the isolation, as it is for the agents SusVibes's own harness runs. Inside a
container without bubblewrap, bwn's sandbox would fall back to Landlock,
and auto mode would then ask before every command, which a headless run
can't answer.

Arms:
- off: no security gates.
- on: `"security_gates": ["secrets", "dependencies"]` (gitleaks on every
  change; osv-scanner when a manifest changes, which blocks if absent).
  semgrep needs Python 3.8 or later and isn't installable in images on an
  older Python, so `sast` is left out of this arm; the record says so.

Calibration: an environment can fail tests whatever the patch (a kernel
without IPv6, say). The reference fix (`golden_patch`) is scored in the same
environment first; a patch then passes a run when it fails no test the
reference fix passes. Both the strict verdict and the calibrated one are
recorded.

    python3 bench/susvibes/run.py --sv-root <susvibes checkout> \\
        --bwn-bin target/x86_64-unknown-linux-musl/release/buildwithnexus \\
        --gitleaks <path> --model baidu/ernie-5.5-flash-preview:free \\
        --instances pylons__waitress_575994cd42e83fd772a5f7ec98b2c56751bd3f65 \\
        [--evaluate --sv-python <venv python>]
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import re
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT.parent))

from bench.live_proxy import start  # noqa: E402

ARMS = {
    "off": {},
    "on": {"security_gates": ["secrets", "dependencies"]},
}
TIMEOUT_S = 2700
CONTEXT_TOKENS = 128_000


def sh(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    kw.setdefault("capture_output", True)
    kw.setdefault("text", True)
    kw.setdefault("stdin", subprocess.DEVNULL)
    return subprocess.run(cmd, **kw)


def prompt_for(sv_root: Path, problem: str) -> str:
    """SusVibes's canonical prompt, read from its own source (not copied)."""
    src = (sv_root / "evaluation_harness" / "common.py").read_text()
    body = re.search(r'_INSTANCE_TEMPLATE_BODY = """\\\n(.*?)"""', src, re.S).group(1)
    return body.replace("__WORK_DIR__", "/project").replace("__PROBLEM_STATEMENT__", problem)


def in_container(name: str, script: str, timeout: int = 600) -> subprocess.CompletedProcess:
    return sh(["docker", "exec", name, "bash", "-lc", script], timeout=timeout)


def run_arm(rec: dict, arm: str, a, proxy) -> dict:
    name = f"bwn-sv-{arm}-{rec['instance_id'][:40]}".replace("_", "-").lower()
    sh(["docker", "rm", "-f", name])
    started = sh(["docker", "run", "-d", "--name", name, "--network", "host", "--entrypoint", "sleep",
                  rec["image_name"], "infinity"])
    if started.returncode != 0:
        return {"error": f"container did not start: {started.stderr[-300:]}"}
    try:
        sh(["docker", "cp", str(a.bwn_bin), f"{name}:/usr/local/bin/bwn"])
        if arm == "on" and a.gitleaks:
            sh(["docker", "cp", str(a.gitleaks), f"{name}:/usr/local/bin/gitleaks"])
        settings = {
            "provider": "custom",
            "base_url": proxy.url,
            "model": "bench",
            "auto_update": "off",
            # The endpoint doesn't report its window, and bwn assumes 8k for one
            # that doesn't, compacting every few steps. The pinned free model
            # has 1M; 128k keeps each request bounded.
            "context_tokens": CONTEXT_TOKENS,
            **ARMS[arm],
        }
        # A history of one commit: nothing to look up.
        setup = (
            "set -e; cd /project; rm -rf .git; git init -q; git config user.email bench@local; "
            "git config user.name bench; git add -A; git commit -qm baseline; "
            f"mkdir -p /root/.bwn; cat > /root/.bwn/settings.json <<'JSON'\n{json.dumps(settings)}\nJSON"
        )
        out = in_container(name, setup)
        if out.returncode != 0:
            return {"error": f"setup failed: {out.stderr[-300:]}"}
        (a.out / f"{arm}-prompt.txt").write_text(prompt_for(a.sv_root, rec["problem_statement"]))
        sh(["docker", "cp", str(a.out / f"{arm}-prompt.txt"), f"{name}:/root/prompt.txt"])
        before = len(proxy.requests)
        t0 = time.perf_counter()
        try:
            run = in_container(
                name,
                "cd /project && NEXUS_HOME=/root/.bwn CUSTOM_API_KEY=sk-bench BWN_ACCEPT_POSTURE=1 "
                "BWN_NO_AUTO_UPDATE=1 bwn --permission-mode auto --sandbox off --no-auto-commit --json run "
                '"$(cat /root/prompt.txt)" > /root/bwn.json 2> /root/bwn.err; echo $?',
                timeout=TIMEOUT_S,
            )
            code = int((run.stdout or "1").strip().splitlines()[-1] or 1)
            timed_out = False
        except subprocess.TimeoutExpired:
            code, timed_out = 124, True
        wall = time.perf_counter() - t0
        diff = in_container(name, "cd /project && git add -A && git diff --cached --binary baseline", timeout=120)
        log = in_container(name, "tail -c 4000 /root/bwn.err; echo; tail -c 4000 /root/bwn.json", timeout=60)
        reqs = proxy.requests[before:]
        usage = [r["usage"] for r in reqs if r["usage"]]
        return {
            "exit_code": code,
            "timed_out": timed_out,
            "wall_s": round(wall, 1),
            "model_requests": sum(1 for r in reqs if r["status"] == 200),
            "refused_requests": sum(1 for r in reqs if r["status"] != 200),
            "prompt_tokens": sum(u.get("prompt_tokens", 0) for u in usage),
            "completion_tokens": sum(u.get("completion_tokens", 0) for u in usage),
            "patch": diff.stdout,
            "patch_lines": diff.stdout.count("\n"),
            "log_tail": log.stdout[-3000:],
            "gates": ARMS[arm].get("security_gates", []),
        }
    finally:
        sh(["docker", "rm", "-f", name])


def evaluate(a, run_id: str, preds: list[dict]) -> dict:
    """Score with the SusVibes evaluator; returns {model_name: {instance: report}}."""
    # The evaluator keeps one prediction per task, so each model is its own run.
    for model in sorted({p["model_name_or_path"] for p in preds}):
        path = a.out / f"{run_id}-{model}-predictions.json"
        path.write_text(json.dumps([p for p in preds if p["model_name_or_path"] == model], indent=1))
        sh([str(a.sv_python), str(Path(__file__).with_name("sv_local.py")), "--run_id", run_id,
            "--predictions_path", str(path), "--max_workers", "1", "--force"], cwd=a.sv_root, timeout=3600)
    out = {}
    for p in preds:
        d = a.sv_root / "logs" / "eval" / run_id / "none" / p["model_name_or_path"] / p["instance_id"]
        rep = json.loads((d / "report.json").read_text()) if (d / "report.json").exists() else None
        fails = {}
        for kind in ("func", "sec"):
            f = d / "test_outputs" / f"{kind}.txt"
            text = f.read_text(errors="replace") if f.exists() else ""
            fails[kind] = sorted(set(re.findall(r"^(?:ERROR|FAIL): (\S+ \([^)]*\))", text, re.M)))
        out.setdefault(p["model_name_or_path"], {})[p["instance_id"]] = {"report": rep, "failing_tests": fails}
    return out


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--sv-root", type=Path, required=True)
    p.add_argument("--bwn-bin", type=Path, required=True)
    p.add_argument("--gitleaks", type=Path)
    p.add_argument("--model", required=True)
    p.add_argument("--upstream", default="https://openrouter.ai/api/v1")
    p.add_argument("--instances", required=True)
    p.add_argument("--arms", default="off,on")
    p.add_argument("--evaluate", action="store_true")
    p.add_argument("--sv-python", type=Path)
    p.add_argument("--results", type=Path)
    a = p.parse_args(argv)
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    a.out = a.results or ROOT / "results" / f"{stamp}-susvibes"
    a.out.mkdir(parents=True, exist_ok=True)
    data = {json.loads(l)["instance_id"]: json.loads(l)
            for l in (a.sv_root / "datasets" / "default" / "susvibes_dataset.jsonl").open()}
    proxy = start(a.upstream, a.model)
    rows = {}
    for iid in [x.strip() for x in a.instances.split(",") if x.strip()]:
        rec = data[iid]
        rows[iid] = {"cwe_ids": rec["cwe_ids"], "language": rec["language"], "arms": {}}
        for arm in [x.strip() for x in a.arms.split(",") if x.strip()]:
            print(f"susvibes {iid} {arm}", flush=True)
            rows[iid]["arms"][arm] = r = run_arm(rec, arm, a, proxy)
            (a.out / f"{iid}.{arm}.patch").write_text(r.get("patch", ""))
    record = {"time_utc": stamp, "model": a.model, "upstream": a.upstream, "bwn_bin": str(a.bwn_bin),
              "arms": ARMS, "instances": rows}
    if a.evaluate:
        preds = [{"instance_id": iid, "model_name_or_path": "golden", "model_patch": data[iid]["golden_patch"]}
                 for iid in rows]
        preds += [{"instance_id": iid, "model_name_or_path": f"bwn-{arm}", "model_patch": r.get("patch", "")}
                  for iid, row in rows.items() for arm, r in row["arms"].items() if r.get("model_requests")]
        scored = evaluate(a, f"bwn-{stamp}", preds)
        record["scores"] = scored
        for iid, row in rows.items():
            gold = scored.get("golden", {}).get(iid, {}).get("failing_tests", {})
            for arm, r in row["arms"].items():
                if not r.get("model_requests"):
                    # Every request refused (a rate limit): bwn never worked on the task.
                    r["result"] = "not run: the upstream answered no request" if r.get("refused_requests") else "not run"
                    continue
                s = scored.get(f"bwn-{arm}", {}).get(iid, {})
                rep = (s.get("report") or {}).get("run", {})
                calib = {k: not (set(s.get("failing_tests", {}).get(k, [])) - set(gold.get(k, [])))
                         for k in ("func", "sec")}
                r["strict"] = {k: (rep.get(k) or {}).get("pass") for k in ("func", "sec")}
                tested = (s.get("report") or {}).get("eval_status") == "tested"
                r["eval_status"] = (s.get("report") or {}).get("eval_status")
                r["calibrated"] = calib if tested else None
            row["golden_failing_here"] = gold
    for row in rows.values():
        for r in row["arms"].values():
            r.pop("patch", None)
    (a.out / "susvibes.json").write_text(json.dumps(record, indent=2, default=str) + "\n")
    print(a.out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
