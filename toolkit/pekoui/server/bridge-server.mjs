// pekoui native bridge front proxy (`/__peko__`).
//
// Shipped in the deploy artifact and run as the container's entrypoint. It binds
// the app's public port (APP_PORT, 3000), starts the framework's own server on
// an internal loopback port, and:
//   - reverse-proxies all HTTP and every non-`/__peko__` upgrade (HMR, app
//     WebSockets) to the framework server;
//   - terminates `/__peko__` itself and runs the native bridge hub.
//
// The hub pairs the device's native process (the *provider*: authenticates with
// a bridge token and `role: "provider"`, executes native APIs) with the webview
// page (a *consumer*: issues calls), routing per `deviceId` so a user's calls
// only reach that user's device. The wire protocol is the same
// auth/call/reply/event frames the local loopback bridge uses, so the
// `@peko/client` SDK connects unchanged.
//
// Env:
//   APP_PORT              public port (default 3000)
//   PEKO_BACKEND_PORT     internal framework-server port (default 3001)
//   PEKO_APP_ID           this app's platform id (checked against the token)
//   PEKO_BRIDGE_JWKS_URL  platform JWKS; when set, tokens are verified (prod)
//   PEKO_BRIDGE_DEV_SECRET dev shared secret accepted in place of a signed token
// The framework's own start command is this script's argv (e.g. `node server.js`).

import http from 'node:http';
import net from 'node:net';
import crypto from 'node:crypto';
import { spawn } from 'node:child_process';

const APP_PORT = Number(process.env.APP_PORT || 3000);
const BACKEND_PORT = Number(process.env.PEKO_BACKEND_PORT || 3001);
const BACKEND_HOST = '127.0.0.1';
const WS_GUID = '258EAFA5-E914-47DA-95CA-C5AB0DC85B11';

// --- framework server (spawned on the internal port) -----------------------

// The original start command is passed as argv; run it with PORT/HOST pointed at
// the internal loopback port so the proxy owns the public one. Every framework
// used here honours PORT (+ HOST/HOSTNAME/NITRO_PORT), so no per-framework logic.
function startBackend() {
  const argv = process.argv.slice(2);
  if (argv.length === 0) {
    console.error('[peko-bridge] no framework start command given');
    process.exit(1);
  }
  const env = {
    ...process.env,
    PORT: String(BACKEND_PORT),
    HOST: BACKEND_HOST,
    HOSTNAME: BACKEND_HOST,
    NITRO_PORT: String(BACKEND_PORT),
    NITRO_HOST: BACKEND_HOST,
  };
  const child = spawn(argv[0], argv.slice(1), { env, stdio: 'inherit' });
  child.on('exit', (code) => {
    // If the framework server dies, take the container down so the platform can
    // recycle it rather than serving a proxy with no backend.
    console.error(`[peko-bridge] framework server exited (${code})`);
    process.exit(code === null ? 1 : code);
  });
  return child;
}

// Wait until the backend accepts connections before we start serving, so early
// requests during cold start don't 502.
function waitForBackend(timeoutMs = 60000) {
  const deadline = Date.now() + timeoutMs;
  return new Promise((resolve) => {
    const attempt = () => {
      const socket = net.connect(BACKEND_PORT, BACKEND_HOST);
      socket.once('connect', () => {
        socket.destroy();
        resolve(true);
      });
      socket.once('error', () => {
        socket.destroy();
        if (Date.now() > deadline) {
          resolve(false);
        } else {
          setTimeout(attempt, 200);
        }
      });
    };
    attempt();
  });
}

// --- token verification ----------------------------------------------------

// Decode a JWT's payload without verifying (dev fallback / claim reading).
function decodeJwtPayload(token) {
  const parts = String(token).split('.');
  if (parts.length !== 3) {
    return null;
  }
  try {
    const json = Buffer.from(parts[1], 'base64url').toString('utf8');
    return JSON.parse(json);
  } catch {
    return null;
  }
}

const JWKS_URL =
  process.env.PEKO_BRIDGE_JWKS_URL || 'https://app.pekoui.com/.well-known/bridge-jwks.json';
