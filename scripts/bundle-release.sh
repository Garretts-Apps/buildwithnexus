#!/usr/bin/env bash
# Bundle NEXUS source into a release tarball for npm distribution.
#
# Output: dist/nexus-release.tar.gz, included in the npm package and verified
# at install time by `da-init`.
#
# Reproducibility:
# - SOURCE_DATE_EPOCH (or the nexus HEAD commit time) is used as the mtime for
#   every entry, so two builds from the same nexus SHA produce byte-identical
#   tarballs.
# - File ownership/group are stripped (--owner=0 --group=0 --numeric-owner).
# - File order is forced by piping a sorted file list into tar.
set -euo pipefail

NEXUS_SRC="${NEXUS_SRC:-$(dirname "$0")/../../nexus}"
OUT_DIR="$(dirname "$0")/../dist"

if [ ! -d "$NEXUS_SRC/src" ]; then
  echo "ERROR: NEXUS source not found at $NEXUS_SRC"
  echo "Set NEXUS_SRC to the nexus repo root."
  exit 1
fi

# Pin mtime: prefer SOURCE_DATE_EPOCH (set by CI to the nexus commit SHA's time),
# fall back to the nexus HEAD commit timestamp, then to 0 (the epoch).
if [ -n "${SOURCE_DATE_EPOCH:-}" ]; then
  MTIME="$SOURCE_DATE_EPOCH"
elif [ -d "$NEXUS_SRC/.git" ]; then
  MTIME="$(git -C "$NEXUS_SRC" log -1 --pretty=%ct)"
else
  MTIME=0
fi

# Record the nexus commit SHA (when available) inside the tarball so consumers
# can verify what they got.
SOURCE_INFO="$OUT_DIR/.source-info"
mkdir -p "$OUT_DIR"
{
  echo "package: buildwithnexus"
  echo "bundled-at: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  if [ -d "$NEXUS_SRC/.git" ]; then
    echo "nexus-sha: $(git -C "$NEXUS_SRC" rev-parse HEAD)"
    echo "nexus-ref: $(git -C "$NEXUS_SRC" rev-parse --abbrev-ref HEAD 2>/dev/null || echo 'detached')"
  fi
} > "$SOURCE_INFO"

# Build a deterministic file list. Sorting locale-independently is essential
# for cross-platform reproducibility.
INCLUDE=(
  src
  docker
  requirements.txt
  pyproject.toml
  start.sh
  setup_env.sh
)

EXCLUDE_PATTERNS=(
  '.git'
  '.github'
  '__pycache__'
  '*.pyc'
  '*.pyo'
  '.mypy_cache'
  '.ruff_cache'
  '.pytest_cache'
  '*.db'
  '*.sqlite'
  '*.sqlite3'
  'node_modules'
  'nexus-dashboard'
  'nana-tracker'
  'nana-tracker-rails'
  'docs-hub'
  'output'
  '.env'
  '.env.*'
  '.envrc'
  '*.docx'
  '*.log'
  '.DS_Store'
  '.idea'
  '.vscode'
)

EXCLUDE_ARGS=()
for pat in "${EXCLUDE_PATTERNS[@]}"; do
  EXCLUDE_ARGS+=(--exclude="$pat")
done

LC_ALL=C tar \
  --sort=name \
  --owner=0 --group=0 --numeric-owner \
  --mtime="@$MTIME" \
  "${EXCLUDE_ARGS[@]}" \
  -czf "$OUT_DIR/nexus-release.tar.gz" \
  -C "$NEXUS_SRC" \
  "${INCLUDE[@]}"

# Drop the bundle's source-info into the tarball staging dir so consumers can
# read it without un-taring (we keep a copy at dist/.source-info too).
cp "$SOURCE_INFO" "$OUT_DIR/nexus-source-info.txt"

SIZE=$(du -h "$OUT_DIR/nexus-release.tar.gz" | cut -f1)
SHA=$(sha256sum "$OUT_DIR/nexus-release.tar.gz" | cut -d' ' -f1)
echo "Bundled nexus-release.tar.gz ($SIZE, sha256:$SHA)"
