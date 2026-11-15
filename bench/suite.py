"""Install the pinned CLIs and measure startup, memory, size, prompt, and a mock task.

Every number written under the results directory comes from a command this
process ran. Missing measurements are recorded as errors or skips, never filled in.
"""

from __future__ import annotations

import json
import os
import platform
import re
import shutil
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent
REPO = ROOT.parent
PINS = json.loads((ROOT / "pins.json").read_text())
ANSI = re.compile(r"\x1b(?:[@-Z\\-_]|\[[0-?]*[ -/]*[@-~])")
TASK_TEXT = PINS["task_text"]
TOKEN = "BENCHMARK_OK"


def log(msg: str) -> None:
    print(msg, flush=True)


def run(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    kw.setdefault("text", True)
    return subprocess.run(cmd, **kw)


def check(cmd: list[str], **kw) -> subprocess.CompletedProcess:
    kw.setdefault("check", True)
    return run(cmd, **kw)


def tool_path() -> str:
    parts: list[str] = []
    for name in ("node", "npm", "python3", "git", "curl", "sudo"):
        found = shutil.which(name)
        if found:
            parts.append(str(Path(found).parent))
    parts.extend(["/usr/local/sbin", "/usr/local/bin", "/usr/sbin", "/usr/bin", "/sbin", "/bin"])
    seen: dict[str, None] = {}
    for p in parts:
        seen.setdefault(p, None)
    return ":".join(seen)


def base_env(home: Path, extra: dict | None = None) -> dict[str, str]:
    env = {
        "PATH": tool_path(),
        "HOME": str(home),
        "USER": "bench",
        "LOGNAME": "bench",
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "TERM": "xterm-256color",
        "SHELL": "/bin/bash",
        "GIT_TERMINAL_PROMPT": "0",
        "NPM_CONFIG_UPDATE_NOTIFIER": "false",
    }
    if extra:
        env.update(extra)
    return env


def linux_target() -> str:
    machine = platform.machine()
    arch = {"x86_64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}.get(machine)
    if not arch:
        raise SystemExit(f"unsupported architecture for the release binary: {machine}")
    return f"{arch}-unknown-linux-gnu"


def read_text(path: Path) -> str:
    return path.read_text(encoding="utf-8", errors="replace")


def write_json(path: Path, obj: object) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(obj, indent=2, sort_keys=False) + "\n")


def median(values: list[float]) -> float:
    if not values:
        raise ValueError("empty")
    ordered = sorted(values)
    mid = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2


def du_bytes(path: Path) -> int:
    if not path.exists():
        return 0
    total = 0
    if path.is_file():
        return path.stat().st_size
    for dirpath, dirnames, filenames in os.walk(path):
        # Don't follow directory symlinks into the rest of the disk.
        dirnames[:] = [d for d in dirnames if not os.path.islink(os.path.join(dirpath, d))]
        for name in filenames:
            fp = Path(dirpath) / name
            if fp.is_symlink():
                continue
            try:
                total += fp.stat().st_size
            except OSError:
                pass
    return total


def curl_bytes(url: str, dest: Path) -> int:
    dest.parent.mkdir(parents=True, exist_ok=True)
    if dest.exists() and dest.stat().st_size > 0:
        return dest.stat().st_size
    tmp = dest.with_suffix(dest.suffix + ".partial")
    check(["curl", "-fsSL", "--retry", "3", "-o", str(tmp), url])
    tmp.replace(dest)
    return dest.stat().st_size


def npm_tarball(package: str, version: str, cache: Path) -> tuple[int, str]:
    proc = check(
        ["npm", "view", f"{package}@{version}", "dist.tarball", "--json"],
        capture_output=True,
        env={**os.environ, "NPM_CONFIG_UPDATE_NOTIFIER": "false", "npm_config_update_notifier": "false"},
    )
    raw = proc.stdout.strip()
    try:
        url = json.loads(raw)
    except json.JSONDecodeError:
        match = re.search(r"https://\S+", raw)
        if not match:
            raise
        url = match.group(0).rstrip('"')
    if isinstance(url, list):
        url = url[0]
    safe = re.sub(r"[^A-Za-z0-9._-]+", "_", f"{package}-{version}") + ".tgz"
    size = curl_bytes(url, cache / safe)
    return size, url


def collect_hardware() -> dict:
    os_release = {}
    rel = Path("/etc/os-release")
    if rel.exists():
        for line in rel.read_text().splitlines():
            if "=" in line:
                k, v = line.split("=", 1)
                os_release[k] = v.strip().strip('"')
    model = ""
    bogomips = None
    cpuinfo = Path("/proc/cpuinfo").read_text(errors="replace")
    for line in cpuinfo.splitlines():
        if line.lower().startswith("model name") and not model:
            model = line.split(":", 1)[1].strip()
        if line.lower().startswith("bogomips") and bogomips is None:
            try:
                bogomips = float(line.split(":", 1)[1].strip())
            except ValueError:
                bogomips = None
    hv = None
    lscpu = run(["lscpu"], capture_output=True)
    if lscpu.returncode == 0:
        for line in lscpu.stdout.splitlines():
            if line.lower().startswith("hypervisor vendor:"):
                hv = line.split(":", 1)[1].strip() or None
    mem = 0
    for line in Path("/proc/meminfo").read_text().splitlines():
        if line.startswith("MemTotal:"):
            mem = int(line.split()[1]) * 1024
            break
    hyperfine = run(["hyperfine", "--version"], capture_output=True)
    gnu = run(["/usr/bin/time", "--version"], capture_output=True)
    node = run(["node", "--version"], capture_output=True)
    return {
        "timestamp_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
        "date_utc": time.strftime("%Y-%m-%d", time.gmtime()),
        "os": os_release.get("PRETTY_NAME", platform.platform()),
        "kernel": platform.release(),
        "arch": platform.machine(),
        "cpu_model": model,
        "cpu_count": os.cpu_count(),
        "bogomips": bogomips,
        "hypervisor": hv,
        "mem_total_bytes": mem,
        "hyperfine": (hyperfine.stdout or hyperfine.stderr or "").strip(),
        "gnu_time": (gnu.stdout or gnu.stderr or "").splitlines()[0] if gnu.returncode == 0 or gnu.stderr or gnu.stdout else None,
        "node": (node.stdout or "").strip(),
        "python": platform.python_version(),
    }


class Layout:
    def __init__(self, tools: Path):
        self.tools = tools
        self.tools.mkdir(parents=True, exist_ok=True)
        self.cache = tools / "downloads"
        self.cache.mkdir(exist_ok=True)
        self.homes = tools / "homes"
        self.homes.mkdir(exist_ok=True)

    def prefix(self, install_id: str) -> Path:
        return self.tools / "prefixes" / install_id

    def home(self, install_id: str) -> Path:
        path = self.homes / install_id
        path.mkdir(parents=True, exist_ok=True)
        return path


def install_all(layout: Layout) -> dict[str, dict]:
    """Return install metadata keyed by install id."""
    meta: dict[str, dict] = {}
    for spec in PINS["installs"]:
        log(f"install {spec['id']} {spec['package']}@{spec['version']}")
        if spec["manager"] == "npm":
            meta[spec["id"]] = install_npm(layout, spec)
        elif spec["manager"] == "pip":
            meta[spec["id"]] = install_pip(layout, spec)
        else:
            raise SystemExit(f"unknown manager {spec['manager']}")
    return meta


def install_npm(layout: Layout, spec: dict) -> dict:
    prefix = layout.prefix(spec["id"])
    marker = prefix / ".pin"
    want = f"{spec['package']}@{spec['version']}"
    if marker.is_file() and marker.read_text().strip() == want and (prefix / "bin").exists():
        log(f"  reuse {prefix}")
    else:
        if prefix.exists():
            shutil.rmtree(prefix)
        prefix.mkdir(parents=True)
        check(
            [
                "npm",
                "install",
                "--global",
                "--prefix",
                str(prefix),
                "--no-audit",
                "--no-fund",
                want,
            ],
            env={**os.environ, "NPM_CONFIG_UPDATE_NOTIFIER": "false"},
        )
        marker.write_text(want + "\n")
    bins = {}
    for name in spec["bins"]:
        path = prefix / "bin" / name
        if not path.exists():
            raise SystemExit(f"missing bin {path}")
        bins[name] = str(path)
    info = {
        "id": spec["id"],
        "manager": "npm",
        "package": spec["package"],
        "version": spec["version"],
        "prefix": str(prefix),
        "bins": bins,
        "home": str(layout.home(spec["id"])),
    }
    if spec.get("bootstrap_release_binary"):
        info.update(bootstrap_bwn(layout, spec, bins["bwn"]))
    info["sizes"] = measure_npm_sizes(layout, spec, prefix, info)
    return info


def bootstrap_bwn(layout: Layout, spec: dict, bwn: str) -> dict:
    home = layout.home(spec["id"])
    binary = home / "bin" / spec["version"] / "buildwithnexus"
    if not binary.is_file():
        env = base_env(home, {"NEXUS_HOME": str(home), "BWN_ALLOW_BOOTSTRAP": "1"})
        proc = run([bwn, "--bootstrap", "--help"], env=env, capture_output=True, timeout=180)
        if proc.returncode != 0 or not binary.is_file():
            raise SystemExit(
                "bwn bootstrap failed\n"
                + (proc.stdout or "")[-2000:]
                + "\n"
                + (proc.stderr or "")[-2000:]
            )
    target = linux_target()
    asset = f"buildwithnexus-{target}"
    url = f"https://github.com/Garretts-Apps/buildwithnexus/releases/download/v{spec['version']}/{asset}"
    asset_path = layout.cache / asset
    asset_bytes = curl_bytes(url, asset_path)
    return {
        "native_binary": str(binary),
        "native_binary_bytes": binary.stat().st_size,
        "release_asset": asset,
        "release_asset_url": url,
        "release_asset_bytes": asset_bytes,
    }


def _is_installed_manifest(pkg_json: Path) -> bool:
    """True for node_modules/<name>/package.json and node_modules/@scope/<name>/package.json."""
    parent = pkg_json.parent
    grand = parent.parent
    if grand.name == "node_modules":
        return True
    return grand.parent.name == "node_modules" and grand.name.startswith("@")


def npm_packages(prefix: Path) -> list[tuple[str, str]]:
    found: list[tuple[str, str]] = []
    modules = prefix / "lib" / "node_modules"
    if not modules.exists():
        return found
    for pkg_json in modules.rglob("package.json"):
        if not _is_installed_manifest(pkg_json):
            continue
        try:
            data = json.loads(pkg_json.read_text())
        except (OSError, json.JSONDecodeError):
            continue
        name = data.get("name")
        version = data.get("version")
        if name and version:
            found.append((name, version))
    return sorted(set(found))


def measure_npm_sizes(layout: Layout, spec: dict, prefix: Path, info: dict) -> dict:
    parts = []
    unresolved = []
    for name, version in npm_packages(prefix):
        try:
            size, url = npm_tarball(name, version, layout.cache)
        except subprocess.CalledProcessError as exc:
            unresolved.append({"package": name, "version": version, "error": str(exc)})
            continue
        parts.append({"package": name, "version": version, "bytes": size, "url": url})
    top = next((p for p in parts if p["package"] == spec["package"] and p["version"] == spec["version"]), None)
    download = sum(p["bytes"] for p in parts)
    install = du_bytes(prefix)
    notes = [
        "download_bytes is the sum of the npm tarballs for every package actually installed under this prefix, measured by downloading each tarball.",
    ]
    release_bytes = info.get("release_asset_bytes")
    if release_bytes:
        download += release_bytes
        install += info["native_binary_bytes"]
        parts.append(
            {
                "package": info["release_asset"],
                "version": spec["version"],
                "bytes": release_bytes,
                "url": info["release_asset_url"],
                "kind": "github-release-asset",
            }
        )
        notes.append(
            "The npm package does not contain the native binary. First run downloads the GitHub release asset; that asset's byte size is included in download_bytes and the installed binary is included in install_bytes."
        )
    entry = prefix / "bin" / spec["bins"][0]
    return {
        "package_bytes": None if top is None else top["bytes"],
        "package_url": None if top is None else top["url"],
        "download_bytes": download,
        "download_parts": parts,
        "download_unresolved": unresolved,
        "install_bytes": install,
        "entrypoint": str(entry),
        "entrypoint_bytes": entry.stat().st_size,
        "notes": notes,
    }


def install_pip(layout: Layout, spec: dict) -> dict:
    prefix = layout.prefix(spec["id"])
    marker = prefix / ".pin"
    want = f"{spec['package']}=={spec['version']}"
    venv_py = prefix / "bin" / "python"
    if not (marker.is_file() and marker.read_text().strip() == want and venv_py.exists()):
        if prefix.exists():
            shutil.rmtree(prefix)
        check([sys.executable, "-m", "venv", str(prefix)])
        check(
            [str(venv_py), "-m", "pip", "install", "--disable-pip-version-check", "--no-cache-dir", want],
            timeout=600,
        )
        marker.write_text(want + "\n")
    wheel_dir = layout.cache / f"pip-{spec['id']}"
    if wheel_dir.exists():
        shutil.rmtree(wheel_dir)
    wheel_dir.mkdir(parents=True)
    pkg_dir = wheel_dir / "package"
    all_dir = wheel_dir / "all"
    pkg_dir.mkdir()
    all_dir.mkdir()
    pip = str(prefix / "bin" / "pip")
    check(
        [pip, "download", "--disable-pip-version-check", "--no-deps", "-d", str(pkg_dir), want],
        timeout=180,
        capture_output=True,
    )
    check(
        [pip, "download", "--disable-pip-version-check", "-d", str(all_dir), want],
        timeout=300,
        capture_output=True,
    )
    pkg_files = [p for p in pkg_dir.iterdir() if p.is_file()]
    all_files = [p for p in all_dir.iterdir() if p.is_file()]
    if len(pkg_files) != 1:
        raise SystemExit(f"expected one aider wheel, found {pkg_files}")
    entry = prefix / "bin" / "aider"
    return {
        "id": spec["id"],
        "manager": "pip",
        "package": spec["package"],
        "version": spec["version"],
        "prefix": str(prefix),
        "bins": {"aider": str(entry)},
        "home": str(layout.home(spec["id"])),
        "sizes": {
            "package_bytes": pkg_files[0].stat().st_size,
            "package_url": pkg_files[0].name,
            "download_bytes": sum(p.stat().st_size for p in all_files),
            "download_parts": [{"file": p.name, "bytes": p.stat().st_size} for p in sorted(all_files)],
            "download_unresolved": [],
            "install_bytes": du_bytes(prefix),
            "entrypoint": str(entry),
            "entrypoint_bytes": entry.stat().st_size,
            "notes": [
                "package_bytes is the aider-chat wheel alone (pip download --no-deps).",
                "download_bytes is every wheel pip download fetches for that pin, including dependencies.",
                "install_bytes is the virtualenv on disk after pip install, including pip itself.",
            ],
        },
    }


def command_argv(cmd: dict, installs: dict[str, dict]) -> tuple[list[str], dict]:
    inst = installs[cmd["install"]]
    home = Path(inst["home"])
    extra = {}
    if cmd["install"] == "bwn":
        extra["NEXUS_HOME"] = str(home)
    env = base_env(home, extra)
    # Put this install's bin first so the timed command is the pin, not another copy.
    env["PATH"] = str(Path(inst["prefix"]) / "bin") + ":" + env["PATH"]
    if cmd.get("launch") == "native":
        argv0 = inst["native_binary"]
    else:
        argv0 = inst["bins"][cmd["bin"]]
    return [argv0], env


def capture_version(cmd: dict, installs: dict) -> dict:
    argv, env = command_argv(cmd, installs)
    proc = run(argv + ["--version"], env=env, capture_output=True, timeout=60)
    text = ((proc.stdout or "") + (proc.stderr or "")).strip()
    ok = proc.returncode == 0 and cmd["version_expect"] in text
    return {
        "id": cmd["id"],
        "display": cmd["display"],
        "argv0": argv[0],
        "exit_code": proc.returncode,
        "version_text": text[:500],
        "expect": cmd["version_expect"],
        "ok": ok,
    }


def hyperfine(argv: list[str], env: dict, out: Path, *, cold: bool, runs: int) -> dict:
    command = " ".join(argv)
    hf = [
        "hyperfine",
        "--shell=none",
        "--runs",
        str(runs),
        "--warmup",
        "0" if cold else "3",
        "--export-json",
        str(out),
    ]
    if cold:
        hf.extend(
            [
                "--prepare",
                "sudo -n sh -c 'sync; echo 3 > /proc/sys/vm/drop_caches'",
            ]
        )
    hf.append(command)
    proc = run(hf, env=env, capture_output=True, timeout=1800)
    raw = {}
    if out.exists():
        raw = json.loads(out.read_text())
    result = (raw.get("results") or [{}])[0]
    times = result.get("times") or []
    ok = proc.returncode == 0 and len(times) >= runs and "mean" in result and "median" in result
    return {
        "ok": ok,
        "exit_code": proc.returncode,
        "command": command,
        "cold": cold,
        "runs": len(times),
        "mean_s": result.get("mean"),
        "median_s": result.get("median"),
        "stddev_s": result.get("stddev"),
        "min_s": result.get("min"),
        "max_s": result.get("max"),
        "times_s": times,
        "hyperfine_stdout": (proc.stdout or "")[-2000:],
        "hyperfine_stderr": (proc.stderr or "")[-2000:],
        "export": str(out.name),
    }


def prime(argv: list[str], env: dict) -> None:
    run(argv + ["--version"], env=env, capture_output=True, timeout=60)


def measure_startup(cmd: dict, installs: dict, results: Path, runs: int) -> dict:
    argv, env = command_argv(cmd, installs)
    prime(argv, env)
    out = {}
    for flag, name in (("--version", "version"), ("--help", "help")):
        for cold, label in ((False, "warm"), (True, "cold")):
            export = results / "raw" / f"startup-{cmd['id']}-{name}-{label}.json"
            export.parent.mkdir(parents=True, exist_ok=True)
            log(f"startup {cmd['id']} {name} {label}")
            out[f"{name}_{label}"] = hyperfine(argv + [flag], env, export, cold=cold, runs=runs)
    return out


def parse_rss_kib(text: str) -> int | None:
    for line in text.splitlines():
        if "Maximum resident set size" in line:
            parts = line.split(":")
            if len(parts) == 2:
                return int(parts[1].strip())
    return None


def measure_memory(cmd: dict, installs: dict, runs: int) -> dict:
    argv, env = command_argv(cmd, installs)
    samples = []
    raw_lines = []
    for i in range(runs):
        proc = run(
            ["/usr/bin/time", "-v", "--", *argv, "--version"],
            env=env,
            capture_output=True,
            timeout=60,
        )
        blob = (proc.stderr or "") + "\n" + (proc.stdout or "")
        kib = parse_rss_kib(blob)
        raw_lines.append({"run": i + 1, "kib": kib, "exit_code": proc.returncode})
        if kib is None or proc.returncode != 0:
            return {
                "ok": False,
                "command": argv[0] + " --version",
                "samples": raw_lines,
                "error": "GNU time did not report Maximum resident set size, or --version failed",
            }
        samples.append(kib)
    return {
        "ok": True,
        "command": argv[0] + " --version",
        "tool": "/usr/bin/time -v",
        "field": "Maximum resident set size (kbytes)",
        "unit": "GNU time kbytes (1024-byte kibibytes), top-level process only",
        "samples_kib": samples,
        "median_kib": median([float(s) for s in samples]),
        "note": "GNU time reports the top-level process. A Node launcher's figure is that Node process, not a child native binary.",
    }


def strip_ansi(data: bytes) -> str:
    return ANSI.sub("", data.decode("utf-8", "replace"))


def measure_prompt_once(argv: list[str], env: dict, pattern: str | None, timeout_s: float) -> dict:
    import fcntl
    import pty
    import select
    import struct
    import termios

    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 24, 80, 0, 0))
    cwd = Path(env["HOME"]) / "empty-cwd"
    cwd.mkdir(parents=True, exist_ok=True)
    proc = subprocess.Popen(
        argv,
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        cwd=cwd,
        start_new_session=True,
    )
    os.close(slave)
    start = time.perf_counter()
    buf = b""
    matched_at = None
    deadline = start + timeout_s
    try:
        while time.perf_counter() < deadline:
            if proc.poll() is not None and not select.select([master], [], [], 0)[0]:
                break
            ready, _, _ = select.select([master], [], [], 0.05)
            now = time.perf_counter()
            if ready:
                try:
                    chunk = os.read(master, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                buf += chunk
                if pattern and matched_at is None and pattern in strip_ansi(buf):
                    matched_at = now - start
            if len(buf) > 200_000:
                break
    finally:
        try:
            os.killpg(proc.pid, 15)
        except ProcessLookupError:
            pass
        try:
            proc.wait(timeout=2)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(proc.pid, 9)
            except ProcessLookupError:
                pass
            proc.wait(timeout=2)
        os.close(master)
    text = strip_ansi(buf)
    return {
        "matched": matched_at is not None,
        "seconds": matched_at,
        "exit_code": proc.poll(),
        "transcript": text[-4000:],
    }


def seed_bwn_settings(home: Path) -> None:
    home.mkdir(parents=True, exist_ok=True)
    settings = {
        "provider": "custom",
        "model": "bench",
        "base_url": "http://127.0.0.1:9/v1",
        "permission": "ask",
        "sandbox": "off",
        "auto_update": "off",
        "auto_commit": False,
        "repo_map": False,
        "reasoning_effort": "off",
    }
    (home / "settings.json").write_text(json.dumps(settings, indent=2) + "\n")


def measure_prompt(cmd: dict, installs: dict) -> dict:
    pattern = (PINS.get("prompts") or {}).get(cmd["id"], {}).get("pattern")
    argv, env = command_argv(cmd, installs)
    if cmd["install"] == "bwn":
        seed_bwn_settings(Path(env["NEXUS_HOME"]))
        env["CUSTOM_API_KEY"] = "sk-bench"
        env["BWN_NO_AUTO_UPDATE"] = "1"
    log(f"prompt {cmd['id']}")
    if not pattern:
        # One look, so the result records what the screen actually did.
        # A time is published only after a pattern from that transcript is pinned.
        sample = measure_prompt_once(argv, env, None, float(PINS["prompt_timeout_s"]))
        return {
            "ok": False,
            "status": "not_measured",
            "reason": "No ready-prompt pattern is pinned for this tool. A pattern is only timed after it has been seen in that tool's own PTY transcript.",
            "process_exited": sample["exit_code"] is not None,
            "transcript": sample["transcript"],
        }
    runs = []
    for _ in range(PINS["prompt_runs"]):
        one = measure_prompt_once(argv, env, pattern, float(PINS["prompt_timeout_s"]))
        runs.append(one)
    seconds = [r["seconds"] for r in runs if r["matched"] and r["seconds"] is not None]
    if len(seconds) != len(runs):
        return {
            "ok": False,
            "status": "error",
            "pattern": pattern,
            "runs": [{k: v for k, v in r.items()} for r in runs],
            "reason": "The ready pattern did not appear in every run.",
        }
    return {
        "ok": True,
        "status": "measured",
        "pattern": pattern,
        "definition": "Seconds from process start until the PTY transcript contains the pattern. The process is still running (it was killed after the match). Started in an empty HOME, 80x24.",
        "runs_s": seconds,
        "median_s": median(seconds),
        "mean_s": sum(seconds) / len(seconds),
        "transcript_last": runs[-1]["transcript"],
    }


def git_fixture(dest: Path) -> None:
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    (dest / "README.md").write_text("fixture\n")
    (dest / "src").mkdir()
    (dest / "src" / "hello.py").write_text("print('hello')\n")
    check(["git", "init", "-q"], cwd=dest)
    check(["git", "config", "user.email", "bench@example.com"], cwd=dest)
    check(["git", "config", "user.name", "bench"], cwd=dest)
    check(["git", "add", "."], cwd=dest)
    check(["git", "commit", "-q", "-m", "fixture"], cwd=dest)


def task_succeeded(proc: subprocess.CompletedProcess, out_file: Path | None, requests: list) -> bool:
    """The prompt itself contains the token, so a tool that only echoes it does not count."""
    posts = [r for r in requests if r.get("method") == "POST"]
    blob = (proc.stdout or "") + "\n" + (proc.stderr or "")
    if out_file and out_file.exists():
        blob += "\n" + out_file.read_text(errors="replace")
    return proc.returncode == 0 and bool(posts) and TOKEN in blob


def measure_task(cmd: dict, installs: dict, base_url: str, work: Path, mock_requests: list) -> dict:
    spec = task_command(cmd, installs, base_url, work)
    if spec.get("status") == "skipped" or spec.get("skip"):
        return spec
    argv = spec["argv"]
    env = spec["env"]
    cwd = spec["cwd"]
    out_file = Path(spec["out_file"]) if spec.get("out_file") else None
    runs = []
    log(f"task {cmd['id']}")
    before = len(mock_requests)
    for i in range(PINS["task_runs"]):
        run_dir = Path(cwd)
        # Fresh tree each run so one tool's edits don't leak into the next.
        git_fixture(run_dir)
        if out_file:
            out_file.parent.mkdir(parents=True, exist_ok=True)
            if out_file.exists():
                out_file.unlink()
        start_n = len(mock_requests)
        t0 = time.perf_counter()
        try:
            proc = run(argv, env=env, cwd=run_dir, capture_output=True, timeout=PINS["task_timeout_s"])
            elapsed = time.perf_counter() - t0
            timed_out = False
        except subprocess.TimeoutExpired as exc:
            elapsed = time.perf_counter() - t0
            proc = subprocess.CompletedProcess(argv, 124, exc.stdout or "", exc.stderr or "")
            timed_out = True
        new_requests = mock_requests[start_n:]
        ok = (not timed_out) and task_succeeded(proc, out_file, new_requests)
        runs.append(
            {
                "ok": ok,
                "seconds": elapsed if ok else None,
                "elapsed_s": elapsed,
                "exit_code": proc.returncode,
                "timed_out": timed_out,
                "stdout": (proc.stdout or "")[-1500:],
                "stderr": (proc.stderr or "")[-1500:],
                "mock_requests": new_requests,
            }
        )
        if not ok:
            return {
                "ok": False,
                "status": "error",
                "methodology": spec["methodology"],
                "command": argv,
                "runs": runs,
                "reason": "A run did not exit 0 with BENCHMARK_OK in its output. No time is published for this tool.",
            }
    seconds = [r["seconds"] for r in runs]
    return {
        "ok": True,
        "status": "measured",
        "methodology": spec["methodology"],
        "command": argv,
        "runs_s": seconds,
        "median_s": median(seconds),
        "mean_s": sum(seconds) / len(seconds),
        "mock_requests_total": len(mock_requests) - before,
        "last_stdout": runs[-1]["stdout"],
    }


def task_command(cmd: dict, installs: dict, base_url: str, work: Path) -> dict:
    """How to point this tool at the mock. Skip when it cannot speak OpenAI."""
    if cmd["id"] in ("claude",):
        return {
            "ok": False,
            "status": "skipped",
            "reason": "Claude Code speaks the Anthropic Messages API (ANTHROPIC_BASE_URL). It cannot be pointed at an OpenAI-compatible chat or responses endpoint, so it is not given a fake time.",
        }
    if cmd["id"] == "gemini":
        return {
            "ok": False,
            "status": "skipped",
            "reason": "Gemini CLI's GOOGLE_GEMINI_BASE_URL is the Gemini API base (generateContent), not an OpenAI-compatible chat or responses URL. It is not given a fake time.",
        }
    argv, env = command_argv(cmd, installs)
    home = Path(env["HOME"])
    cwd = work / cmd["id"]
    methodology = (
        "Wall clock of the process against a local OpenAI-compatible mock that returns the "
        "fixed text BENCHMARK_OK for every completion. The mock does no model work. "
        f"The task text is: {TASK_TEXT}"
    )
    if cmd["id"] in ("bwn-native", "bwn-npm"):
        env["CUSTOM_API_KEY"] = "sk-bench"
        env["BWN_NO_AUTO_UPDATE"] = "1"
        # NEXUS_HOME stays the install home: the npm launcher finds the binary there.
        # --provider and --base-url override whatever the prompt measurement saved.
        return {
            "argv": argv
            + [
                "--provider",
                "custom",
                "--base-url",
                base_url,
                "--model",
                "bench",
                "--permission-mode",
                "readonly",
                "--sandbox",
                "off",
                "--no-auto-commit",
                "--no-repo-map",
                "run",
                TASK_TEXT,
            ],
            "env": env,
            "cwd": str(cwd),
            "methodology": methodology
            + " Flags keep the run from stopping on an approval prompt. Endpoint: --provider custom --base-url <mock>/v1.",
        }
    if cmd["id"] == "aider":
        env["OPENAI_API_KEY"] = "sk-bench"
        return {
            "argv": argv
            + [
                "--model",
                "openai/gpt-4o-mini",
                "--openai-api-base",
                base_url,
                "--openai-api-key",
                "sk-bench",
                "--no-git",
                "--no-stream",
                "--no-pretty",
                "--no-check-update",
                "--no-analytics",
                "--no-show-model-warnings",
                "--yes-always",
                "--message",
                TASK_TEXT,
            ],
            "env": env,
            "cwd": str(cwd),
            "methodology": methodology
            + " The model name is a label Aider already accepts; the completion bytes come from --openai-api-base, the mock.",
        }
    if cmd["id"] == "codex":
        codex_home = home / "codex-task"
        if codex_home.exists():
            shutil.rmtree(codex_home)
        codex_home.mkdir(parents=True)
        (codex_home / "config.toml").write_text(
            "\n".join(
                [
                    'model = "bench"',
                    'model_provider = "bench"',
                    'approval_policy = "never"',
                    'sandbox_mode = "read-only"',
                    "",
                    "[model_providers.bench]",
                    'name = "bench"',
                    f'base_url = "{base_url}"',
                    'env_key = "OPENAI_API_KEY"',
                    'wire_api = "responses"',
                    "requires_openai_auth = false",
                    "",
                ]
            )
        )
        env["CODEX_HOME"] = str(codex_home)
        env["OPENAI_API_KEY"] = "sk-bench"
        out_file = codex_home / "last.txt"
        return {
            "argv": argv
            + [
                "exec",
                "--skip-git-repo-check",
                "--color",
                "never",
                "-s",
                "read-only",
                "-a",
                "never",
                "-o",
                str(out_file),
                TASK_TEXT,
            ],
            "env": env,
            "cwd": str(cwd),
            "out_file": str(out_file),
            "methodology": methodology
            + " Codex 0.169 rejects wire_api=chat, so the mock's Responses API is used (wire_api=responses), which is the OpenAI endpoint this CLI can be pointed at.",
        }
    if cmd["id"] == "opencode":
        cfg = home / "opencode-task.json"
        cfg.write_text(
            json.dumps(
                {
                    "model": "openai/gpt-4o-mini",
                    "provider": {
                        "openai": {
                            "options": {
                                "baseURL": base_url,
                                "apiKey": "sk-bench",
                            }
                        }
                    },
                },
                indent=2,
            )
        )
        env["OPENCODE_CONFIG"] = str(cfg)
        env["OPENAI_API_KEY"] = "sk-bench"
        env["OPENAI_BASE_URL"] = base_url
        env["OPENCODE_DISABLE_AUTOUPDATE"] = "1"
        env["OPENCODE_DISABLE_MODELS_FETCH"] = "1"
        return {
            "argv": argv + ["run", "--pure", "--model", "openai/gpt-4o-mini", TASK_TEXT],
            "env": env,
            "cwd": str(cwd),
            "methodology": methodology
            + " OpenCode is pointed at the mock with provider.options.baseURL and OPENAI_BASE_URL. Model-catalog fetch and auto-update are off so the timed run cannot leave the mock.",
        }
    return {"ok": False, "status": "skipped", "reason": f"No mock invocation is defined for {cmd['id']}."}


def size_row(cmd: dict, installs: dict) -> dict:
    inst = installs[cmd["install"]]
    sizes = dict(inst["sizes"])
    native = inst.get("native_binary_bytes")
    if cmd.get("launch") == "native":
        # cargo/prebuilt entry: the binary is the package the user runs.
        sizes = {
            "package_bytes": native,
            "package_url": inst.get("release_asset_url"),
            "download_bytes": inst.get("release_asset_bytes"),
            "download_parts": [
                {
                    "package": inst.get("release_asset"),
                    "bytes": inst.get("release_asset_bytes"),
                    "url": inst.get("release_asset_url"),
                    "kind": "github-release-asset",
                }
            ],
            "download_unresolved": [],
            "install_bytes": native,
            "entrypoint": inst.get("native_binary"),
            "entrypoint_bytes": native,
            "binary_bytes": native,
            "notes": [
                "This row is the release binary invoked directly (the cargo-install / prebuilt entry), not the npm launcher.",
                "download_bytes is the GitHub release asset. cargo install builds from source instead of downloading this file; the asset is what the npm launcher downloads on first run.",
            ],
        }
    else:
        sizes["binary_bytes"] = native or sizes.get("entrypoint_bytes")
    sizes["binary_bytes"] = sizes.get("binary_bytes")
    return sizes


def rankings(summary_tools: dict) -> dict:
    """Lowest value wins. Tools without a measured number are left out."""

    def collect(getter):
        rows = []
        for tid, tool in summary_tools.items():
            val = getter(tool)
            if isinstance(val, (int, float)):
                rows.append({"id": tid, "display": tool["display"], "value": val})
        rows.sort(key=lambda r: r["value"])
        return rows

    metrics = {
        "startup_version_warm_median_s": lambda t: (t.get("startup") or {}).get("version_warm", {}).get("median_s"),
        "startup_version_cold_median_s": lambda t: (t.get("startup") or {}).get("version_cold", {}).get("median_s"),
        "startup_help_warm_median_s": lambda t: (t.get("startup") or {}).get("help_warm", {}).get("median_s"),
        "startup_help_cold_median_s": lambda t: (t.get("startup") or {}).get("help_cold", {}).get("median_s"),
        "memory_median_kib": lambda t: (t.get("memory") or {}).get("median_kib"),
        "install_bytes": lambda t: (t.get("sizes") or {}).get("install_bytes"),
        "download_bytes": lambda t: (t.get("sizes") or {}).get("download_bytes"),
        "binary_bytes": lambda t: (t.get("sizes") or {}).get("binary_bytes"),
        "package_bytes": lambda t: (t.get("sizes") or {}).get("package_bytes"),
        "prompt_median_s": lambda t: (t.get("prompt") or {}).get("median_s") if (t.get("prompt") or {}).get("ok") else None,
        "task_median_s": lambda t: (t.get("task") or {}).get("median_s") if (t.get("task") or {}).get("ok") else None,
    }
    out = {}
    for name, getter in metrics.items():
        rows = collect(getter)
        out[name] = {
            "lower_is_better": True,
            "order": rows,
            "fastest": rows[0]["id"] if rows else None,
        }
    return out


def render_summary_md(summary: dict) -> str:
    lines = [
        "<!-- Generated by bench/run.py from summary.json. Do not edit by hand. -->",
        "",
        f"# Competitor benchmark {summary['run_id']}",
        "",
        f"Date (UTC): {summary['hardware']['date_utc']}",
        f"Machine: {summary['hardware']['cpu_model']}, {summary['hardware']['cpu_count']} CPUs, {summary['hardware']['mem_total_bytes']} bytes RAM, {summary['hardware']['os']}, kernel {summary['hardware']['kernel']}.",
        "",
        "Startup times are hyperfine medians in milliseconds (`--shell=none`).",
        "",
        "| Tool | version warm | version cold | help warm | help cold | RSS KiB | install | download | binary |",
        "|---|---:|---:|---:|---:|---:|---:|---:|---:|",
    ]

    def ms(block, key):
        val = (block or {}).get(key, {})
        med = val.get("median_s") if isinstance(val, dict) else None
        if med is None:
            return "—"
        return f"{med * 1000:.3f}"

    def num(val):
        if val is None:
            return "—"
        return str(int(val)) if isinstance(val, float) and val == int(val) else str(val)

    for tid, tool in summary["tools"].items():
        st = tool.get("startup") or {}
        mem = tool.get("memory") or {}
        sz = tool.get("sizes") or {}
        lines.append(
            "| {display} | {vw} | {vc} | {hw} | {hc} | {rss} | {inst} | {down} | {bin} |".format(
                display=tool["display"],
                vw=ms(st, "version_warm"),
                vc=ms(st, "version_cold"),
                hw=ms(st, "help_warm"),
                hc=ms(st, "help_cold"),
                rss=num(mem.get("median_kib")),
                inst=num(sz.get("install_bytes")),
                down=num(sz.get("download_bytes")),
                bin=num(sz.get("binary_bytes")),
            )
        )
    lines.append("")
    lines.append("## Where the native binary is not the fastest")
    lines.append("")
    native_losses = []
    npm_losses = []
    for metric, info in summary["rankings"].items():
        order = info["order"]
        if not order:
            continue
        winner = order[0]["id"]
        by_id = {row["id"]: row for row in order}
        if "bwn-native" in by_id and winner != "bwn-native":
            native_losses.append(f"- `{metric}`: fastest is {winner} ({by_id[winner]['value']}); bwn native is {by_id['bwn-native']['value']}")
        if "bwn-npm" in by_id and winner != "bwn-npm":
            npm_losses.append(f"- `{metric}`: fastest is {winner} ({by_id[winner]['value']}); bwn (npm) is {by_id['bwn-npm']['value']}")
    lines.append("Native binary:")
    lines.extend(native_losses or ["- none of the measured metrics"])
    lines.append("")
    lines.append("npm launcher:")
    lines.extend(npm_losses or ["- none of the measured metrics"])
    lines.append("")
    return "\n".join(lines)


def execute(results: Path, only: set[str] | None = None) -> dict:
    def want(name: str) -> bool:
        return only is None or name in only

    results.mkdir(parents=True, exist_ok=True)
    (results / "raw").mkdir(exist_ok=True)
    hardware = collect_hardware()
    write_json(results / "hardware.json", hardware)
    layout = Layout(ROOT / ".tools")
    installs = install_all(layout)
    versions = [capture_version(cmd, installs) for cmd in PINS["commands"]]
    write_json(results / "versions.json", {"tools": versions})
    bad = [v for v in versions if not v["ok"]]
    if bad:
        raise SystemExit("version pin mismatch: " + json.dumps(bad, indent=2))

    tools: dict[str, dict] = {}
    for cmd in PINS["commands"]:
        tools[cmd["id"]] = {
            "id": cmd["id"],
            "display": cmd["display"],
            "version_text": next(v["version_text"] for v in versions if v["id"] == cmd["id"]),
            "sizes": size_row(cmd, installs),
        }

    if want("startup"):
        for cmd in PINS["commands"]:
            tools[cmd["id"]]["startup"] = measure_startup(cmd, installs, results, PINS["startup_runs"])
            write_json(results / "startup.json", {k: v.get("startup") for k, v in tools.items()})
    if want("memory"):
        for cmd in PINS["commands"]:
            log(f"memory {cmd['id']}")
            tools[cmd["id"]]["memory"] = measure_memory(cmd, installs, PINS["memory_runs"])
            write_json(results / "memory.json", {k: v.get("memory") for k, v in tools.items()})
    if want("prompt"):
        for cmd in PINS["commands"]:
            tools[cmd["id"]]["prompt"] = measure_prompt(cmd, installs)
            write_json(results / "prompt.json", {k: v.get("prompt") for k, v in tools.items()})

    if want("task"):
        from bench.mock_openai import start

        server, base = start()
        log(f"mock {base}")
        try:
            # Sanity: the mock itself answers before any tool is blamed.
            with urllib.request.urlopen(base + "/models", timeout=5) as resp:
                body = resp.read()
            if TOKEN.encode() not in urllib.request.urlopen(
                urllib.request.Request(
                    base + "/chat/completions",
                    data=json.dumps({"model": "bench", "messages": [{"role": "user", "content": "hi"}]}).encode(),
                    headers={"Content-Type": "application/json"},
                ),
                timeout=5,
            ).read():
                raise SystemExit("mock chat completion did not contain BENCHMARK_OK")
            if b"bench" not in body:
                raise SystemExit("mock model list failed")
            work = layout.tools / "task-work"
            for cmd in PINS["commands"]:
                server.requests.clear()
                tools[cmd["id"]]["task"] = measure_task(cmd, installs, base, work, server.requests)
                write_json(results / "task.json", {k: v.get("task") for k, v in tools.items()})
        finally:
            server.shutdown()

    write_json(results / "sizes.json", {k: v.get("sizes") for k, v in tools.items()})
    summary = {
        "run_id": hardware["timestamp_utc"].replace(":", "-"),
        "hardware": hardware,
        "pins": {"installs": PINS["installs"], "startup_runs": PINS["startup_runs"]},
        "methodology": {
            "startup": "hyperfine --shell=none, 3 warmup runs then 20 measured runs (warm), or 20 measured runs each prepared with sync and echo 3 > /proc/sys/vm/drop_caches (cold). Mean, median and stddev are hyperfine's. One untimed --version primes each tool's empty HOME before the timed runs.",
            "memory": "/usr/bin/time -v on --version, 5 runs, median of Maximum resident set size (kbytes). Top-level process only.",
            "sizes": "download_bytes are files this run fetched (npm tarballs, the GitHub release asset, pip wheels). install_bytes is the byte size of the installed tree. package_bytes is the published package artifact. binary_bytes is the native executable when the tool has one, else the entrypoint script.",
            "prompt": "PTY 80x24. Timed only when a pinned transcript pattern appears. bwn is given a settings file so it does not stop in onboarding, with auto-update off.",
            "task": "Local mock OpenAI server (bench/mock_openai.py) returns BENCHMARK_OK for chat completions and the Responses API. Tools that cannot be pointed at that protocol are skipped. Each published time exited 0 and printed BENCHMARK_OK.",
            "environment": "Clean HOME per tool, no API keys except the dummy key used for the mock. PATH starts with that tool's install prefix.",
        },
        "tools": tools,
    }
    summary["rankings"] = rankings(tools)
    # Only call the run complete when every required startup and memory sample exists.
    required_ok = True
    for tool in tools.values():
        startup = tool.get("startup") or {}
        for key in ("version_warm", "version_cold", "help_warm", "help_cold"):
            if not (startup.get(key) or {}).get("ok"):
                required_ok = False
        if not (tool.get("memory") or {}).get("ok"):
            required_ok = False
        if not tool.get("sizes", {}).get("download_bytes"):
            required_ok = False
    summary["complete"] = required_ok and only is None
    write_json(results / "summary.json", summary)
    (results / "summary.md").write_text(render_summary_md(summary))
    if not required_ok and only is None:
        raise SystemExit("required measurements failed; see summary.json")
    return summary
