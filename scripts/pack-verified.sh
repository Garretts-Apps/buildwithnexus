#!/usr/bin/env bash
# Packs the npm tarball for the version in package.json, with checksums.json
# holding the hash of every release binary.
#
# Release assets can be replaced after publishing, so their .sha256 files
# only catch corruption. Each binary is downloaded from the GitHub release
# and checked against its build-provenance attestation (Sigstore, tied to
# release.yml in this repo) before its hash is recorded; the launcher's
# first-run download trusts only these hashes.
#
# release.yml runs this to test the tarball before crates.io is published,
# and publish.yml runs it again for the tarball it sends to npm, so both
# gates install the same thing.
#
# Needs GH_TOKEN (attestations: read) and GITHUB_REPOSITORY. Writes
# checksums.json and dist/<tarball>, and `tarball=` / `sha256=` to
# $GITHUB_OUTPUT when it is set.
set -euo pipefail

V=$(node -p "require('./package.json').version")
BASE="https://github.com/${GITHUB_REPOSITORY}/releases/download/v${V}"
mkdir -p verified dist
echo '{}' > checksums.json
for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu x86_64-apple-darwin aarch64-apple-darwin x86_64-pc-windows-msvc; do
  ext=""; [[ "$t" == *windows* ]] && ext=".exe"
  asset="buildwithnexus-${t}${ext}"
  curl -fsSL --retry 3 -o "verified/${asset}" "${BASE}/${asset}"
  gh attestation verify "verified/${asset}" \
    --repo "${GITHUB_REPOSITORY}" \
    --signer-workflow "${GITHUB_REPOSITORY}/.github/workflows/release.yml"
  sum=$(sha256sum "verified/${asset}" | cut -d' ' -f1)
  ASSET="$asset" SUM="$sum" node -e '
    const fs = require("fs");
    const j = JSON.parse(fs.readFileSync("checksums.json", "utf8"));
    j[process.env.ASSET] = process.env.SUM;
    fs.writeFileSync("checksums.json", JSON.stringify(j, null, 2) + "\n");
  '
done
cat checksums.json

# Guard the launch path: a syntax error in the wrapper or bootstrap would
# break `npm install` for every user.
node --check bin/buildwithnexus.js
for f in scripts/*.js; do node --check "$f"; done

tgz=$(npm pack --pack-destination dist --json | node -e 'console.log(JSON.parse(require("fs").readFileSync(0, "utf8"))[0].filename)')
sum=$(sha256sum "dist/$tgz" | cut -d' ' -f1)
echo "$sum  $tgz"
if [ -n "${GITHUB_OUTPUT:-}" ]; then
  echo "tarball=$tgz" >> "$GITHUB_OUTPUT"
  echo "sha256=$sum" >> "$GITHUB_OUTPUT"
fi
