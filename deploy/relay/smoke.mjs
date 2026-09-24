#!/usr/bin/env node
// Local-only deployment acceptance. Requires Node >=22.12 and a running Docker daemon.
import assert from 'node:assert/strict';
import { randomBytes, createHash } from 'node:crypto';
import { spawnSync } from 'node:child_process';

const image = process.argv[2] ?? 'blackwall-relay:ci';
const suffix = randomBytes(8).toString('hex');
const container = `blackwall-relay-smoke-${suffix}`;
const volume = `${container}-data`;
const token = randomBytes(32).toString('base64url');
const guestKey = `bw1_${randomBytes(32).toString('base64url')}`;
const salt = randomBytes(32);
const owner = {
  protocolVersion: 1,
  sessionId: `bws_${randomBytes(32).toString('base64url')}`,
  hostKey: `bwh_${randomBytes(32).toString('base64url')}`,
  relayToken: token,
  model: 'container-smoke-model',
  guestKeySalt: salt.toString('base64url'),
  guestKeyHash: createHash('sha256').update(salt).update(guestKey).digest('base64url'),
  expiresAtMs: Date.now() + 120_000,
};
const sockets = new Set();
let volumeCreated = false;
let containerCreated = false;

function docker(args) {
  const result = spawnSync('docker', args, { timeout: 30_000, encoding: 'utf8' });
  // Do not include arguments: registration credentials are among the run environment values.
  assert.equal(result.status, 0, `Docker command failed: ${args[0]} (${result.error?.message ?? result.stderr.trim()})`);
  return result.stdout.trim();
}
function cleanup() {
  for (const socket of sockets) socket.close();
  if (containerCreated) {
    spawnSync('docker', ['rm', '--force', container], { timeout: 15_000, stdio: 'ignore' });
  }
  if (volumeCreated) {
    spawnSync('docker', ['volume', 'rm', volume], { timeout: 15_000, stdio: 'ignore' });
  }
}
for (const signal of ['SIGINT', 'SIGTERM']) {
  process.once(signal, () => { cleanup(); process.exit(1); });
}
async function until(check, message) {
  const deadline = Date.now() + 10_000;
  while (Date.now() < deadline) {
    if (await check()) return;
    await new Promise(resolve => setTimeout(resolve, 50));
  }
  assert.fail(message);
}
async function start() {
  docker(['run', '--detach', '--name', container, '--publish', '127.0.0.1::8787',
    '--mount', `type=volume,source=${volume},target=/data`,
    '--env', `BLACKWALL_RELAY_TOKEN=${token}`, image]);
  containerCreated = true;
  const binding = docker(['port', container, '8787/tcp']);
  assert.match(binding, /^127\.0\.0\.1:\d+$/);
  const origin = `http://${binding}`;
  await until(async () => {
    try { return (await fetch(`${origin}/health`, { signal: AbortSignal.timeout(1000) })).ok; }
    catch { return false; }
  }, 'Container health endpoint did not become available');
  return origin;
}
async function register(origin, registration) {
  const socket = new WebSocket(`${origin.replace('http:', 'ws:')}/v1/host/connect`);
  sockets.add(socket);
  socket.addEventListener('close', () => sockets.delete(socket));
  const reply = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => { socket.close(); reject(new Error('Registration timed out')); }, 5000);
    socket.addEventListener('open', () => socket.send(JSON.stringify({ type: 'register', registration })), { once: true });
    socket.addEventListener('message', event => { clearTimeout(timer); resolve(JSON.parse(event.data)); }, { once: true });
    socket.addEventListener('error', () => { clearTimeout(timer); reject(new Error('Registration socket failed')); }, { once: true });
    socket.addEventListener('close', () => { clearTimeout(timer); reject(new Error('Registration closed before a reply')); }, { once: true });
  });
  return { socket, reply };
}
function models(origin, key = guestKey) {
  return fetch(`${origin}/s/${owner.sessionId}/v1/models`, {
    headers: { Authorization: `Bearer ${key}` }, signal: AbortSignal.timeout(2000),
  });
}

