#!/usr/bin/env node
'use strict';
// Thin launcher: hand off to the native Rust binary with stdio inherited so the
// alternate-screen TUI works exactly as if it were invoked directly.
//
// Auto-update: at most once a day a detached background process checks npm and
// silently installs a newer version (opt out: BWN_NO_AUTO_UPDATE=1). The only
// launcher-side cost is one small JSON read + at most one fork — startup never
// waits on the network.
const { spawn, spawnSync } = require('child_process');
const path = require('path');
const { existing } = require('../scripts/resolve-binary.js');

function maybeAutoUpdate() {
  if (process.env.BWN_NO_AUTO_UPDATE === '1') return;
  try {
    const { readState, writeState } = require('../scripts/auto-update.js');
    const state = readState();
    // One-line notice for an update that landed in the background.
    if (state.updatedTo && state.updatedTo !== state.noticeShownFor) {
      process.stderr.write(`\x1b[2mbuildwithnexus updated to v${state.updatedTo} — restart to use it\x1b[0m\n`);
      writeState({ noticeShownFor: state.updatedTo });
    }
    const DAY = 24 * 60 * 60 * 1000;
    if (Date.now() - (state.lastCheck || 0) < DAY) return;
    const child = spawn(
      process.execPath,
      [path.join(__dirname, '..', 'scripts', 'auto-update.js')],
      { detached: true, stdio: 'ignore', windowsHide: true }
    );
    child.unref();
  } catch { /* the updater must never break the launcher */ }
}

maybeAutoUpdate();

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
