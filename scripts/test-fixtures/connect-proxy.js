'use strict';
// A local HTTP CONNECT proxy and a local stand-in for GitHub's TLS hosts, so
// the launcher's own proxy tunnel can be tested end to end with no network.
//
// startProxy({ upstream, user, password, status, hang })
//   Every CONNECT is logged ({ target, auth, userAgent }) in `proxy.log`.
//   `upstream` is the [host, port] each tunnel really goes to (the fake
//   GitHub below); `user`/`password` make it answer 407 to anything else;
//   `status` answers every CONNECT with that status; `hang` never answers.
//
// startFakeGitHub({ asset, redirectTo })
//   One TLS server for github.com and objects.githubusercontent.com, with a
//   throwaway self-signed certificate made by openssl (null when openssl is
//   missing). The launcher trusts it only through NODE_EXTRA_CA_CERTS.
//   Release downloads redirect to objects.githubusercontent.com (or to
//   `redirectTo`), which serves `asset`; .sha256 requests get a 404.
const fs = require('fs');
const http = require('http');
const https = require('https');
const net = require('net');
const os = require('os');
const path = require('path');
const { spawnSync } = require('child_process');

function listen(server) {
  return new Promise((resolve) => server.listen(0, '127.0.0.1', () => resolve(server.address().port)));
}

async function startProxy({ upstream, user, password, status, hang } = {}) {
  const log = [];
  const sockets = new Set();
  const server = http.createServer((req, res) => {
    res.writeHead(405).end();
  });
  server.on('connection', (s) => {
    sockets.add(s);
    s.on('close', () => sockets.delete(s));
  });
  server.on('connect', (req, client, head) => {
    const auth = req.headers['proxy-authorization'] || null;
    log.push({ target: req.url, auth, userAgent: req.headers['user-agent'] || null });
    if (hang) return;
    const expected = user !== undefined && `Basic ${Buffer.from(`${user}:${password}`).toString('base64')}`;
    const answer = status || (expected && auth !== expected ? 407 : 200);
    if (answer !== 200) {
      const extra = answer === 407 ? 'Proxy-Authenticate: Basic realm="test"\r\n' : '';
      client.end(`HTTP/1.1 ${answer} ${http.STATUS_CODES[answer] || 'Nope'}\r\n${extra}Content-Length: 0\r\n\r\n`);
      return;
    }
    const [host, port] = upstream;
    const remote = net.connect(port, host, () => {
      client.write('HTTP/1.1 200 Connection Established\r\n\r\n');
      if (head && head.length) remote.write(head);
      remote.pipe(client);
      client.pipe(remote);
    });
    remote.on('error', () => client.destroy());
    client.on('error', () => remote.destroy());
  });
  const port = await listen(server);
  return {
    port,
    log,
    close: () => {
      for (const s of sockets) s.destroy();
      return new Promise((resolve) => server.close(resolve));
    },
  };
}

// A self-signed certificate for the GitHub hosts, or null without openssl.
function makeCert(dir) {
  const key = path.join(dir, 'key.pem');
  const cert = path.join(dir, 'cert.pem');
  const r = spawnSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert,
    '-days', '2', '-subj', '/CN=github.com',
    '-addext', 'subjectAltName=DNS:github.com,DNS:objects.githubusercontent.com'], { encoding: 'utf8' });
  if (r.status !== 0 || !fs.existsSync(cert)) return null;
  return { key: fs.readFileSync(key), cert: fs.readFileSync(cert), certFile: cert };
}

function opensslAvailable() {
  const r = spawnSync('openssl', ['version'], { encoding: 'utf8' });
  return r.status === 0;
}

async function startFakeGitHub({ asset, redirectTo } = {}) {
  const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'bwn-fake-gh-'));
  const tlsFiles = makeCert(dir);
  if (!tlsFiles) {
    fs.rmSync(dir, { recursive: true, force: true });
    return null;
  }
  const requests = [];
  const server = https.createServer({ key: tlsFiles.key, cert: tlsFiles.cert }, (req, res) => {
    const host = req.headers.host;
    requests.push(`${host}${req.url}`);
    if (req.url.endsWith('.sha256')) return res.writeHead(404).end();
    if (host === 'github.com' && req.url.includes('/releases/download/')) {
      const name = req.url.split('/').pop();
      const location = redirectTo || `https://objects.githubusercontent.com/release-asset/${name}`;
      return res.writeHead(302, { location }).end();
    }
    if (host === 'objects.githubusercontent.com' && asset) {
      res.writeHead(200, { 'content-type': 'application/octet-stream' });
      return fs.createReadStream(asset).pipe(res);
    }
    res.writeHead(404).end();
  });
  const sockets = new Set();
  server.on('connection', (s) => {
    sockets.add(s);
    s.on('close', () => sockets.delete(s));
  });
  const port = await listen(server);
  return {
    port,
    requests,
    certFile: tlsFiles.certFile,
    close: () => {
      for (const s of sockets) s.destroy();
      return new Promise((resolve) => server.close(() => {
        fs.rmSync(dir, { recursive: true, force: true });
        resolve();
      }));
    },
  };
}

// A port nothing listens on.
async function closedPort() {
  const server = net.createServer();
  const port = await listen(server);
  await new Promise((resolve) => server.close(resolve));
  return port;
}

module.exports = { startProxy, startFakeGitHub, opensslAvailable, closedPort };
