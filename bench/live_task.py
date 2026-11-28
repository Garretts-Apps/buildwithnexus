#!/usr/bin/env python3
"""The efficiency bench's fixed task against one live model (roadmap 6.4).

Each pinned harness runs the bench's one-line task once, pointed at
`live_proxy.py`, which forwards to the upstream endpoint with the model
replaced by the pinned one. Recorded per harness: whether it exited 0 with
BENCHMARK_OK, wall time, the model requests it made, their bytes, and the
prompt and completion tokens the upstream reported for them. A harness that
can't be pointed at an OpenAI-compatible endpoint is skipped with the
efficiency bench's own reason.

    python3 bench/live_task.py --model baidu/ernie-5.5-flash-preview:free \
        [--upstream https://openrouter.ai/api/v1] [--only bwn-native,codex]

Free models are rate-limited (OpenRouter: about 20 requests a minute and 50
a day without credits), so this runs each harness once and waits between
them. Wall times include the upstream's own latency and queueing: they are
not the startup or overhead figures of the main bench.
"""

from __future__ import annotations

import argparse
import datetime as dt
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT.parent))

from bench import suite  # noqa: E402
from bench.live_proxy import start  # noqa: E402


def outcome(ok: bool, statuses: list[int], timed_out: bool) -> str:
    """What happened, so an upstream refusal isn't read as the harness failing."""
    if ok:
        return "ok"
    if 429 in statuses:
        return "rate-limited"
    if any(st in (404, 405) for st in statuses):
        return "endpoint not served upstream"
    if any(st >= 500 or st == 502 for st in statuses):
        return "upstream error"
    if timed_out:
        return "timed out"
    if not statuses:
        return "no model request"
    return "harness failed"


