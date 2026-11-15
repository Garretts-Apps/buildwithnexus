'use strict';
// Fill Formula/buildwithnexus.rb, the Scoop manifest, and the winget
// manifests from package.json plus the release .sha256 files in --dist.
// `node scripts/update-install-manifests.js --dist dist` writes them.
// `--check` exits 1 when the formula version disagrees with package.json.
const fs = require('fs');
const path = require('path');

const REPO = 'Garretts-Apps/buildwithnexus';
const ROOT = path.join(__dirname, '..');
const ASSETS = [
  ['macos', 'arm', 'buildwithnexus-aarch64-apple-darwin'],
  ['macos', 'intel', 'buildwithnexus-x86_64-apple-darwin'],
  ['linux', 'arm', 'buildwithnexus-aarch64-unknown-linux-gnu'],
  ['linux', 'intel', 'buildwithnexus-x86_64-unknown-linux-gnu'],
];
const WINDOWS = 'buildwithnexus-x86_64-pc-windows-msvc.exe';

function parseArgs(argv) {
  const out = { dist: '', check: false };
  for (let i = 2; i < argv.length; i++) {
    if (argv[i] === '--dist') out.dist = argv[++i];
    else if (argv[i] === '--check') out.check = true;
    else throw new Error('unknown argument ' + argv[i]);
  }
  return out;
}

function version() {
  return require(path.join(ROOT, 'package.json')).version;
}

function readHash(dist, name) {
  const file = path.join(dist, name + '.sha256');
  const line = fs.readFileSync(file, 'utf8').trim().split(/\s+/)[0];
  if (!/^[0-9a-f]{64}$/i.test(line)) throw new Error('bad checksum in ' + file);
  return line.toLowerCase();
}

function url(ver, name) {
  return `https://github.com/${REPO}/releases/download/v${ver}/${name}`;
}

function formula(ver, hashes) {
  const block = (os, cpu, name) => `    on_${cpu} do
      url "${url(ver, name)}"
      sha256 "${hashes[name]}"
    end`;
  return `class Buildwithnexus < Formula
  desc "Agentic AI coding CLI"
  homepage "https://buildwithnexus.dev"
  version "${ver}"
  license "MIT"

  on_macos do
${block('macos', 'arm', 'buildwithnexus-aarch64-apple-darwin')}
${block('macos', 'intel', 'buildwithnexus-x86_64-apple-darwin')}
  end

  on_linux do
${block('linux', 'arm', 'buildwithnexus-aarch64-unknown-linux-gnu')}
${block('linux', 'intel', 'buildwithnexus-x86_64-unknown-linux-gnu')}
  end

  def install
    if OS.mac?
      src = Hardware::CPU.arm? ? "buildwithnexus-aarch64-apple-darwin" : "buildwithnexus-x86_64-apple-darwin"
    else
      src = Hardware::CPU.arm? ? "buildwithnexus-aarch64-unknown-linux-gnu" : "buildwithnexus-x86_64-unknown-linux-gnu"
    end
    bin.install src => "bwn"
    chmod 0755, bin/"bwn"
    bin.install_symlink "bwn" => "buildwithnexus"
  end

  test do
    assert_match "buildwithnexus #{version}", shell_output("#{bin}/bwn version")
  end
end
`;
}

function scoop(ver, hash) {
  return JSON.stringify({
    version: ver,
    description: 'Agentic AI coding CLI',
    homepage: 'https://buildwithnexus.dev',
    license: 'MIT',
    architecture: {
      '64bit': { url: url(ver, WINDOWS), hash },
    },
    bin: [
      ['buildwithnexus.exe', 'bwn'],
      'buildwithnexus.exe',
    ],
  }, null, 2) + '\n';
}

function winget(ver, hash) {
  const id = 'Garretts-Apps.buildwithnexus';
  const versionYaml = `PackageIdentifier: ${id}
PackageVersion: ${ver}
DefaultLocale: en-US
ManifestType: version
ManifestVersion: 1.6.0
`;
  const installer = `PackageIdentifier: ${id}
PackageVersion: ${ver}
Installers:
  - Architecture: x64
    InstallerType: portable
    InstallerUrl: ${url(ver, WINDOWS)}
    InstallerSha256: ${hash}
    Commands:
      - bwn
      - buildwithnexus
ManifestType: installer
ManifestVersion: 1.6.0
`;
  const locale = `PackageIdentifier: ${id}
PackageVersion: ${ver}
PackageLocale: en-US
Publisher: Garretts-Apps
PackageName: buildwithnexus
License: MIT
ShortDescription: Agentic AI coding CLI
ManifestType: defaultLocale
ManifestVersion: 1.6.0
`;
  return { versionYaml, installer, locale };
}

function writeAll(dist) {
  const ver = version();
  const hashes = {};
  for (const [, , name] of ASSETS) hashes[name] = readHash(dist, name);
  const win = readHash(dist, WINDOWS);
  const formulaPath = path.join(ROOT, 'Formula', 'buildwithnexus.rb');
  fs.mkdirSync(path.dirname(formulaPath), { recursive: true });
  fs.writeFileSync(formulaPath, formula(ver, hashes));
  const scoopDir = path.join(ROOT, 'packaging', 'scoop');
  fs.mkdirSync(scoopDir, { recursive: true });
  fs.writeFileSync(path.join(scoopDir, 'buildwithnexus.json'), scoop(ver, win));
  const wingetDir = path.join(ROOT, 'packaging', 'winget');
  fs.mkdirSync(wingetDir, { recursive: true });
  const w = winget(ver, win);
  fs.writeFileSync(path.join(wingetDir, 'Garretts-Apps.buildwithnexus.yaml'), w.versionYaml);
  fs.writeFileSync(path.join(wingetDir, 'Garretts-Apps.buildwithnexus.installer.yaml'), w.installer);
  fs.writeFileSync(path.join(wingetDir, 'Garretts-Apps.buildwithnexus.locale.en-US.yaml'), w.locale);
  return ver;
}

function check() {
  const ver = version();
  const text = fs.readFileSync(path.join(ROOT, 'Formula', 'buildwithnexus.rb'), 'utf8');
  if (!text.includes(`version "${ver}"`)) {
    throw new Error(`Formula/buildwithnexus.rb is not version ${ver}`);
  }
  const scoopText = fs.readFileSync(path.join(ROOT, 'packaging', 'scoop', 'buildwithnexus.json'), 'utf8');
  if (!scoopText.includes(`"version": "${ver}"`)) {
    throw new Error(`Scoop manifest is not version ${ver}`);
  }
}

function main() {
  const args = parseArgs(process.argv);
  if (args.check) {
    check();
    return;
  }
  if (!args.dist) throw new Error('--dist is required');
  const ver = writeAll(args.dist);
  process.stdout.write(`updated install manifests for ${ver}\n`);
}

if (require.main === module) {
  try { main(); } catch (e) {
    process.stderr.write(String(e.message || e) + '\n');
    process.exit(1);
  }
}

module.exports = { formula, scoop, winget, writeAll, url };
