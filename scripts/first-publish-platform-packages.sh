#!/usr/bin/env bash
# One-time bootstrap for the five buildwithnexus-<platform> npm packages.
#
# npm's OIDC Trusted Publishing cannot CREATE a package — a package must
# exist before a Trusted Publisher can be attached to it. So the very first
# publish of each platform package happens here, from a maintainer machine
# that is `npm login`'d. Afterwards, on npmjs.com attach a Trusted Publisher
# to each package (org Garretts-Apps, repo buildwithnexus, workflow
# publish.yml) and CI publishes every future version via OIDC — no tokens.
#
# Usage: scripts/first-publish-platform-packages.sh <version>
#        (release assets for v<version> must already exist on GitHub)
set -euo pipefail
V="${1:?usage: $0 <version>   e.g. $0 0.12.1}"
BASE="https://github.com/Garretts-Apps/buildwithnexus/releases/download/v${V}"
declare -A T=(
  [linux-x64]=x86_64-unknown-linux-gnu
  [linux-arm64]=aarch64-unknown-linux-gnu
  [darwin-x64]=x86_64-apple-darwin
  [darwin-arm64]=aarch64-apple-darwin
  [win32-x64]=x86_64-pc-windows-msvc
)
cd "$(dirname "$0")/.."
for p in "${!T[@]}"; do
  t=${T[$p]}; ext=""; [[ "$t" == *windows* ]] && ext=".exe"
  if npm view "buildwithnexus-${p}@${V}" version >/dev/null 2>&1; then
    echo "buildwithnexus-${p}@${V} already on npm — skipping"
    continue
  fi
  mkdir -p "npm/${p}/bin"
  curl -fsSL -o "npm/${p}/bin/buildwithnexus${ext}" "${BASE}/buildwithnexus-${t}${ext}"
  curl -fsSL -o "/tmp/bwn-${p}.sha256" "${BASE}/buildwithnexus-${t}${ext}.sha256"
  expected=$(cut -d' ' -f1 "/tmp/bwn-${p}.sha256")
  if command -v sha256sum >/dev/null 2>&1; then
    got=$(sha256sum "npm/${p}/bin/buildwithnexus${ext}" | cut -d' ' -f1)
  else
    got=$(shasum -a 256 "npm/${p}/bin/buildwithnexus${ext}" | cut -d' ' -f1)
  fi
  if [[ "${expected}" != "${got}" ]]; then
    echo "checksum mismatch for ${p} — refusing to publish" >&2
    exit 1
  fi
  chmod +x "npm/${p}/bin/buildwithnexus${ext}"
  node -e "const fs=require('fs'),f='npm/${p}/package.json',j=require('./'+f);j.version='${V}';fs.writeFileSync(f,JSON.stringify(j,null,2)+'\n')"
  (cd "npm/${p}" && npm publish --access public)
  echo "✓ published buildwithnexus-${p}@${V}"
done
cat <<'DONE'

All platform packages are live. Finish the one-time setup:
  1. npmjs.com → each buildwithnexus-<platform> package → Settings →
     Trusted Publisher: GitHub, Garretts-Apps/buildwithnexus, publish.yml
  2. Re-run the publish workflow (Actions → "Publish to npm" → Run workflow,
     version bump: none) — it verifies the platform packages, then publishes
     the main package via OIDC.
For crates.io (also one-time): cargo login, then
  cargo publish -p buildwithnexus && cargo publish -p bwn
and configure Trusted Publishing on each crate's Settings page so CI
publishes future versions via OIDC.
DONE
