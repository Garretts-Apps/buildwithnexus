#!/usr/bin/env node
'use strict';
// Thin launcher: resolve the platform binary and hand off with stdio inherited
// so the alternate-screen TUI works exactly as if it were invoked directly.
// Deliberately boring — no install scripts, no network, no dynamic code. The
// binary itself handles update checks.
const path = require('path');
const { spawnSync } = require('child_process');
const { existing, platformPackage, target } = require('../scripts/resolve-binary.js');

let bin = existing();
if (!bin) {
  // Fallback: the platform optionalDependency is absent (--omit=optional, or
  // the platform package isn't published yet). Fetch the checksum-verified
  // prebuilt from the GitHub release — visibly, on first run, never during
  // npm install.
  spawnSync(process.execPath, [path.join(__dirname, '..', 'scripts', 'bootstrap.js')], {
    stdio: 'inherit',
  });
  bin = existing();
}
if (!bin) {
  const pkg = platformPackage();
  const t = target();
  process.stderr.write(
    'buildwithnexus: native binary not found.\n' +
    (pkg
      ? `  The platform package "${pkg}" is missing — it installs as an optionalDependency,\n` +
        '  so re-install without --omit=optional / --no-optional:\n' +
        '    npm install -g buildwithnexus\n'
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

const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  process.stderr.write(`buildwithnexus: ${result.error.message}\n`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
