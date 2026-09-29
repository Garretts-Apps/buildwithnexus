'use strict';
// Locate the native binary. No network, no shell, no install scripts — the
// binary arrives as a per-platform optionalDependency (like esbuild), and this
// module only resolves paths. Node builtins only.
const path = require('path');
const fs = require('fs');

const ROOT = path.join(__dirname, '..');

function ext() {
  return process.platform === 'win32' ? '.exe' : '';
}

// Platform package that carries the prebuilt binary for this machine.
const PLATFORM_PACKAGES = {
  'linux x64': 'buildwithnexus-linux-x64',
  'linux arm64': 'buildwithnexus-linux-arm64',
  'darwin x64': 'buildwithnexus-darwin-x64',
  'darwin arm64': 'buildwithnexus-darwin-arm64',
  'win32 x64': 'buildwithnexus-win32-x64',
};

function platformPackage() {
  return PLATFORM_PACKAGES[`${process.platform} ${process.arch}`] || null;
}

// Binary installed by the platform optionalDependency, if present. Only a
// package installed next to this one (or nested inside it) counts: Node's
// lookup also walks every parent `node_modules` and NODE_PATH, where another
// user could plant a package (`C:\node_modules\buildwithnexus-win32-x64`).
function packagedBinary() {
  const name = platformPackage();
  if (!name) return null;
  let found;
  try {
    found = require.resolve(`${name}/bin/buildwithnexus${ext()}`, { paths: [ROOT] });
  } catch {
    return null;
  }
  const allowed = [
    path.join(path.dirname(ROOT), name) + path.sep,
    path.join(ROOT, 'node_modules', name) + path.sep,
  ];
  return allowed.some((dir) => found.startsWith(dir)) ? found : null;
}

// BWN_BIN, if it is an absolute path. A relative value would resolve against
// whatever directory `bwn` is run in, so an untrusted checkout could supply it.
function overrideBinary() {
  const p = process.env.BWN_BIN;
  if (!p) return null;
  if (!path.isAbsolute(p)) {
    process.stderr.write(`buildwithnexus: ignoring BWN_BIN=${p}: it must be an absolute path.\n`);
    return null;
  }
  return p;
}

// Rust target triple for the current platform (used in docs/error messages).
function target() {
  const arch = { arm64: 'aarch64', x64: 'x86_64' }[process.arch] || process.arch;
  switch (process.platform) {
    case 'darwin': return `${arch}-apple-darwin`;
    case 'win32': return `${arch}-pc-windows-msvc`;
    case 'linux': return `${arch}-unknown-linux-gnu`;
    default: return null;
  }
}

// Legacy location used by pre-0.12.1 installs (postinstall-downloaded).
function installedBinary() {
  return path.join(ROOT, 'bin', 'buildwithnexus' + ext());
}

// Local development builds in a repo checkout.
function devBinary() {
  const rootTarget = path.join(ROOT, 'target', 'release', 'buildwithnexus' + ext());
  if (fs.existsSync(rootTarget)) return rootTarget;
  return path.join(ROOT, 'harness', 'target', 'release', 'buildwithnexus' + ext());
}

// First existing binary: explicit override, platform package, legacy, dev.
function existing() {
  const candidates = [overrideBinary(), packagedBinary(), installedBinary(), devBinary()].filter(Boolean);
  return candidates.find((p) => fs.existsSync(p)) || null;
}

module.exports = { ROOT, ext, target, platformPackage, packagedBinary, installedBinary, devBinary, existing };