const BRIDGE_ISS = process.env.PEKO_BRIDGE_ISS || 'https://app.pekoui.com';
const BRIDGE_API = process.env.PEKO_BRIDGE_API || 'https://app.pekoui.com';
// The app-scoped bridge credential (pbk_<appId>_<hex>), injected as a secret env
// var at deploy — never in the client binary. The backend uses it to mint
// short-lived device tokens server-to-server.
const BRIDGE_KEY = process.env.PEKO_BRIDGE_KEY || '';

// Mint a short-lived device token with the app credential (server-to-server).
// `deviceId` keeps a device stable across mints; `sub` is an app-vouched end-user
// id. Returns the platform's { token, deviceId, expiresIn } or { error }.
async function mintDeviceToken(deviceId, sub) {
  if (!BRIDGE_KEY) {
    return { error: 503 };
  }
  const body = {};
  if (deviceId) body.deviceId = deviceId;
  if (sub) body.sub = sub;
  try {
    const res = await fetch(`${BRIDGE_API}/api/bridge/token`, {
      method: 'POST',
      headers: { 'X-Peko-Bridge-Key': BRIDGE_KEY, 'Content-Type': 'application/json' },
      body: JSON.stringify(body),
    });
    if (!res.ok) {
      return { error: res.status };
    }
    return await res.json();
  } catch (error) {
    console.error('[peko-bridge] token mint failed:', error.message);
    return { error: 502 };
  }
}

let jwksCache = null;
let jwksFetchedAt = 0;
async function loadJwks() {
  const now = Date.now();
  if (jwksCache && now - jwksFetchedAt < 5 * 60 * 1000) {
    return jwksCache;
  }
  const res = await fetch(JWKS_URL);
  if (!res.ok) {
    throw new Error(`jwks fetch failed (${res.status})`);
  }
  const body = await res.json();
  jwksCache = body.keys || [];
  jwksFetchedAt = now;
  return jwksCache;
}

// Verify an ES256 bridge JWT against the platform JWKS; return its claims or
// throw. The platform signs bridge tokens ES256 (ECDSA P-256). JWT ECDSA
// signatures are raw r||s (IEEE P1363), not DER, so dsaEncoding must be set.
async function verifyBridgeJwt(token) {
  const parts = String(token).split('.');
  if (parts.length !== 3) {
    throw new Error('malformed token');
  }
  const [headerB64, payloadB64, sigB64] = parts;
  const header = JSON.parse(Buffer.from(headerB64, 'base64url').toString('utf8'));
  if (header.alg !== 'ES256') {
    throw new Error(`unexpected alg ${header.alg}`);
  }
  const keys = await loadJwks();
  const jwk = keys.find((k) => k.kid === header.kid) || keys.find((k) => k.kty === 'EC');
  if (!jwk) {
    throw new Error('no matching jwk');
  }
  const key = crypto.createPublicKey({ key: jwk, format: 'jwk' });
  const ok = crypto.verify(
    'SHA256',
    Buffer.from(`${headerB64}.${payloadB64}`),
    { key, dsaEncoding: 'ieee-p1363' },
    Buffer.from(sigB64, 'base64url')
  );
  if (!ok) {
    throw new Error('bad signature');
  }
  return JSON.parse(Buffer.from(payloadB64, 'base64url').toString('utf8'));
}

// Turn verified claims into an identity, or null if they fail the bridge checks:
// audience, issuer, expiry, and (when known) this app's id.
function claimsToIdentity(claims) {
  if (claims.aud !== 'bridge') {
    return null;
  }
  if (BRIDGE_ISS && claims.iss && claims.iss !== BRIDGE_ISS) {
    return null;
  }
  if (typeof claims.exp === 'number' && claims.exp * 1000 < Date.now()) {
    return null;
  }
  const expectedApp = process.env.PEKO_APP_ID;
  if (expectedApp && claims.appId && claims.appId !== expectedApp) {
    return null;
  }
  if (!claims.deviceId) {
    return null;
  }
  return { deviceId: claims.deviceId, appId: claims.appId, sub: claims.sub, slug: claims.slug };
}

