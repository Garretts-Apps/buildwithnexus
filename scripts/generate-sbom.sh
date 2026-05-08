#!/usr/bin/env bash
# Generate a CycloneDX SBOM for the production dependency tree of buildwithnexus.
# Output: dist/sbom.cdx.json
set -euo pipefail

OUT_DIR="$(dirname "$0")/../dist"
mkdir -p "$OUT_DIR"

npx --yes @cyclonedx/cyclonedx-npm@^2.0.0 \
  --output-format JSON \
  --output-file "$OUT_DIR/sbom.cdx.json" \
  --omit dev \
  --spec-version 1.5

SIZE=$(wc -c < "$OUT_DIR/sbom.cdx.json")
echo "SBOM written to $OUT_DIR/sbom.cdx.json ($SIZE bytes)"
