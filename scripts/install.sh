#!/bin/sh
# Install the native buildwithnexus binary. No sudo, no Node.
#   curl -fsSL https://raw.githubusercontent.com/Garretts-Apps/buildwithnexus/main/scripts/install.sh | sh
#   ./install.sh --version 0.21.0
#   ./install.sh --uninstall
# CI points --file at a built binary and --sha256 at its digest.
set -eu

REPO='Garretts-Apps/buildwithnexus'
VERSION=''
PREFIX="${HOME:-}/.local/bin"
FILE=''
SHA=''
UNINSTALL=0
NO_ATTEST=0
REQUIRE_ATTEST=0

usage() {
  echo "usage: install.sh [--version VER] [--prefix DIR] [--file PATH --sha256 HEX] [--no-attest] [--require-attest] [--uninstall]" >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --version) VERSION=${2:-}; shift 2 ;;
    --prefix) PREFIX=${2:-}; shift 2 ;;
    --file) FILE=${2:-}; shift 2 ;;
    --sha256) SHA=${2:-}; shift 2 ;;
    --no-attest) NO_ATTEST=1; shift ;;
    --require-attest) REQUIRE_ATTEST=1; shift ;;
    --uninstall) UNINSTALL=1; shift ;;
    -h|--help) usage ;;
    *) echo "install.sh: unknown argument: $1" >&2; usage ;;
  esac
done

if [ -z "${HOME:-}" ] && [ "$PREFIX" = "/.local/bin" ]; then
  echo "install.sh: HOME is not set. Pass --prefix." >&2
  exit 1
fi

if [ "$NO_ATTEST" = 1 ] && [ "$REQUIRE_ATTEST" = 1 ]; then
  echo "install.sh: --no-attest and --require-attest cannot be combined" >&2
  exit 2
fi

case "$VERSION" in
  v*) VERSION=${VERSION#v} ;;
esac

if [ "$UNINSTALL" = 1 ]; then
  rm -f "$PREFIX/bwn" "$PREFIX/buildwithnexus"
  echo "removed $PREFIX/bwn and $PREFIX/buildwithnexus"
  exit 0
fi

asset_name() {
  os=$(uname -s)
  arch=$(uname -m)
  case "$os:$arch" in
    Linux:x86_64) echo buildwithnexus-x86_64-unknown-linux-gnu ;;
    Linux:aarch64|Linux:arm64) echo buildwithnexus-aarch64-unknown-linux-gnu ;;
    Darwin:x86_64) echo buildwithnexus-x86_64-apple-darwin ;;
    Darwin:arm64|Darwin:aarch64) echo buildwithnexus-aarch64-apple-darwin ;;
    *) echo "install.sh: no binary for $os $arch. Build from source and put it on PATH." >&2; return 1 ;;
  esac
}

hash_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
    return
  fi
  if command -v shasum >/dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
    return
  fi
  echo "install.sh: need sha256sum or shasum to check the download" >&2
  return 1
}

ASSET=$(asset_name) || exit 1
tmp=$(mktemp)
trap 'rm -f "$tmp" "$tmp.sha"' EXIT

if [ -n "$FILE" ]; then
  [ -n "$SHA" ] || { echo "install.sh: --file needs --sha256" >&2; exit 2; }
  cp "$FILE" "$tmp"
else
  if ! command -v curl >/dev/null 2>&1; then
    echo "install.sh: curl is required" >&2
    exit 1
  fi
  if [ -n "$VERSION" ]; then
    base="https://github.com/$REPO/releases/download/v$VERSION"
  else
    base="https://github.com/$REPO/releases/latest/download"
  fi
  curl -fsSL "$base/$ASSET" -o "$tmp"
  curl -fsSL "$base/$ASSET.sha256" -o "$tmp.sha"
  SHA=$(awk '{print $1}' "$tmp.sha")
fi

got=$(hash_of "$tmp")
if [ "$(printf '%s' "$got" | tr 'A-F' 'a-f')" != "$(printf '%s' "$SHA" | tr 'A-F' 'a-f')" ]; then
  echo "install.sh: checksum mismatch (got $got, expected $SHA). Not installing." >&2
  exit 1
fi

# The checksum above is the install gate. Attestation is checked only when gh
# is logged in. A missing, logged-out, offline, or too-old gh prints one line
# and the install continues. A verification that actually runs and rejects the
# file does not install. --require-attest turns the warning into a failure.
warn_attest() {
  if [ "$REQUIRE_ATTEST" = 1 ]; then
    echo "install.sh: --require-attest: $1" >&2
    exit 1
  fi
  echo "install.sh: $1" >&2
}

attest_rejected() {
  case "$1" in
    *attestations/sha256*|*failed\ to\ verify*|*no\ attestation*|*did\ not\ match*|*verification\ failed*)
      return 0
      ;;
  esac
  return 1
}

check_attest() {
  if [ "$NO_ATTEST" = 1 ]; then
    return 0
  fi
  if ! command -v gh >/dev/null 2>&1; then
    warn_attest "gh is not installed, so the build-provenance attestation was not checked. The SHA-256 matched."
    return 0
  fi
  if ! gh auth status >/dev/null 2>&1; then
    warn_attest "gh is not logged in, so the build-provenance attestation was not checked. The SHA-256 matched."
    return 0
  fi
  if ! gh attestation verify --help >/dev/null 2>&1; then
    warn_attest "this gh has no attestation command, so the build-provenance attestation was not checked. The SHA-256 matched."
    return 0
  fi
  err=$(mktemp)
  if gh attestation verify "$1" --repo "$REPO" >"$err" 2>&1; then
    rm -f "$err"
    return 0
  fi
  text=$(cat "$err" 2>/dev/null || true)
  rm -f "$err"
  if attest_rejected "$text"; then
    echo "install.sh: gh attestation verify rejected this binary. Not installing." >&2
    printf '%s\n' "$text" >&2
    exit 1
  fi
  warn_attest "gh attestation verify could not run, so the build-provenance attestation was not checked. The SHA-256 matched."
}

check_attest "$tmp"

mkdir -p "$PREFIX" || {
  echo "install.sh: cannot create $PREFIX (no sudo; pick a directory you own with --prefix)" >&2
  exit 1
}
# A directory we do not own is a mistake, not a prompt for a password.
if [ ! -w "$PREFIX" ]; then
  echo "install.sh: $PREFIX is not writable. Pass --prefix, or create it as your user. No sudo." >&2
  exit 1
fi

install_into() {
  rm -f "$PREFIX/bwn" "$PREFIX/buildwithnexus"
  cp "$tmp" "$PREFIX/bwn"
  chmod 755 "$PREFIX/bwn"
  ln -sfn bwn "$PREFIX/buildwithnexus"
}

install_into
echo "installed $PREFIX/bwn"
case ":$PATH:" in
  *":$PREFIX:"*) echo "bwn is already on PATH" ;;
  *)
    echo "Open a new terminal, or add this to your shell profile:"
    echo "  export PATH=\"$PREFIX:\$PATH\""
    ;;
esac
"$PREFIX/bwn" version
