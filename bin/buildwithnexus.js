#!/usr/bin/env node
'use strict';
// Thin launcher: hand off to the native Rust binary with stdio inherited so the
// alternate-screen TUI works exactly as if it were invoked directly.
const { spawnSync } = require('child_process');
const { existing } = require('../scripts/resolve-binary.js');

const bin = existing();
if (!bin) {
  process.stderr.write(
    'buildwithnexus: native binary not found.\n' +
    '  Reinstall the package, or build it locally with Rust (https://rustup.rs):\n' +
    '    npm explore buildwithnexus -- npm run build\n'
  );
  process.exit(1);
}

const result = spawnSync(bin, process.argv.slice(2), { stdio: 'inherit' });
if (result.error) {
  process.stderr.write(`buildwithnexus: ${result.error.message}\n`);
  process.exit(1);
}
process.exit(result.status === null ? 1 : result.status);