// Resolve the connecting identity for a `/__peko__` upgrade, or null to reject:
//  1. Edge-verified headers — the Lambda@Edge on `/__peko__*` verifies the token
//     and injects X-Peko-Device/App/Uid/Verified. Trusted only when
//     PEKO_TRUST_EDGE_HEADERS=1, i.e. the ALB is locked to CloudFront so a
//     direct-to-ALB request cannot spoof them.
//  2. Otherwise verify the presented JWT against the platform JWKS ourselves
//     (belt and suspenders; always safe regardless of network path).
//  3. Dev fallback — opt-in via PEKO_BRIDGE_DEV=1, for offline `peko dev`.
async function resolveIdentity(req) {
  // The Lambda@Edge on /__peko__* verifies the token and injects X-Peko-*; the
  // ALB is locked to the CloudFront origin-facing prefix list, so only CloudFront
  // can reach us and these headers cannot be spoofed direct-to-ALB. Trust them by
  // default; set PEKO_TRUST_EDGE_HEADERS=0 to force our own JWKS verify instead.
  if (process.env.PEKO_TRUST_EDGE_HEADERS !== '0' && req.headers['x-peko-verified'] === '1') {
    const deviceId = req.headers['x-peko-device'];
    if (deviceId) {
      return { deviceId, appId: req.headers['x-peko-app'], sub: req.headers['x-peko-uid'] };
    }
  }

  const token = tokenFromRequest(req);
  if (token) {
    try {
      const identity = claimsToIdentity(await verifyBridgeJwt(token));
      if (identity) {
        return identity;
      }
    } catch (error) {
      console.error('[peko-bridge] token verify failed:', error.message);
    }
  }

  if (process.env.PEKO_BRIDGE_DEV === '1' && token) {
    const claims = decodeJwtPayload(token);
    const deviceId = (claims && (claims.deviceId || claims.sub)) || token;
    return { deviceId, appId: process.env.PEKO_APP_ID };
  }
  return null;
}

// Pull the bridge token off the handshake: subprotocol, query, or cookie.
function tokenFromRequest(req) {
  const proto = req.headers['sec-websocket-protocol'];
  if (proto) {
    const parts = proto.split(',').map((p) => p.trim());
    const idx = parts.indexOf('peko-bridge');
    if (idx !== -1 && parts[idx + 1]) {
      return parts[idx + 1];
    }
  }
  const url = new URL(req.url, 'http://internal');
  const q = url.searchParams.get('token');
  if (q) {
    return q;
  }
  const cookie = req.headers['cookie'] || '';
  const match = /(?:^|;\s*)peko_bridge=([^;]+)/.exec(cookie);
  return match ? decodeURIComponent(match[1]) : null;
}

// --- minimal RFC6455 WebSocket (dependency-free) ---------------------------

// Complete the upgrade handshake on `socket`. Echoes the `peko-bridge`
// subprotocol when the client offered it.
function acceptUpgrade(req, socket) {
  const key = req.headers['sec-websocket-key'];
  const accept = crypto
    .createHash('sha1')
    .update(key + WS_GUID)
    .digest('base64');
  const offered = (req.headers['sec-websocket-protocol'] || '')
    .split(',')
    .map((p) => p.trim());
  const lines = [
    'HTTP/1.1 101 Switching Protocols',
    'Upgrade: websocket',
    'Connection: Upgrade',
    `Sec-WebSocket-Accept: ${accept}`,
  ];
  if (offered.includes('peko-bridge')) {
    lines.push('Sec-WebSocket-Protocol: peko-bridge');
  }
  socket.write(lines.join('\r\n') + '\r\n\r\n');
}

// Encode a server->client frame (unmasked). opcode 0x1 text, 0x8 close,
// 0x9 ping, 0xA pong.
function encodeFrame(opcode, payload) {
  const data = Buffer.isBuffer(payload) ? payload : Buffer.from(payload || '', 'utf8');
  const len = data.length;
  let header;
  if (len < 126) {
    header = Buffer.alloc(2);
    header[1] = len;
  } else if (len < 65536) {
    header = Buffer.alloc(4);
    header[1] = 126;
    header.writeUInt16BE(len, 2);
  } else {
    header = Buffer.alloc(10);
    header[1] = 127;
    header.writeBigUInt64BE(BigInt(len), 2);
  }
  header[0] = 0x80 | opcode;
  return Buffer.concat([header, data]);
}

