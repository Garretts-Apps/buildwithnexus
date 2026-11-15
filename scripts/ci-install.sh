#!/bin/sh
# Install the binary this job built, run it, then uninstall it.
set -eu
bin=${1:?binary}
prefix=${2:-${RUNNER_TEMP:-/tmp}/bwn-prefix}
if command -v sha256sum >/dev/null 2>&1; then
  hash=$(sha256sum "$bin" | awk '{print $1}')
else
  hash=$(shasum -a 256 "$bin" | awk '{print $1}')
fi
rm -rf "$prefix"
sh scripts/install.sh --file "$bin" --sha256 "$hash" --prefix "$prefix" --no-attest
out=$("$prefix/bwn" version)
printf '%s\n' "$out"
printf '%s' "$out" | grep -q 'buildwithnexus'
sh scripts/install.sh --uninstall --prefix "$prefix"
test ! -e "$prefix/bwn"
echo "installer ok"
