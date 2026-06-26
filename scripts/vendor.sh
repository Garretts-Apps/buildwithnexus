#!/usr/bin/env bash
# Vendor the CLI app: pull every Rust dependency into the tree so the harness
# builds fully offline and reproducibly (no network, pinned by Cargo.lock).
#
#   bash scripts/vendor.sh            # vendor into harness/vendor + wire .cargo
#   bash scripts/vendor.sh --tar      # also emit a self-contained tarball
#
# After vendoring:  cargo build --release --offline --manifest-path harness/Cargo.toml
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT/harness"

mkdir -p .cargo
echo "→ vendoring crates into harness/vendor …"
cargo vendor --locked vendor > .cargo/config.toml
echo "→ wrote harness/.cargo/config.toml (offline source replacement)"

if [[ "${1:-}" == "--tar" ]]; then
  out="$ROOT/buildwithnexus-vendored-$(grep '^version' Cargo.toml | head -1 | cut -d'"' -f2).tar.gz"
  echo "→ packing self-contained bundle …"
  tar -czf "$out" -C "$ROOT" \
    harness/Cargo.toml harness/Cargo.lock harness/src harness/vendor harness/.cargo \
    bin scripts package.json README.md LICENSE
  echo "→ $out"
fi

echo "✓ vendored. Offline build:"
echo "    cargo build --release --offline --manifest-path harness/Cargo.toml"
