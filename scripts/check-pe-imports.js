'use strict';
// Fails when a Windows exe imports a Visual C++ runtime DLL (VCRUNTIME140.dll,
// MSVCP140.dll, ...), which a Windows without the VC++ Redistributable lacks.
// The release exe links the C runtime statically (.cargo/config.toml), and
// release.yml runs this on it. Reads the PE import tables itself, so it needs
// neither dumpbin nor Windows. Node builtins only.
// Usage: node scripts/check-pe-imports.js <exe>
const fs = require('fs');

// DLLs that come only with the Visual C++ Redistributable. The Universal CRT
// (ucrtbase.dll, api-ms-win-crt-*) ships with Windows 10 and later.
const VC_RUNTIME = /^(vcruntime|msvcp|vccorlib|concrt|vcomp)\d.*\.dll$/i;

// Names of the DLLs a PE image imports, normal and delay-loaded, in table order.
function peImports(buf) {
  const fail = (why) => { throw new Error(`not a PE image: ${why}`); };
  if (buf.length < 0x40 || buf.toString('latin1', 0, 2) !== 'MZ') fail('no MZ header');
  const pe = buf.readUInt32LE(0x3c);
  if (pe + 24 > buf.length || buf.toString('latin1', pe, pe + 4) !== 'PE\0\0') fail('no PE signature');
  const sections = buf.readUInt16LE(pe + 6);
  const optSize = buf.readUInt16LE(pe + 20);
  const opt = pe + 24;
  const magic = buf.readUInt16LE(opt);
  if (magic !== 0x10b && magic !== 0x20b) fail(`unknown optional header magic 0x${magic.toString(16)}`);
  const dirs = opt + (magic === 0x20b ? 112 : 96);
  const dirCount = buf.readUInt32LE(dirs - 4);
  const dir = (i) => (i < dirCount ? buf.readUInt32LE(dirs + i * 8) : 0);

  const table = opt + optSize;
  const offset = (rva) => {
    for (let i = 0; i < sections; i++) {
      const s = table + i * 40;
      const va = buf.readUInt32LE(s + 12);
      const size = Math.max(buf.readUInt32LE(s + 8), buf.readUInt32LE(s + 16));
      if (rva >= va && rva < va + size) return buf.readUInt32LE(s + 20) + (rva - va);
    }
    return fail(`RVA 0x${rva.toString(16)} is in no section`);
  };
  const cstring = (rva) => {
    const at = offset(rva);
    const end = buf.indexOf(0, at);
    return buf.toString('latin1', at, end === -1 ? buf.length : end);
  };

  const names = [];
  // IMAGE_IMPORT_DESCRIPTOR (20 bytes, name RVA at 12) and the delay-load
  // descriptor (32 bytes, name RVA at 4), each list ending in a zeroed entry.
  for (const [index, size, nameAt] of [[1, 20, 12], [13, 32, 4]]) {
    const rva = dir(index);
    if (!rva) continue;
    for (let at = offset(rva); at + size <= buf.length; at += size) {
      const name = buf.readUInt32LE(at + nameAt);
      if (!name) break;
      names.push(cstring(name));
    }
  }
  return names;
}

function vcRuntimeImports(names) {
  return names.filter((n) => VC_RUNTIME.test(n));
}

if (require.main === module) {
  const exe = process.argv[2];
  if (!exe) {
    process.stderr.write('usage: node scripts/check-pe-imports.js <exe>\n');
    process.exit(2);
  }
  const names = peImports(fs.readFileSync(exe));
  process.stdout.write(`${exe} imports: ${names.join(', ')}\n`);
  const bad = vcRuntimeImports(names);
  if (bad.length) {
    process.stdout.write(`::error::${exe} imports ${bad.join(', ')} from the Visual C++ Redistributable; ` +
      'it would not start on a Windows without it. Link the C runtime statically (+crt-static).\n');
    process.exit(1);
  }
}

module.exports = { peImports, vcRuntimeImports };
