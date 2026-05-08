#!/usr/bin/env bash
# Sanity-check a packed buildwithnexus tarball before publishing.
# Run via `npm run verify-package` after `npm pack`.
set -euo pipefail

cd "$(dirname "$0")/.."

# Pack into a deterministic name in /tmp.
TARBALL=$(npm pack --silent --pack-destination /tmp)
TARBALL_PATH="/tmp/$TARBALL"
echo "Packed: $TARBALL_PATH ($(du -h "$TARBALL_PATH" | cut -f1))"

# 1. Required files present.
for required in package/dist/bin.js package/dist/nexus-release.tar.gz package/README.md package/LICENSE package/SECURITY.md; do
  if ! tar tzf "$TARBALL_PATH" | grep -qx "$required"; then
    echo "::error::Missing required file in packed tarball: $required"
    exit 1
  fi
done
echo "✓ Required files present."

# 2. No accidental secret patterns.
LEAK_PATTERNS=(
  'sk-ant-[A-Za-z0-9_-]{30,}'
  'AKIA[0-9A-Z]{16}'
  'ghp_[A-Za-z0-9]{36}'
  'github_pat_[A-Za-z0-9_]{82}'
  'BEGIN (RSA |OPENSSH |EC |DSA )?PRIVATE KEY'
)
TMP_EXTRACT=$(mktemp -d)
trap 'rm -rf "$TMP_EXTRACT"' EXIT
tar xzf "$TARBALL_PATH" -C "$TMP_EXTRACT"

for pattern in "${LEAK_PATTERNS[@]}"; do
  if grep -rE "$pattern" "$TMP_EXTRACT" > /dev/null 2>&1; then
    echo "::error::Possible secret leak in packed tarball matching pattern: $pattern"
    grep -rEn "$pattern" "$TMP_EXTRACT" | head -5
    exit 1
  fi
done
echo "✓ No obvious secret patterns in packed tarball."

# 3. No node_modules or test fixtures.
for unwanted in package/node_modules package/tests package/.env package/.git; do
  if tar tzf "$TARBALL_PATH" | grep -q "^$unwanted"; then
    echo "::error::Packed tarball contains $unwanted — adjust the 'files' allowlist."
    exit 1
  fi
done
echo "✓ No unwanted files in packed tarball."

# 4. Tarball size sanity.
SIZE_BYTES=$(stat -c%s "$TARBALL_PATH" 2>/dev/null || stat -f%z "$TARBALL_PATH")
MAX_SIZE=$((50 * 1024 * 1024))  # 50 MB
if [ "$SIZE_BYTES" -gt "$MAX_SIZE" ]; then
  echo "::error::Packed tarball is $SIZE_BYTES bytes (> 50 MB ceiling). Bloat check needed."
  exit 1
fi
echo "✓ Tarball size within ceiling: $SIZE_BYTES bytes."

echo ""
echo "Package verification passed: $TARBALL_PATH"