async function pairing(origin, path, method, key, body) {
  return fetch(`${origin}/v1/pairing${path}`, {
    method, headers: { Authorization: `Bearer ${key}`, 'x-blackwall-relay-token': token, 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body), signal: AbortSignal.timeout(2000),
  });
}

try {
  docker(['volume', 'create', volume]);
  volumeCreated = true;
  let origin = await start();
  assert.equal(docker(['exec', container, 'id', '-u']), '65532');
  assert.equal(docker(['exec', container, 'stat', '-c', '%a', '/data']), '700');
  assert.equal(docker(['exec', container, 'stat', '-c', '%a', '/data/ownership.sqlite3']), '600');
  assert.equal((await (await pairing(origin, '/capabilities', 'GET', '')).json()).version, 1);
  const invitation = `bwi_${randomBytes(32).toString('base64url')}`;
  const inviteSalt = randomBytes(32);
  const exchange = {
    version: 1, id: `bwp_${randomBytes(32).toString('base64url')}`, hostName: 'Container host', model: owner.model,
    salt: inviteSalt.toString('base64url'), hash: createHash('sha256').update(inviteSalt).update(invitation).digest('base64url'),
  };
  assert.equal((await pairing(origin, '', 'POST', owner.hostKey, exchange)).status, 200);
  const candidate = { id: `bwd_${randomBytes(32).toString('base64url')}`, name: 'Container client', salt: owner.guestKeySalt, hash: owner.guestKeyHash };
  assert.equal((await pairing(origin, `/${exchange.id}/join`, 'POST', invitation, candidate)).status, 200);
  assert.equal((await models(origin)).status, 404, 'Pairing granted model access before host approval');
  assert.equal((await pairing(origin, `/${exchange.id}/approve`, 'POST', owner.hostKey, { candidate, sessionId: owner.sessionId })).status, 200);
  assert.equal((await (await pairing(origin, `/${exchange.id}/result`, 'GET', guestKey)).json()).sessionId, owner.sessionId);
  const initial = await register(origin, owner);
  assert.equal(initial.reply.type, 'registered');
  assert.equal((await models(origin, 'wrong-key')).status, 401);
  assert.equal((await (await models(origin)).json()).data[0].id, owner.model);
  initial.socket.close();
  await until(async () => !(await models(origin)).ok, 'Closed host stayed available');
  docker(['rm', '--force', container]);
  containerCreated = false;
  origin = await start();
  assert.equal((await pairing(origin, `/${exchange.id}/result`, 'GET', guestKey)).status, 404, 'Unfinished exchanges must not survive a relay restart');
  const impostor = await register(origin, { ...owner, hostKey: `bwh_${randomBytes(32).toString('base64url')}` });
  assert.equal(impostor.reply.type, 'error', 'Offline ownership was lost during replacement');
  impostor.socket.close();
  const restored = await register(origin, owner);
  assert.equal(restored.reply.type, 'registered');
  assert.equal((await models(origin)).status, 200);
  assert.equal((await pairing(origin, `/devices/${owner.sessionId}`, 'DELETE', 'wrong-key')).status, 400);
  assert.equal((await models(origin)).status, 200, 'An invalid revocation disconnected the owner');
  assert.equal((await pairing(origin, `/devices/${owner.sessionId}`, 'DELETE', owner.hostKey)).status, 200);
  await until(async () => !(await models(origin)).ok, 'Revoked device stayed available');
  docker(['rm', '--force', container]);
  containerCreated = false;
  origin = await start();
  const revoked = await register(origin, owner);
  assert.equal(revoked.reply.type, 'error', 'A stale host restored revoked access after replacement');
  revoked.socket.close();
  const stored = spawnSync('docker', ['exec', container, 'cat', '/data/ownership.sqlite3'], { timeout: 5000 });
  assert.equal(stored.status, 0);
  for (const secret of [owner.hostKey, guestKey, token, invitation]) assert.equal(stored.stdout.includes(Buffer.from(secret)), false);
  console.log('Relay container passed: non-root execution, private storage, authorization, host-approved pairing, preserved ownership, rejected takeover, reconnect, and durable revocation after replacement.');
} finally {
  cleanup();
}
