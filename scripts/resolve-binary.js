'use strict';
// Locate the native binary. No network, no shell, no install scripts — the
// binary arrives as a per-platform optionalDependency (like esbuild), and this
// module only resolves paths. Node builtins only.
const path = require('path');
const fs = require('fs');
const os = require('os');

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

// ~/.buildwithnexus, or NEXUS_HOME, as the binary resolves it. A relative
// NEXUS_HOME is ignored here: the launcher runs what it finds in this
// directory, and a relative path would resolve against the current project.
function nexusHome() {
  const h = process.env.NEXUS_HOME;
  if (h && path.isAbsolute(h)) return h;
  return path.join(process.env.HOME || process.env.USERPROFILE || os.homedir(), '.buildwithnexus');
}

function packageVersion() {
  return require(path.join(ROOT, 'package.json')).version;
}

// Where releases before 0.15 downloaded the binary: inside this package, so
// every npm update or reinstall deleted it.
function legacyBinary() {
  return path.join(ROOT, 'bin', 'buildwithnexus' + ext());
}

// Where the first run downloads the binary for this package version:
// <home>/bin/<version>/, outside the package so npm leaves it alone, and one
// directory per version so an upgrade never runs or replaces the old one.
// BWN_INSTALL_IN_PACKAGE=1 keeps the pre-0.15 location.
function installDir() {
  if (process.env.BWN_INSTALL_IN_PACKAGE === '1') return path.join(ROOT, 'bin');
  return path.join(nexusHome(), 'bin', packageVersion());
}

function installedBinary() {
  return path.join(installDir(), 'buildwithnexus' + ext());
}

// Written by bootstrap.js next to the binary after a verified download:
// { version, sha256 }. If it is here but the binary is not, something removed
// the binary after it was installed (typically endpoint protection), and
// downloading it again would only repeat that.
function installMarker() {
  return path.join(installDir(), '.installed.json');
}

// The marker for `bin`, if `bin` is a downloaded binary (either location).
function readInstallMarker(bin = installedBinary()) {
  if (bin !== installedBinary() && bin !== legacyBinary()) return null;
  try {
    const m = JSON.parse(fs.readFileSync(path.join(path.dirname(bin), '.installed.json'), 'utf8'));
    return m && typeof m.version === 'string' ? m : null;
  } catch {
    return null;
  }
}

// bootstrap.js exits with this after it printed why the binary it installed
// cannot run, so the launcher does not add a second message.
const EXIT_EXPLAINED = 3;

// Local development builds in a repo checkout.
function devBinary() {
  const rootTarget = path.join(ROOT, 'target', 'release', 'buildwithnexus' + ext());
  if (fs.existsSync(rootTarget)) return rootTarget;
  return path.join(ROOT, 'harness', 'target', 'release', 'buildwithnexus' + ext());
}

// First existing binary: explicit override, platform package, downloaded,
// downloaded by an earlier release, dev.
function existing() {
  const candidates = [overrideBinary(), packagedBinary(), installedBinary(), legacyBinary(), devBinary()]
    .filter(Boolean);
  return candidates.find((p) => fs.existsSync(p)) || null;
}

module.exports = {
  ROOT, ext, target, platformPackage, packagedBinary, nexusHome, installDir, installedBinary, legacyBinary,
  installMarker, readInstallMarker, devBinary, existing, EXIT_EXPLAINED,
};
