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
