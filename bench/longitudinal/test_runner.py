"""Tests for the longitudinal runner (roadmap 6.1), with scripted harnesses
instead of a model: an oracle that applies each historical change, one that
does nothing, and one that breaks an earlier task. They check the scoring
the benchmark rests on: hidden tests the harness can't see, tests that
accumulate, regressions, held-out tasks reported apart, and the records,
bundles and manifest hash a run leaves.

    python3 -m unittest bench.longitudinal.test_runner
"""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import runner  # noqa: E402


def git(cwd: Path, *args: str) -> str:
    return subprocess.run(["git", *args], cwd=cwd, check=True, capture_output=True, text=True).stdout.strip()


class RunnerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="bwn-long-test-"))
        repo = self.tmp / "repo"
        repo.mkdir()
        git(repo, "init", "-q", "-b", "main")
        git(repo, "config", "user.name", "t")
        git(repo, "config", "user.email", "t@t")
        (repo / "calc.py").write_text("def add(a, b):\n    return a + b\n")
        git(repo, "add", ".")
        git(repo, "commit", "-q", "-m", "base")
        self.base = git(repo, "rev-parse", "HEAD")
        # Task 1 adds mul, with its test.
        (repo / "calc.py").write_text("def add(a, b):\n    return a + b\n\ndef mul(a, b):\n    return a * b\n")
        (repo / "tests").mkdir()
        (repo / "tests/test_mul.py").write_text("import sys; sys.path.insert(0, '.')\nfrom calc import mul\nassert mul(2, 3) == 6\n")
        git(repo, "add", ".")
        git(repo, "commit", "-q", "-m", "add mul")
        self.c1 = git(repo, "rev-parse", "HEAD")
        # Task 2 adds square on top of mul (depends on task 1), held out.
        (repo / "calc.py").write_text((repo / "calc.py").read_text() + "\ndef square(a):\n    return mul(a, a)\n")
        (repo / "tests/test_square.py").write_text("import sys; sys.path.insert(0, '.')\nfrom calc import square\nassert square(4) == 16\n")
        git(repo, "add", ".")
        git(repo, "commit", "-q", "-m", "add square")
        self.c2 = git(repo, "rev-parse", "HEAD")
        self.repo = repo
        harness_dir = self.tmp / "h"
        harness_dir.mkdir()
        # The oracle: the requirement names the historical change; it applies
        # that change's calc.py (never its tests).
        (harness_dir / "oracle.py").write_text(
            "import subprocess, sys\n"
            f"src = {str(repo)!r}\n"
            "sha = sys.argv[1].split()[-1]\n"
            "text = subprocess.run(['git', '-C', src, 'show', sha + ':calc.py'], capture_output=True, text=True, check=True).stdout\n"
            "open('calc.py', 'w').write(text)\n"
            "print('{\"type\": \"tool_call\"}')\n"
            "print('{\"type\": \"result\", \"tokens_in\": 10, \"tokens_out\": 2, \"denied\": 0, \"outcome\": \"success\"}')\n"
        )
        # Breaks task 1 while doing task 2.
        (harness_dir / "breaker.py").write_text(
            "import sys\n"
            "if 'square' in sys.argv[1]:\n"
            "    open('calc.py', 'w').write('def add(a, b):\\n    return a + b\\n\\ndef square(a):\\n    return a * a\\n')\n"
            "else:\n"
            "    open('calc.py', 'a').write('\\ndef mul(a, b):\\n    return a * b\\n')\n"
        )
        self.manifest = {
            "repo": str(repo),
            "base": self.base,
            "test_command": "for t in {tests}; do python3 $t || exit 1; done",
            "tasks": [
                {"id": "t1", "requirement": f"add mul {self.c1}", "tests": ["tests/test_mul.py"], "from_commit": self.c1},
                {"id": "t2", "requirement": f"add square {self.c2}", "tests": ["tests/test_square.py"], "from_commit": self.c2, "held_out": True},
            ],
            "harnesses": [
                {"name": "oracle", "command": f"python3 {harness_dir / 'oracle.py'} {{requirement}}"},
                {"name": "idle", "command": "true"},
                {"name": "breaker", "command": f"python3 {harness_dir / 'breaker.py'} {{requirement}}"},
            ],
            "model": {"model": "mock"},
            "runs": 2,
            "task_timeout_secs": 60,
        }

    def run_manifest(self) -> Path:
        m = self.tmp / "manifest.json"
        m.write_text(json.dumps(self.manifest))
        out = self.tmp / "results"
        self.assertEqual(runner.main([str(m), "--out", str(out)]), 0)
        (stamp,) = list(out.iterdir())
        return stamp / "longitudinal"

    def records(self, root: Path, harness: str, run: int) -> list[dict]:
        return [json.loads((root / harness / f"run{run}" / f"{t}.json").read_text()) for t in ("t1", "t2")]

    def test_scores_hidden_tests_regressions_and_held_out_tasks(self) -> None:
        root = self.run_manifest()
        oracle = self.records(root, "oracle", 1)
        self.assertEqual([r["tests_passed"] for r in oracle], [True, True])
        self.assertEqual([r["regressions"] for r in oracle], [[], []])
        self.assertEqual(oracle[0]["tokens_in"], 10)
        self.assertEqual(oracle[0]["tool_calls"], 1)
        self.assertTrue(oracle[1]["held_out"])
        idle = self.records(root, "idle", 1)
        self.assertEqual([r["tests_passed"] for r in idle], [False, False])
        breaker = self.records(root, "breaker", 2)
        self.assertEqual(breaker[0]["tests_passed"], True)
        self.assertEqual(breaker[1]["tests_passed"], True)
        self.assertEqual(breaker[1]["regressions"], ["tests/test_mul.py"])
        # Size of each change, rework of the previous tasks' lines, and the
        # scanners (reported as not installed where they aren't).
        self.assertGreater(oracle[0]["lines_added"], 0)
        self.assertEqual(oracle[0]["rework_lines"], 0)
        self.assertGreater(breaker[1]["rework_lines"], 0, breaker[1])
        self.assertIn("gitleaks", oracle[0]["scanner_findings"])
        # The public API each change added or removed.
        self.assertEqual(oracle[0]["api_added"], ["calc.py:mul"])
        self.assertEqual(breaker[1]["api_removed"], ["calc.py:mul"])
        self.assertIn("complexity", oracle[0])
        # Each task's tree is kept, and the manifest with its hash.
        self.assertTrue((root / "oracle" / "run1" / "t2.bundle").is_file())
        self.assertEqual(json.loads((root / "manifest.json").read_text())["base"], self.base)
        self.assertEqual(len((root / "manifest.sha256").read_text().strip()), 64)
        summary = (root / "summary.md").read_text()
        self.assertIn("| oracle | 1 (1–1) | 1 (1–1) | 0 (0–0) | 2/2 | 0 (0–0) |", summary)
        self.assertIn("| breaker | 1 (1–1) | 1 (1–1) | 1 (1–1) | 0/2 |", summary)
        self.assertIn("| idle | 0 (0–0) | 0 (0–0) | 0 (0–0) | 2/2 |", summary)

    def test_the_harness_never_sees_the_hidden_tests(self) -> None:
        peek = self.tmp / "h" / "peek.py"
        peek.write_text("import os, sys\nopen('saw.txt', 'w').write(str(os.path.exists('tests/test_mul.py')))\n")
        self.manifest["harnesses"] = [{"name": "peek", "command": f"python3 {peek} {{requirement}}"}]
        self.manifest["tasks"] = self.manifest["tasks"][:1]
        self.manifest["runs"] = 1
        root = self.run_manifest()
        bundle = root / "peek" / "run1" / "t1.bundle"
        check = self.tmp / "check"
        git(self.tmp, "clone", "-q", str(bundle), str(check))
        self.assertEqual((check / "saw.txt").read_text(), "False")


if __name__ == "__main__":
    unittest.main()
