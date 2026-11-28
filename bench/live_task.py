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


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--model", required=True)
    p.add_argument("--upstream", default="https://openrouter.ai/api/v1")
    p.add_argument("--only", default="")
    p.add_argument("--pause", type=float, default=10.0, help="Seconds between harnesses (rate limits).")
    p.add_argument("--results", type=Path)
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
    for cmd in suite.PINS["commands"]:
        if only and cmd["id"] not in only:
            continue
        spec = suite.task_command(cmd, installs, proxy.url, work)
        if spec.get("status") == "skipped" or spec.get("skip"):
            rows[cmd["id"]] = {"display": cmd["display"], "status": "skipped", "reason": spec.get("reason")}
            continue
        run_dir = Path(spec["cwd"])
        suite.git_fixture(run_dir)
        out_file = Path(spec["out_file"]) if spec.get("out_file") else None
        if out_file and out_file.exists():
            out_file.unlink()
        before = len(proxy.requests)
        t0 = time.perf_counter()
        try:
            proc = subprocess.run(spec["argv"], env=spec["env"], cwd=run_dir, capture_output=True, text=True, stdin=subprocess.DEVNULL, timeout=600)
            timed_out = False
        except subprocess.TimeoutExpired as e:
            proc = subprocess.CompletedProcess(spec["argv"], 124, e.stdout or "", e.stderr or "")
            timed_out = True
        wall = time.perf_counter() - t0
        reqs = [r for r in proxy.requests[before:] if r["path"] != "/v1/models"]
        text = (proc.stdout or "") + (out_file.read_text() if out_file and out_file.exists() else "")
        usage = [r["usage"] for r in reqs if r["usage"]]
        rows[cmd["id"]] = {
            "display": cmd["display"],
            "status": "measured",
            "ok": proc.returncode == 0 and "BENCHMARK_OK" in text and not timed_out,
            "exit_code": proc.returncode,
            "wall_s": round(wall, 3),
            "model_requests": len(reqs),
            "request_bytes": sum(r["request_bytes"] for r in reqs),
            "prompt_tokens": sum(u.get("prompt_tokens", 0) for u in usage) if usage else None,
            "completion_tokens": sum(u.get("completion_tokens", 0) for u in usage) if usage else None,
            "requests_without_usage": sum(1 for r in reqs if not r["usage"]),
            "upstream_statuses": [r["status"] for r in reqs],
            "stdout_tail": (proc.stdout or "")[-600:],
            "stderr_tail": (proc.stderr or "")[-600:],
        }
        suite.log(f"live {cmd['id']}: {rows[cmd['id']]['ok']} {rows[cmd['id']]['prompt_tokens']} tokens")
        time.sleep(a.pause)
    (out / "live_task.json").write_text(json.dumps({
        "model": a.model, "upstream": a.upstream, "task_text": suite.TASK_TEXT,
        "time_utc": stamp, "tools": rows,
    }, indent=2) + "\n")
    lines = [
        f"# Fixed task against {a.model}",
        "",
        f"Run {stamp}, one run per harness through `bench/live_proxy.py` to {a.upstream}. "
        "Tokens are what the upstream reported for each request. Wall time includes the "
        "upstream's latency and queueing. The proxy buffers each reply, so it isn't a latency measurement.",
        "",
        "| Harness | BENCHMARK_OK | requests | request bytes | prompt tokens | completion tokens | wall s |",
        "|---|---|---:|---:|---:|---:|---:|",
    ]
    for r in rows.values():
        if r["status"] == "skipped":
            lines.append(f"| {r['display']} | skipped: {r['reason']} | | | | | |")
            continue
        lines.append(
            f"| {r['display']} | {'yes' if r['ok'] else 'no (exit ' + str(r['exit_code']) + ')'} | {r['model_requests']} | "
            f"{r['request_bytes']} | {r['prompt_tokens'] if r['prompt_tokens'] is not None else '—'} | "
            f"{r['completion_tokens'] if r['completion_tokens'] is not None else '—'} | {r['wall_s']} |"
        )
    (out / "live_task.md").write_text("\n".join(lines) + "\n")
    print(out)
    return 0


if __name__ == "__main__":
    sys.exit(main())
