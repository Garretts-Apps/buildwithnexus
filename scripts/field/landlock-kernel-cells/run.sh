#!/bin/sh
# Boot the old-kernel Landlock conformance cells (see README.md).
set -eu
here=$(CDPATH= cd "$(dirname "$0")" && pwd)
repo=$(CDPATH= cd "$here/../../.." && pwd)
work=${WORK:-$(mktemp -d)}
mkdir -p "$work"
echo "work dir: $work"

need() { command -v "$1" >/dev/null 2>&1 || { echo "missing: $1" >&2; exit 2; }; }
need qemu-system-x86_64
need qemu-img
need cloud-localds
mkiso() { if command -v genisoimage >/dev/null 2>&1; then genisoimage "$@"; else xorriso -as mkisofs "$@"; fi; }

# 1. Build the static test binary and the bwn binary the cells exec. Run the
# cargo commands from the workspace root, where `cargo test --no-run` names
# the test binary with a path relative to the repo (it prints this line on
# every run, cached or not, as long as it is not quiet).
echo "building static musl test binary..."
target=x86_64-unknown-linux-musl
buildlog="$work/cargo-test-norun.log"
( cd "$repo"
  cargo build -q -p buildwithnexus --bin buildwithnexus --target "$target"
  cargo test -p buildwithnexus --lib --target "$target" --no-run ) >"$buildlog" 2>&1 \
  || { cat "$buildlog" >&2; echo "build failed" >&2; exit 2; }
# The "Executable unittests ..." line ends in the binary path in parens,
# absolute or repo-relative (build-script output under target/.../out/).
testbin=$(grep -oE '\([^)]*buildwithnexus-[0-9a-f]+\)' "$buildlog" | tr -d '()' | head -1 || true)
case "${testbin:-}" in
  /*) ;;
  target/*) testbin="$repo/$testbin" ;;
  *) testbin= ;;
esac
[ -n "${testbin:-}" ] && [ -x "$testbin" ] || { cat "$buildlog" >&2; echo "could not find the test binary" >&2; exit 2; }
cp "$testbin" "$here/kit/bwn-tests"
cp "$repo/target/$target/debug/buildwithnexus" "$here/kit/bwn"

# 2. One kit ISO (same for both cells) and a cloud-init seed.
mkiso -quiet -V BWNKIT -r -J -o "$work/kit.iso" "$here/kit"
cloud-localds "$work/seed.iso" "$here/user-data" "$here/meta-data"

# 3. Fetch + verify each base image, boot an ephemeral overlay, collect serial.
rc=0
while IFS='|' read -r cell url sha; do
  case "$cell" in ''|\#*) continue ;; esac
  img="$work/$cell.base"
  if [ ! -f "$img" ] || [ "$(sha256sum "$img" | cut -d' ' -f1)" != "$sha" ]; then
    echo "fetching $cell..."
    curl -fSL "$url" -o "$img"
    got=$(sha256sum "$img" | cut -d' ' -f1)
    if [ "$got" != "$sha" ]; then
      echo "sha256 mismatch for $cell: upstream rotated the image." >&2
      echo "  expected $sha" >&2
      echo "  got      $got" >&2
      echo "verify the new image and update images.txt." >&2
      exit 3
    fi
  fi
  overlay="$work/$cell-run.qcow2"
  qemu-img create -q -f qcow2 -b "$img" -F qcow2 "$overlay" 20G
  serial="$work/$cell-serial.log"
  : > "$serial"
  accel=tcg
  [ -w /dev/kvm ] && accel=kvm
  echo "booting $cell ($accel)..."
  timeout 3000 qemu-system-x86_64 -machine q35 -accel "$accel" -cpu max -smp 2 -m 3072 \
    -nographic -drive file="$overlay",if=virtio \
    -drive file="$work/seed.iso",if=virtio,format=raw,readonly=on \
    -drive file="$work/kit.iso",if=virtio,format=raw,readonly=on \
    -netdev user,id=n0 -device virtio-net-pci,netdev=n0 \
    -serial file:"$serial" -monitor none -display none >"$work/$cell-qemu.log" 2>&1 || true
  echo "##### $cell"
  sed -n '/PROBE START/,/PROBE END/p' "$serial" || true
  if ! grep -q "PROBE END" "$serial"; then echo "$cell: did not reach PROBE END" >&2; rc=1; fi
  if grep -q "panicked" "$serial"; then echo "$cell: a test panicked" >&2; rc=1; fi
  if grep -q "EMPTY FILTER" "$serial"; then echo "$cell: a test filter matched nothing" >&2; rc=1; fi
done < "$here/images.txt"

exit $rc