def run_one(cmd: dict, spec: dict, proxy) -> dict:
    run_dir = Path(spec["cwd"])
    suite.git_fixture(run_dir)
    out_file = Path(spec["out_file"]) if spec.get("out_file") else None
    if out_file and out_file.exists():
        out_file.unlink()
    before = len(proxy.requests)
    t0 = time.perf_counter()
    try:
        proc = subprocess.run(
            spec["argv"], env=spec["env"], cwd=run_dir, capture_output=True, text=True,
            stdin=subprocess.DEVNULL, timeout=600,
        )
        timed_out = False
    except subprocess.TimeoutExpired as e:
        proc = subprocess.CompletedProcess(spec["argv"], 124, suite.as_text(e.stdout), suite.as_text(e.stderr))
        timed_out = True
    wall = time.perf_counter() - t0
    reqs = [r for r in proxy.requests[before:] if r["path"] != "/v1/models"]
    text = (proc.stdout or "") + (out_file.read_text() if out_file and out_file.exists() else "")
    usage = [r["usage"] for r in reqs if r["usage"]]
    statuses = [r["status"] for r in reqs]
    ok = proc.returncode == 0 and "BENCHMARK_OK" in text and not timed_out
    row = {
        "display": cmd["display"],
        "status": "measured",
        "ok": ok,
        "outcome": outcome(ok, statuses, timed_out),
        "exit_code": proc.returncode,
        "wall_s": round(wall, 3),
        # Requests the upstream answered; ones it refused (429) are counted apart.
        "model_requests": sum(1 for r in reqs if r["status"] == 200),
        "refused_requests": sum(1 for r in reqs if r["status"] != 200),
        "request_bytes": sum(r["request_bytes"] for r in reqs if r["status"] == 200),
        "prompt_tokens": sum(u.get("prompt_tokens", 0) for u in usage) if usage else None,
        "completion_tokens": sum(u.get("completion_tokens", 0) for u in usage) if usage else None,
        "requests_without_usage": sum(1 for r in reqs if not r["usage"]),
        "upstream_statuses": statuses,
        "paths": sorted({r["path"] for r in reqs}),
        "stdout_tail": (proc.stdout or "")[-600:],
        "stderr_tail": (proc.stderr or "")[-600:],
    }
    suite.log(f"live {cmd['id']}: {row['outcome']} {row['prompt_tokens']} tokens {statuses}")
    return row


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--model", required=True)
    p.add_argument("--upstream", default="https://openrouter.ai/api/v1")
    p.add_argument("--only", default="")
    p.add_argument("--pause", type=float, default=10.0, help="Seconds between harnesses (rate limits).")
    p.add_argument("--retry-pause", type=float, default=90.0, help="Seconds before retrying a rate-limited harness.")
    p.add_argument("--results", type=Path)
    p.add_argument("--merge", action="store_true", help="Keep the rows already in --results and replace only the harnesses run now.")
    a = p.parse_args(argv)
    only = {x.strip() for x in a.only.split(",") if x.strip()}
    stamp = dt.datetime.now(dt.timezone.utc).strftime("%Y-%m-%dT%H-%M-%SZ")
    out = a.results or ROOT / "results" / f"{stamp}-live"
    out.mkdir(parents=True, exist_ok=True)
    layout = suite.Layout(ROOT / ".tools")
    installs = suite.install_all(layout)
    proxy = start(a.upstream, a.model)
    work = layout.tools / "live-work"
    rows = {}
    if a.merge and (out / "live_task.json").exists():
        prev = json.loads((out / "live_task.json").read_text())
        rows = prev["tools"]
        stamp = prev["time_utc"]
    cmds = [c for c in suite.PINS["commands"] if not only or c["id"] in only]
    for cmd in cmds:
        spec = suite.task_command(cmd, installs, proxy.url, work)
        if spec.get("status") == "skipped" or spec.get("skip"):
            rows[cmd["id"]] = {"display": cmd["display"], "status": "skipped", "reason": spec.get("reason")}
            continue
        rows[cmd["id"]] = run_one(cmd, spec, proxy)
        time.sleep(a.pause)
    # A rate-limited row says nothing about the harness: try it once more, later.
    for cmd in cmds:
        if rows.get(cmd["id"], {}).get("outcome") == "rate-limited":
            time.sleep(a.retry_pause)
            spec = suite.task_command(cmd, installs, proxy.url, work)
            rows[cmd["id"]] = run_one(cmd, spec, proxy)
            rows[cmd["id"]]["retried"] = True
    (out / "live_task.json").write_text(json.dumps({
        "model": a.model, "upstream": a.upstream, "task_text": suite.TASK_TEXT,
        "time_utc": stamp, "tools": rows,
    }, indent=2) + "\n")
    (out / "live_task.md").write_text(render(json.loads((out / "live_task.json").read_text())))
    print(out)
    return 0


def render(rec: dict) -> str:
    lines = [
        f"# Fixed task against {rec['model']}",
        "",
        f"Run {rec['time_utc']}, one run per harness through `bench/live_proxy.py` to {rec['upstream']}. "
        "Tokens are what the upstream reported for each request. Wall time includes the "
        "upstream's latency and queueing, and the retries a harness made after a 429. The proxy buffers "
        "each reply, so it isn't a latency measurement. Requests and bytes count the requests the upstream "
        "answered; refused ones are in their own column.",
        "",
        "| Harness | outcome | requests | refused (429 etc.) | request bytes | prompt tokens | completion tokens | wall s |",
        "|---|---|---:|---:|---:|---:|---:|---:|",
    ]
    for r in rec['tools'].values():
        if r["status"] == "skipped":
            lines.append(f"| {r['display']} | skipped: {r['reason']} | | | | | | |")
            continue
        lines.append(
            f"| {r['display']} | {r['outcome'] if r['ok'] or r['outcome'] != 'harness failed' else 'harness failed (exit ' + str(r['exit_code']) + ')'} | {r['model_requests']} | {r['refused_requests']} | "
            f"{r['request_bytes']}{' (incl. refused)' if r.get('request_bytes_include_refused') else ''} | {r['prompt_tokens'] if r['prompt_tokens'] is not None else '—'} | "
            f"{r['completion_tokens'] if r['completion_tokens'] is not None else '—'} | {r['wall_s']} |"
        )
    return "\n".join(lines) + "\n"


if __name__ == "__main__":
    sys.exit(main())