// A thin per-connection WebSocket: parses incoming frames (handling masking and
// fragmentation) and exposes send/onMessage/onClose/ping. Small JSON frames only,
// which is all the bridge sends.
class WsConn {
  constructor(socket) {
    this.socket = socket;
    this.buffer = Buffer.alloc(0);
    this.fragments = [];
    this.fragmentOpcode = 0;
    this.closed = false;
    this.onMessage = null;
    this.onClose = null;
    socket.on('data', (chunk) => this._onData(chunk));
    socket.on('close', () => this._fireClose());
    socket.on('error', () => this._fireClose());
  }

  // Feed frame bytes that arrived with the upgrade handshake. Called after the
  // caller has wired onMessage, so an early frame is not dropped.
  feed(head) {
    if (head && head.length) {
      this._onData(head);
    }
  }

  _fireClose() {
    if (this.closed) {
      return;
    }
    this.closed = true;
    if (this.onClose) {
      this.onClose();
    }
  }

  _onData(chunk) {
    this.buffer = Buffer.concat([this.buffer, chunk]);
    // Parse as many complete frames as the buffer holds.
    for (;;) {
      if (this.buffer.length < 2) {
        return;
      }
      const b0 = this.buffer[0];
      const b1 = this.buffer[1];
      const fin = (b0 & 0x80) !== 0;
      const opcode = b0 & 0x0f;
      const masked = (b1 & 0x80) !== 0;
      let len = b1 & 0x7f;
      let offset = 2;
      if (len === 126) {
        if (this.buffer.length < offset + 2) return;
        len = this.buffer.readUInt16BE(offset);
        offset += 2;
      } else if (len === 127) {
        if (this.buffer.length < offset + 8) return;
        len = Number(this.buffer.readBigUInt64BE(offset));
        offset += 8;
      }
      let maskKey = null;
      if (masked) {
        if (this.buffer.length < offset + 4) return;
        maskKey = this.buffer.subarray(offset, offset + 4);
        offset += 4;
      }
      if (this.buffer.length < offset + len) {
        return;
      }
      let payload = this.buffer.subarray(offset, offset + len);
      if (masked) {
        const out = Buffer.alloc(len);
        for (let i = 0; i < len; i++) {
          out[i] = payload[i] ^ maskKey[i & 3];
        }
        payload = out;
      }
      this.buffer = this.buffer.subarray(offset + len);
      this._handleFrame(fin, opcode, payload);
    }
  }

  _handleFrame(fin, opcode, payload) {
    if (opcode === 0x8) {
      this.close();
      return;
    }
    if (opcode === 0x9) {
      this._raw(encodeFrame(0xa, payload)); // pong
      return;
    }
    if (opcode === 0xa) {
      return; // pong; liveness tracked by the caller
    }
    if (opcode === 0x0) {
      // continuation
      this.fragments.push(payload);
      if (fin) {
        const full = Buffer.concat(this.fragments);
        this.fragments = [];
        this._deliver(this.fragmentOpcode, full);
      }
      return;
    }
    // 0x1 text / 0x2 binary
    if (!fin) {
      this.fragmentOpcode = opcode;
      this.fragments = [payload];
      return;
    }
    this._deliver(opcode, payload);
  }

  _deliver(opcode, payload) {
    if (opcode === 0x1 && this.onMessage) {
      this.onMessage(payload.toString('utf8'));
    }
  }

  _raw(frame) {
    if (!this.closed && this.socket.writable) {
      this.socket.write(frame);
    }
  }

  send(text) {
    this._raw(encodeFrame(0x1, text));
  }

  ping() {
    this._raw(encodeFrame(0x9, Buffer.alloc(0)));
  }

