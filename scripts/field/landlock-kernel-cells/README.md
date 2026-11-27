# Landlock old-kernel conformance cells

Landlock's filesystem rights arrived over several kernel ABIs, and bwn's
guarantees change below ABI 3 (truncation is seccomp's, not Landlock's) and
ABI 4 (no network limiting). The capability table in
`harness/src/capabilities.rs` says "Confirmed on 5.15 and 6.1"; this harness
is what confirms it. It boots two cloud images in QEMU and runs the Landlock
and `invariant_m2` / `invariant_m5` / `invariant_m8c` tests inside each:

- Ubuntu 22.04 — kernel 5.15, Landlock ABI 1
- Debian 12 — kernel 6.1, Landlock ABI 2

It is a developer tool, not part of CI (a TCG run is ~10 minutes per cell and
needs no network once the images are cached). Nested KVM makes it fast where
`/dev/kvm` exists.

## Run

```sh
scripts/field/landlock-kernel-cells/run.sh
```

Needs `qemu-system-x86_64`, `genisoimage` (or `xorriso`), `cloud-localds`
(from `cloud-image-utils`), and the `x86_64-unknown-linux-musl` Rust target
(`rustup target add x86_64-unknown-linux-musl`). It builds a static test
binary, so the cells need no toolchain. Images are cached under `$WORK`
(default: a temp dir; set `WORK=...` to keep them). Each image is verified
against the sha256 in `images.txt`; if upstream has rotated the "release"
build, the run stops and prints the new digest to record.

A cell passes when its serial log shows every `test ... ok` and no
`panicked`. The run prints the `=== PROBE START/END` section per cell and
exits non-zero if either cell did not reach `PROBE END`.
