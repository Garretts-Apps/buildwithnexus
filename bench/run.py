#!/usr/bin/env python3
"""Run the competitor speed suite. See bench/README.md."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT.parent))

from bench.suite import execute  # noqa: E402


def main() -> None:
    parser = argparse.ArgumentParser(description="Measure bwn against other terminal coding agents.")
    parser.add_argument(
        "--results",
        type=Path,
        help="Directory for this run's JSON. Default: bench/results/<UTC timestamp>/",
    )
    parser.add_argument(
        "--only",
        help="Comma-separated subset: startup,memory,prompt,task. Default runs every metric.",
    )
    args = parser.parse_args()
    if args.results:
        results = args.results
    else:
        from bench.suite import collect_hardware

        hw = collect_hardware()
        results = ROOT / "results" / hw["timestamp_utc"].replace(":", "-")
    only = None
    if args.only:
        only = {part.strip() for part in args.only.split(",") if part.strip()}
    summary = execute(results, only)
    print(f"wrote {results}")
    print(f"complete={summary.get('complete')}")


if __name__ == "__main__":
    main()
