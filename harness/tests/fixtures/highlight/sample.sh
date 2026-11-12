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
    Cargo.toml Cargo.lock harness/Cargo.toml harness/src harness/vendor harness/.cargo \
    bin scripts package.json README.md LICENSE
  echo "→ $out"
fi

echo "✓ vendored. Offline build:"
echo "    cargo build --release --offline --manifest-path harness/Cargo.toml"
#!/usr/bin/env bash
# Fails when a Linux binary needs a newer glibc than GLIBC_FLOOR in
# scripts/diagnose.js, the floor the launcher's message and the docs state.
# ci.yml runs it on every PR and release.yml on each release binary.
# Usage: scripts/check-glibc-floor.sh <binary>
set -euo pipefail

bin=${1:?usage: check-glibc-floor.sh <binary>}
here=$(cd "$(dirname "$0")" && pwd)
floor=$(node -p "require(process.argv[1]).GLIBC_FLOOR" "$here/diagnose.js")
# grep finding nothing must reach the check below, not end the script silently.
versions=$(readelf -V "$bin" | { grep -o 'GLIBC_[0-9][0-9.]*' || true; } | sed 's/GLIBC_//' | sort -uV)
if [ -z "$versions" ]; then
  echo "::error::$bin has no GLIBC_ symbol versions; is it a glibc-linked ELF binary?"
  exit 1
fi
need=$(tail -n 1 <<<"$versions")
echo "$bin needs glibc $need; the floor is $floor"
if [ "$(printf '%s\n' "$need" "$floor" | sort -V | tail -n 1)" != "$floor" ]; then
  echo "::error::$bin needs glibc $need; the documented floor is $floor (scripts/diagnose.js)."
  echo "Symbols that need more than $floor:"
  readelf -W --dyn-syms "$bin" | grep -o '[^ ]*@GLIBC_[0-9.]*' | sort -u |
    while IFS=@ read -r sym ver; do
      v=${ver#GLIBC_}
      if [ "$(printf '%s\n' "$v" "$floor" | sort -V | tail -n 1)" != "$floor" ]; then echo "  $sym@$ver"; fi
    done
  exit 1
fi
