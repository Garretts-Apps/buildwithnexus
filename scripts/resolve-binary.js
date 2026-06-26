'use strict';
// Shared between the launcher and the installer: where the native binary lives
// and what release asset matches this platform. Node builtins only — no deps.
const path = require('path');
const fs = require('fs');

const ROOT = path.join(__dirname, '..');

function ext() {
  return process.platform === 'win32' ? '.exe' : '';
}

// Rust target triple for the current platform, or null if unsupported.
function target() {
  const arch = { arm64: 'aarch64', x64: 'x86_64' }[process.arch] || process.arch;
  switch (process.platform) {
    case 'darwin': return `${arch}-apple-darwin`;
    case 'win32': return `${arch}-pc-windows-msvc`;
    case 'linux': return `${arch}-unknown-linux-gnu`;
    default: return null;
  }
}

function installedBinary() {
  return path.join(ROOT, 'bin', 'buildwithnexus' + ext());
}

function devBinary() {
  return path.join(ROOT, 'harness', 'target', 'release', 'buildwithnexus' + ext());
}

// First existing binary: explicit override, packaged, then a local dev build.
function existing() {
  const candidates = [process.env.BWN_BIN, installedBinary(), devBinary()].filter(Boolean);
  return candidates.find((p) => fs.existsSync(p)) || null;
}

module.exports = { ROOT, ext, target, installedBinary, devBinary, existing };
