'use strict';
// Preloaded through NODE_OPTIONS=--require: stands in for the GitHub Release
// so bootstrap.js can be tested with no network.
//   FAKE_ASSET       file served for every release asset (the tests put its
//                    hash in checksums.json); .sha256 requests get a 404
//   FAKE_NET_ERROR   fail every request with this error code instead
//   FAKE_HANG        send requests to a local server that never answers, with
//                    the launcher's 30 s timeouts cut to this many ms
//   FAKE_QUARANTINE  delete the download the way endpoint protection does:
//                    on-close (as soon as it is written), before-rename, or
//                    after-rename (the installed binary)
//   FAKE_REQUESTS    append each requested URL to this file
const EventEmitter = require('events');
const fs = require('fs');
const https = require('https');
const net = require('net');
const { Readable } = require('stream');

const env = process.env;
const realGet = https.get;
let hangPort;

// A local server that accepts connections and never says anything.
function silentServer() {
  hangPort = hangPort || new Promise((resolve) => {
    const server = net.createServer(() => {});
    server.unref();
    server.listen(0, '127.0.0.1', () => resolve(server.address().port));
  });
  return hangPort;
}

https.get = (url, options, cb) => {
  if (env.FAKE_REQUESTS) fs.appendFileSync(env.FAKE_REQUESTS, `${url}\n`);
  if (env.FAKE_HANG) {
    // A real request, so the launcher's own timeout handling is what runs.
    const req = new EventEmitter();
    let real;
    req.destroy = (e) => (real ? real.destroy(e) : req.emit('error', e));
    silentServer().then((port) => {
      real = realGet(`https://127.0.0.1:${port}${new URL(url).pathname}`, options, cb);
      for (const ev of ['timeout', 'error']) real.on(ev, (...a) => req.emit(ev, ...a));
    });
    return req;
  }
  const req = new EventEmitter();
  req.destroy = (e) => e && req.emit('error', e);
  req.setTimeout = () => req;
  process.nextTick(() => {
    if (env.FAKE_NET_ERROR) {
      const e = new Error(`connect ${env.FAKE_NET_ERROR} 140.82.112.3:443`);
      return req.emit('error', Object.assign(e, { code: env.FAKE_NET_ERROR }));
    }
    const found = env.FAKE_ASSET && !String(url).endsWith('.sha256');
    const res = found ? fs.createReadStream(env.FAKE_ASSET) : Readable.from([]);
    cb(Object.assign(res, { statusCode: found ? 200 : 404, headers: {} }));
  });
  return req;
};

if (env.FAKE_HANG) {
  // Only the launcher's own 30 s timers; a missing one still hangs the test.
  const realSetTimeout = global.setTimeout;
  global.setTimeout = (fn, ms, ...rest) => realSetTimeout(fn, ms === 30000 ? Number(env.FAKE_HANG) : ms, ...rest);
}

if (env.FAKE_QUARANTINE === 'on-close') {
  const realCreate = fs.createWriteStream;
  fs.createWriteStream = (p, ...rest) => {
    const ws = realCreate(p, ...rest);
    if (String(p).endsWith('.download')) ws.once('close', () => fs.unlinkSync(p));
    return ws;
  };
} else if (env.FAKE_QUARANTINE) {
  const realRename = fs.renameSync;
  fs.renameSync = (from, to) => {
    if (env.FAKE_QUARANTINE === 'before-rename') fs.unlinkSync(from);
    realRename(from, to);
    if (env.FAKE_QUARANTINE === 'after-rename') fs.unlinkSync(to);
  };
}
