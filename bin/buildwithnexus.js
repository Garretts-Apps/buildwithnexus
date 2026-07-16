#!/usr/bin/env node
'use strict';
// Thin launcher: resolve the platform binary and hand off with stdio inherited
// so the alternate-screen TUI works exactly as if it were invoked directly.
// Deliberately boring — no install scripts, no network, no dynamic code. The
// binary itself handles update checks.
const path = require('path');
const { spawnSync } = require('child_process');
const { existing, platformPackage, target } = require('../scripts/resolve-binary.js');

// Blocking y/N prompt on the controlling TTY — the launcher is synchronous by
// design, so a readline event loop would be more machinery than the question.
// Node keeps a TTY stdin (fd 0) in non-blocking mode, so readSync(0) throws
// EAGAIN on macOS instead of waiting — a freshly opened /dev/tty fd blocks
// like the question needs. Windows has no /dev/tty; there fd 0 does block.
function askConsent() {
  const fs = require('fs');
  process.stderr.write(
    `buildwithnexus: no prebuilt binary is installed for ${process.platform} ${process.arch}.\n` +
    '  Download the checksum-verified binary from the GitHub release now? [y/N] '
  );
  let fd = 0;
  try {
    fd = fs.openSync('/dev/tty', 'r');
  } catch {}
  try {
    const buf = Buffer.alloc(64);
    const n = fs.readSync(fd, buf, 0, 64, null);
    return /^y(es)?$/i.test(buf.toString('utf8', 0, n).trim());
  } catch {
    return false;
  } finally {
    if (fd !== 0) fs.closeSync(fd);
  }
}

let args = process.argv.slice(2);
let bin = existing();
if (!bin) {
  // The platform optionalDependency is absent (--omit=optional, or the
  // platform package isn't published yet). The checksum-verified download
  // from the GitHub release requires explicit consent: the --bootstrap flag,
  // BWN_ALLOW_BOOTSTRAP=1, or a y at the interactive prompt. Never during
  // npm install, never silently.
  const flagIdx = args.indexOf('--bootstrap');
  if (flagIdx !== -1) args = args.filter((a) => a !== '--bootstrap');
  const consented =
    flagIdx !== -1 ||
    process.env.BWN_ALLOW_BOOTSTRAP === '1' ||
    (process.stdin.isTTY && process.stderr.isTTY && askConsent());
  if (consented) {
    spawnSync(process.execPath, [path.join(__dirname, '..', 'scripts', 'bootstrap.js')], {
      stdio: 'inherit',
    });
    bin = existing();
  }
}
if (!bin) {
  const pkg = platformPackage();
  const t = target();
  process.stderr.write(
    'buildwithnexus: native binary not found.\n' +
    (pkg
      ? `  The platform package "${pkg}" is missing — it installs as an optionalDependency,\n` +
        '  so re-install without --omit=optional / --no-optional:\n' +
        '    npm install -g buildwithnexus\n' +
        '  Or download the checksum-verified release binary explicitly:\n' +
        '    bwn --bootstrap        (or BWN_ALLOW_BOOTSTRAP=1 bwn)\n'
      : `  No prebuilt binary exists for this platform (${process.platform} ${process.arch}).\n`) +
    (t
      ? '  Or build from source and point BWN_BIN at the result:\n' +
        '    git clone https://github.com/Garretts-Apps/buildwithnexus\n' +
        '    cargo build --release --manifest-path buildwithnexus/harness/Cargo.toml\n'
      : '') +
    '  Docs: https://buildwithnexus.dev/docs/install\n'
  );
  process.exit(1);
}

const result = spawnSync(bin, args, { stdio: 'inherit' });
if (result.error) {
  process.stderr.write(`buildwithnexus: ${result.error.message}\n`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