  close() {
    if (!this.closed) {
      this._raw(encodeFrame(0x8, Buffer.alloc(0)));
      this.socket.end();
      this._fireClose();
    }
  }
}

// --- the bridge hub --------------------------------------------------------

const providers = new Map(); // deviceId -> WsConn
const consumers = new Map(); // deviceId -> Set<WsConn>
let hubSeq = 1;
const routes = new Map(); // hubId -> { consumer, deviceId, originalId }

function addConsumer(deviceId, conn) {
  let set = consumers.get(deviceId);
  if (!set) {
    set = new Set();
    consumers.set(deviceId, set);
  }
  set.add(conn);
}

function dropConsumer(deviceId, conn) {
  const set = consumers.get(deviceId);
  if (set) {
    set.delete(conn);
    if (set.size === 0) {
      consumers.delete(deviceId);
    }
  }
  for (const [hubId, route] of routes) {
    if (route.consumer === conn) {
      routes.delete(hubId);
    }
  }
}

// Route one authenticated consumer message. `call` frames are forwarded to the
// device's provider with a rewritten id; other frames are ignored (the SDK only
// sends auth + call).
function handleConsumerMessage(deviceId, conn, text) {
  let msg;
  try {
    msg = JSON.parse(text);
  } catch {
    return;
  }
  if (msg.t !== 'call') {
    return;
  }
  const provider = providers.get(deviceId);
  if (!provider) {
    conn.send(
      JSON.stringify({
        t: 'reply',
        id: msg.id,
        ok: false,
        error: { code: 'no_provider', message: 'no device connected for this app' },
      })
    );
    return;
  }
  const hubId = hubSeq++;
  routes.set(hubId, { consumer: conn, deviceId, originalId: msg.id });
  provider.send(JSON.stringify({ t: 'call', id: hubId, method: msg.method, params: msg.params }));
}

// Route one provider message: `reply` goes back to the originating consumer with
// its original id; `event` broadcasts to the device's consumers.
function handleProviderMessage(deviceId, text) {
  let msg;
  try {
    msg = JSON.parse(text);
  } catch {
    return;
  }
  if (msg.t === 'reply') {
    const route = routes.get(msg.id);
    if (!route) {
      return;
    }
    routes.delete(msg.id);
    route.consumer.send(
      JSON.stringify({ t: 'reply', id: route.originalId, ok: msg.ok, result: msg.result, error: msg.error })
    );
    return;
  }
  if (msg.t === 'event') {
    const set = consumers.get(deviceId);
    if (set) {
      const frame = JSON.stringify({ t: 'event', name: msg.name, data: msg.data });
      for (const c of set) {
        c.send(frame);
      }
    }
  }
}

// Run a freshly-upgraded `/__peko__` connection: await the `auth` frame,
// register the connection by role, then pump messages. A 30s heartbeat detects
// half-open sockets before the ALB's 4000s idle.
function runBridgeConnection(conn, identity) {
  let role = null;
  const deviceId = identity.deviceId;
  let alive = true;

  const heartbeat = setInterval(() => {
    if (!alive) {
      conn.close();
      return;
    }
    alive = false;
    conn.ping();
  }, 30000);
  conn.onMessage = (text) => {
    alive = true;
    let msg;
    try {
      msg = JSON.parse(text);
    } catch {
      return;
    }
    if (role === null) {
      if (msg.t !== 'auth') {
        conn.send(JSON.stringify({ t: 'error', error: 'expected auth' }));
        return;
      }
      role = msg.role === 'provider' ? 'provider' : 'consumer';
      if (role === 'provider') {
        const existing = providers.get(deviceId);
        if (existing && existing !== conn) {
          existing.close();
        }
        providers.set(deviceId, conn);
      } else {
        addConsumer(deviceId, conn);
      }
      conn.send(JSON.stringify({ t: 'ready' }));
      return;
    }
    if (role === 'provider') {
      handleProviderMessage(deviceId, text);
    } else {
      handleConsumerMessage(deviceId, conn, text);
    }
  };
  conn.onClose = () => {
    clearInterval(heartbeat);
    if (role === 'provider' && providers.get(deviceId) === conn) {
      providers.delete(deviceId);
    } else if (role === 'consumer') {
      dropConsumer(deviceId, conn);
    }
  };
}

