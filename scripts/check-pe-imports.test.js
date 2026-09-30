'use strict';

const assert = require('node:assert/strict');
const { spawnSync } = require('node:child_process');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const test = require('node:test');

const { peImports, vcRuntimeImports } = require('./check-pe-imports.js');

const SCRIPT = path.join(__dirname, 'check-pe-imports.js');

// Imports of the 0.14.10 x86_64-pc-windows-msvc release exe (as objdump -p
// lists them), which linked the C runtime dynamically.
const DYNAMIC_CRT = ['kernel32.dll', 'api-ms-win-core-synch-l1-2-0.dll', 'bcryptprimitives.dll', 'KERNEL32.dll',
  'bcrypt.dll', 'ADVAPI32.dll', 'USER32.dll', 'ntdll.dll', 'WS2_32.dll', 'VCRUNTIME140.dll',
  'api-ms-win-crt-math-l1-1-0.dll', 'api-ms-win-crt-runtime-l1-1-0.dll', 'api-ms-win-crt-stdio-l1-1-0.dll',
  'api-ms-win-crt-locale-l1-1-0.dll', 'api-ms-win-crt-heap-l1-1-0.dll'];

// A minimal PE image: headers, then one section holding the import
// descriptors (and delay-load descriptors) and the DLL name strings.
function fakePe({ imports = [], delayed = [], pe32 = false } = {}) {
  const peAt = 0x80;
  const optSize = pe32 ? 224 : 240;
  const table = peAt + 24 + optSize;
  const raw = 0x200;
  const va = 0x1000;
  const body = Buffer.alloc(0x400);
  let at = (imports.length + 1) * 20 + (delayed.length + 1) * 32;
  const name = (s) => {
    const rva = va + at;
    body.write(s + '\0', at, 'latin1');
    at += s.length + 1;
    return rva;
  };
  imports.forEach((dll, i) => body.writeUInt32LE(name(dll), i * 20 + 12));
  const delayAt = (imports.length + 1) * 20;
  delayed.forEach((dll, i) => {
    body.writeUInt32LE(1, delayAt + i * 32);
    body.writeUInt32LE(name(dll), delayAt + i * 32 + 4);
  });

  const head = Buffer.alloc(raw);
  head.write('MZ', 0, 'latin1');
  head.writeUInt32LE(peAt, 0x3c);
  head.write('PE\0\0', peAt, 'latin1');
  head.writeUInt16LE(pe32 ? 0x14c : 0x8664, peAt + 4);
  head.writeUInt16LE(1, peAt + 6);
  head.writeUInt16LE(optSize, peAt + 20);
  const opt = peAt + 24;
  head.writeUInt16LE(pe32 ? 0x10b : 0x20b, opt);
  const dirs = opt + (pe32 ? 96 : 112);
  head.writeUInt32LE(16, dirs - 4);
  if (imports.length) head.writeUInt32LE(va, dirs + 8);
  if (delayed.length) head.writeUInt32LE(va + delayAt, dirs + 13 * 8);
  head.write('.idata', table, 'latin1');
  head.writeUInt32LE(body.length, table + 8);
  head.writeUInt32LE(va, table + 12);
  head.writeUInt32LE(body.length, table + 16);
  head.writeUInt32LE(raw, table + 20);
  return Buffer.concat([head, body]);
}

test('reads the imported DLLs of a PE32+ image, including delay-loaded ones', () => {
  const buf = fakePe({ imports: ['KERNEL32.dll', 'VCRUNTIME140.dll'], delayed: ['USER32.dll'] });
  assert.deepEqual(peImports(buf), ['KERNEL32.dll', 'VCRUNTIME140.dll', 'USER32.dll']);
});

test('reads a PE32 image too', () => {
  assert.deepEqual(peImports(fakePe({ imports: ['KERNEL32.dll'], pe32: true })), ['KERNEL32.dll']);
});

test('refuses a file that is not a PE image', () => {
  assert.throws(() => peImports(Buffer.from('\x7fELF'.padEnd(0x100, '\0'))), /not a PE image/);
});

test('flags the Visual C++ runtime, not Windows or the Universal CRT', () => {
  assert.deepEqual(vcRuntimeImports(DYNAMIC_CRT), ['VCRUNTIME140.dll']);
  assert.deepEqual(vcRuntimeImports(['vcruntime140_1.dll', 'MSVCP140.dll', 'ucrtbase.dll', 'msvcrt.dll']),
    ['vcruntime140_1.dll', 'MSVCP140.dll']);
  assert.deepEqual(vcRuntimeImports(DYNAMIC_CRT.filter((n) => !/vcruntime/i.test(n))), []);
});

test('the command fails on an exe that imports VCRUNTIME140.dll and passes one that does not', (t) => {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-pe-'));
  t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
  const run = (imports) => {
    const exe = path.join(dir, 'b.exe');
    fs.writeFileSync(exe, fakePe({ imports }));
    return spawnSync(process.execPath, [SCRIPT, exe], { encoding: 'utf8' });
  };
  const bad = run(DYNAMIC_CRT);
  assert.equal(bad.status, 1, bad.stderr);
  assert.match(bad.stdout, /::error::.*imports VCRUNTIME140\.dll from the Visual C\+\+ Redistributable/);
  const good = run(DYNAMIC_CRT.filter((n) => !/vcruntime/i.test(n)));
  assert.equal(good.status, 0, good.stdout + good.stderr);
  assert.match(good.stdout, /imports: kernel32\.dll, api-ms-win-core-synch-l1-2-0\.dll/);
});
