# buildwithnexus

[![npm version](https://img.shields.io/npm/v/buildwithnexus?style=flat-square&color=blue)](https://www.npmjs.com/package/buildwithnexus)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg?style=flat-square)](https://opensource.org/licenses/MIT)

Launch an autonomous AI runtime with triple-nested VM isolation in one command.

## What It Does

One command bootstraps a complete NEXUS instance:

- **QEMU VM** running Ubuntu 24.04 (auto-installed if missing)
- **Docker** inside the VM for isolated CLI sessions
- **KVM** inside the VM for inner virtual machines (triple nesting)
- **NEXUS server** running on port 4200 with full agent orchestration
- **Cloudflare tunnel** (optional) for remote access

All isolation is mandatory — NEXUS refuses to start unless it detects proper nesting (VM + Docker + KVM).

## Quick Start

```bash
npx buildwithnexus init
```

This walks you through API key setup, VM resource allocation, and boots a fully provisioned NEXUS instance in ~10-25 minutes (first run). Subsequent starts take ~30 seconds.

## Requirements

- **Node.js** >= 18
- ~4GB RAM and ~20GB disk available for the VM
- An Anthropic API key

### macOS (ARM or Intel)

QEMU is installed automatically via Homebrew. If Homebrew isn't installed, get it at [brew.sh](https://brew.sh).

```bash
npx buildwithnexus init
```

### Linux (x64)

QEMU is installed automatically via apt. Requires `sudo` access for package installation.

```bash
npx buildwithnexus init
```

### Windows (via WSL2)

buildwithnexus requires WSL2 with an Ubuntu distribution. Native Windows is not supported.

1. Install WSL2: `wsl --install -d Ubuntu`
2. Open Ubuntu terminal
3. Install Node.js: `curl -fsSL https://deb.nodesource.com/setup_20.x | sudo -E bash - && sudo apt install -y nodejs`
4. Run: `npx buildwithnexus init`

KVM nested virtualization must be enabled in your BIOS/UEFI settings.

## Commands

| Command | Description |
|---------|-------------|
| `buildwithnexus init` | Full scaffolding + VM boot (10 phases) |
| `buildwithnexus start` | Start an existing VM + server |
| `buildwithnexus stop` | Graceful shutdown |
| `buildwithnexus status` | VM / Docker / server / tunnel health |
| `buildwithnexus doctor` | Diagnose QEMU, ports, SSH, disk |
| `buildwithnexus logs [-f]` | Stream server logs |
| `buildwithnexus update` | Upload latest release, rebuild, restart |
| `buildwithnexus destroy [--force]` | Remove VM + all data |
| `buildwithnexus keys set\|list` | Manage API keys |
| `buildwithnexus ssh` | Direct SSH into the VM |
| `buildwithnexus brainstorm [idea]` | Brainstorm an idea with the Chief of Staff |

## Architecture

```
Host (your machine)
  └─ QEMU VM (Ubuntu 24.04)
       ├─ Docker (nexus-cli-sandbox)
       ├─ KVM / libvirt (inner VMs)
       └─ NEXUS server (:4200)
```

Port forwarding: SSH `localhost:2222`, NEXUS `localhost:4200`, HTTPS `localhost:8443`.

## Security

Built-in DLP (Data Loss Prevention) layer protects every data path — zero dependencies, zero configuration:

- **Input Sanitization** — YAML escaping and `shellCommand` tagged templates prevent injection
- **Output Redaction** — API keys auto-redacted from logs, errors, and stdout
- **Secret Validation** — format + injection character checks on all API keys at input
- **File Integrity** — HMAC-SHA256 tamper detection on `.env.keys`
- **Audit Trail** — every sensitive operation logged to `~/.buildwithnexus/audit.log`
- **Environment Scrubbing** — child processes (QEMU, Docker, SSH) never inherit secrets
- **SSH TOFU** — host key pinned on first connect, verified on every subsequent connection
- SSH key-only auth (ed25519, no passwords)
- UFW firewall (deny all, allow 22/80/443/4200)
- auditd enabled (SOC 2 compliance)
- Docker hardened (no-new-privileges, log rotation, cap-drop ALL)
- API keys stored in `~/.buildwithnexus/.env.keys` with `0600` permissions
- All directories created with `0700` permissions
- Nesting enforcement guard prevents running outside VM isolation

## Links

- **npm:** [npmjs.com/package/buildwithnexus](https://www.npmjs.com/package/buildwithnexus)
- **Docs:** [buildwithnexus.dev](https://buildwithnexus.dev)
- **GitHub:** [github.com/Garretts-Apps/buildwithnexus](https://github.com/Garretts-Apps/buildwithnexus)

## License

MIT