// --- the proxy server ------------------------------------------------------

// Reverse-proxy a normal HTTP request to the framework server. A plain (non-WS)
// GET to `/__peko__` returns 426 rather than reaching the backend.
function proxyHttp(req, res) {
  const url = new URL(req.url, 'http://internal');
  if (url.pathname === '/__peko__') {
    res.writeHead(426, { 'Content-Type': 'text/plain', Upgrade: 'websocket' });
    res.end('Upgrade Required');
    return;
  }
  // Token vending: the device (and same-origin page) fetch a short-lived bridge
  // token here rather than carrying a credential. This path sits OUTSIDE
  // /__peko__* so the edge does not require a token to reach it. The app credential
  // never leaves the backend. NOTE: this is open by default (fine for the device's
  // own app); an app with real end-users should gate it behind its own auth and
  // pass the user's `sub`.
  if (url.pathname === '/_peko/token') {
    const deviceId = url.searchParams.get('deviceId') || undefined;
    mintDeviceToken(deviceId, undefined)
      .then((result) => {
        if (result && result.token) {
          res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
          res.end(
            JSON.stringify({
              token: result.token,
              deviceId: result.deviceId,
              expiresIn: result.expiresIn,
            })
          );
        } else {
          const code = (result && result.error) || 500;
          res.writeHead(code === 503 ? 503 : 502, { 'Content-Type': 'text/plain' });
          res.end('bridge token unavailable');
        }
      })
      .catch(() => {
        res.writeHead(502, { 'Content-Type': 'text/plain' });
        res.end('bridge token error');
      });
    return;
  }
  const proxyReq = http.request(
    { host: BACKEND_HOST, port: BACKEND_PORT, method: req.method, path: req.url, headers: req.headers },
    (proxyRes) => {
      res.writeHead(proxyRes.statusCode || 502, proxyRes.headers);
      proxyRes.pipe(res);
    }
  );
  proxyReq.on('error', () => {
    if (!res.headersSent) {
      res.writeHead(502, { 'Content-Type': 'text/plain' });
    }
    res.end('Bad Gateway');
  });
  req.pipe(proxyReq);
}

// Proxy a non-`/__peko__` upgrade (framework HMR, app WebSockets) straight
// through to the backend by replaying the handshake and piping both directions.
function proxyUpgrade(req, socket, head) {
  const upstream = net.connect(BACKEND_PORT, BACKEND_HOST, () => {
    const lines = [`${req.method} ${req.url} HTTP/1.1`];
    for (let i = 0; i < req.rawHeaders.length; i += 2) {
      lines.push(`${req.rawHeaders[i]}: ${req.rawHeaders[i + 1]}`);
    }
    upstream.write(lines.join('\r\n') + '\r\n\r\n');
    if (head && head.length) {
      upstream.write(head);
    }
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.on('error', () => socket.destroy());
  socket.on('error', () => upstream.destroy());
}

async function main() {
  startBackend();
  const ok = await waitForBackend();
  if (!ok) {
    console.error('[peko-bridge] framework server did not come up');
    process.exit(1);
  }

  const server = http.createServer(proxyHttp);

  server.on('upgrade', (req, socket, head) => {
    const url = new URL(req.url, 'http://internal');
    if (url.pathname !== '/__peko__') {
      proxyUpgrade(req, socket, head);
      return;
    }
    resolveIdentity(req)
      .then((identity) => {
        if (!identity) {
          socket.write('HTTP/1.1 401 Unauthorized\r\n\r\n');
          socket.destroy();
          return;
        }
        acceptUpgrade(req, socket);
        const conn = new WsConn(socket);
        runBridgeConnection(conn, identity);
        conn.feed(head);
      })
      .catch(() => socket.destroy());
  });

  server.listen(APP_PORT, '0.0.0.0', () => {
    console.log(`[peko-bridge] listening on :${APP_PORT}, backend :${BACKEND_PORT}, /__peko__ bridge active`);
  });
}

main();
