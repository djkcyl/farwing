import test from 'node:test';
import assert from 'node:assert/strict';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';
import { io as connectSocketIo } from 'socket.io-client';
import WebSocket from 'ws';
import { createServer } from '../index.js';
import { createPairingCode, revokeDevice } from '../db.js';

async function fixture(t, options = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'openuuyc-server-'));
  const app = createServer({
    dbPath: join(dir, 'devices.sqlite'),
    host: '127.0.0.1',
    port: 0,
    maintenanceIntervalMs: 50,
    wsHeartbeatIntervalMs: 100,
    ...options,
  });
  await app.listen(0, '127.0.0.1');
  t.after(async () => {
    await app.close();
    await rm(dir, { recursive: true, force: true });
  });
  return { app, dir, origin: app.publicOrigin() };
}

async function api(origin, path, { token, method = 'GET', body, headers = {} } = {}) {
  const requestHeaders = { ...headers };
  if (token) requestHeaders.Authorization = `Bearer ${token}`;
  if (body !== undefined) requestHeaders['Content-Type'] = 'application/json';
  const response = await fetch(`${origin}${path}`, {
    method,
    headers: requestHeaders,
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const value = await response.json();
  return { response, value };
}

async function pair(app, role, platform, name, boundHostId = null) {
  const grant = createPairingCode(app.db, { role, boundHostId });
  const { response, value } = await api(app.publicOrigin(), '/v1/pair', {
    method: 'POST',
    body: { role, platform, name, code: grant.code },
  });
  assert.equal(response.status, 201, JSON.stringify(value));
  return value;
}

function nextSocketIoEvent(socket, event, timeoutMs = 1500) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      socket.off(event, onEvent);
      reject(new Error(`timed out waiting for Socket.IO ${event}`));
    }, timeoutMs);
    const onEvent = (...args) => {
      clearTimeout(timeout);
      resolve(args);
    };
    socket.once(event, onEvent);
  });
}

function nextWebSocketMessage(socket, predicate = () => true, timeoutMs = 1500) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      socket.off('message', onMessage);
      reject(new Error('timed out waiting for controller WebSocket message'));
    }, timeoutMs);
    const onMessage = (raw) => {
      let message;
      try { message = JSON.parse(raw.toString()); } catch { return; }
      if (!predicate(message)) return;
      clearTimeout(timeout);
      resolve(message);
    };
    socket.on('message', onMessage);
  });
}

function connectHost(url, ticket) {
  const signaling = new URL(url);
  const serverOrigin = `${signaling.protocol === 'wss:' ? 'https:' : 'http:'}//${signaling.host}`;
  return connectSocketIo(serverOrigin, {
    autoConnect: false,
    reconnection: false,
    timeout: 1500,
    path: signaling.pathname,
    transports: ['websocket'],
    extraHeaders: {
      'X-NRD-AUTH': ticket,
      'X-NRD-CONTROLLING': '0',
    },
  });
}

function connected(socket) {
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('host Socket.IO connection timed out')), 1500);
    socket.once('connect', () => { clearTimeout(timer); resolve(); });
    socket.once('connect_error', (error) => { clearTimeout(timer); reject(error); });
    socket.connect();
  });
}

async function openController(url, ticket) {
  const socket = new WebSocket(url);
  socket.pingSeen = new Promise((resolve) => socket.once('ping', resolve));
  await new Promise((resolve, reject) => {
    socket.once('open', resolve);
    socket.once('error', reject);
  });
  const readyPromise = nextWebSocketMessage(socket, (message) => message.type === 'ready');
  socket.send(JSON.stringify({ type: 'authenticate', ticket }));
  const ready = await readyPromise;
  assert.equal(ready.type, 'ready');
  return socket;
}

test('pairs roles atomically, isolates controllers, and relays host signaling', async (t) => {
  const { app, origin } = await fixture(t);
  const hostA = await pair(app, 'host', 'windows', 'Office PC');
  const hostB = await pair(app, 'host', 'windows', 'Lab PC');
  const controllerA = await pair(app, 'controller', 'web', 'Browser A', hostA.deviceId);
  const controllerA2 = await pair(app, 'controller', 'ios', 'Phone A', hostA.deviceId);
  const controllerB = await pair(app, 'controller', 'web', 'Browser B', hostB.deviceId);

  const duplicateCode = createPairingCode(app.db, { role: 'host' });
  const firstUse = await api(origin, '/v1/pair', {
    method: 'POST',
    body: { role: 'host', platform: 'windows', name: 'Single use', code: duplicateCode.code },
  });
  assert.equal(firstUse.response.status, 201);
  const secondUse = await api(origin, '/v1/pair', {
    method: 'POST',
    body: { role: 'host', platform: 'windows', name: 'Replay', code: duplicateCode.code },
  });
  assert.equal(secondUse.response.status, 400);
  assert.deepEqual(Object.keys(secondUse.value), ['error']);

  const roomA = await api(origin, '/v1/host/room', { token: hostA.token, method: 'POST', body: {} });
  const roomB = await api(origin, '/v1/host/room', { token: hostB.token, method: 'POST', body: {} });
  assert.equal(roomA.response.status, 200);
  assert.equal(roomA.value.signalingUrl, `ws://127.0.0.1:${new URL(origin).port}/socket.io/`);

  const socketA = connectHost(roomA.value.signalingUrl, roomA.value.ticket);
  const socketB = connectHost(roomB.value.signalingUrl, roomB.value.ticket);
  t.after(() => { socketA.disconnect(); socketB.disconnect(); });
  await Promise.all([connected(socketA), connected(socketB)]);
  const roomInfo = await new Promise((resolve) => socketA.emit('room_info', resolve));
  assert.equal(roomInfo.room_id, hostA.deviceId);
  const accepting = await api(origin, '/v1/host/controllable', {
    token: hostA.token,
    method: 'PUT',
    body: { enabled: true },
  });
  assert.equal(accepting.response.status, 200);

  const crossHost = await api(origin, '/v1/sessions', {
    token: controllerA.token,
    method: 'POST',
    body: { deviceId: hostB.deviceId },
  });
  assert.equal(crossHost.response.status, 403);
  const otherController = await api(origin, '/v1/sessions', {
    token: controllerB.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(otherController.response.status, 403);

  const created = await api(origin, '/v1/sessions', {
    token: controllerA.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(created.response.status, 201, JSON.stringify(created.value));
  assert.deepEqual(created.value.iceServers, []);
  assert.equal(new URL(created.value.signalingUrl).origin, new URL(origin).origin.replace(/^http/u, 'ws'));

  const duplicateSession = await api(origin, '/v1/sessions', {
    token: controllerA2.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(duplicateSession.response.status, 409);

  const beControlled = nextSocketIoEvent(socketA, 'be-controlled');
  const browser = await openController(created.value.signalingUrl, created.value.ticket);
  t.after(() => { browser.close(); });
  const [hostRequest] = await beControlled;
  assert.equal(hostRequest.client_id.length > 0, true);
  assert.equal(hostRequest.ice_id.length > 0, true);
  assert.equal(hostRequest.app_control_id.length > 0, true);
  assert.ok(Buffer.isBuffer(hostRequest.app_data), 'ConnectOptions is the be-controlled binary attachment');
  assert.ok(hostRequest.app_data.length > 10);
  assert.equal(socketB.listeners('be-controlled').length, 0);
  await browser.pingSeen;

  const replay = new WebSocket(created.value.signalingUrl);
  await new Promise((resolve, reject) => {
    replay.once('open', resolve);
    replay.once('error', reject);
  });
  const replayError = nextWebSocketMessage(replay, (message) => message.type === 'error');
  replay.send(JSON.stringify({ type: 'authenticate', ticket: created.value.ticket }));
  assert.match((await replayError).message, /invalid or expired/u);
  replay.close();

  const capabilityPromise = nextWebSocketMessage(browser, (message) => message.type === 'capability');
  socketA.emit('forward_setting', {
    client_id: hostRequest.client_id,
    data: {
      type: 'device_capability',
      device_capability: {
        ice_id: hostRequest.ice_id,
        display_info: [{ id: 0, fps: 60, type: 0, hdr: 0 }],
        video_codec_capability: [{ video_codec: 1, width: 1920, height: 1080, chroma_sampling: 1, bit_depth: 8 }],
      },
    },
  });
  assert.equal((await capabilityPromise).capability.ice_id, hostRequest.ice_id);

  const offerPromise = nextSocketIoEvent(socketA, 'soac');
  browser.send(JSON.stringify({ type: 'offer', sdp: 'v=0\r\no=browser 0 0 IN IP4 127.0.0.1\r\n' }));
  const [offer] = await offerPromise;
  assert.equal(offer.client_id, hostRequest.client_id);
  assert.equal(offer.data.ice_id, hostRequest.ice_id);
  assert.equal(offer.data.app_control_id, hostRequest.app_control_id);
  assert.equal(offer.data.type, 'offer');

  const controllerCandidatePromise = nextSocketIoEvent(socketA, 'soac');
  browser.send(JSON.stringify({
    type: 'candidate',
    candidate: {
      candidate: 'candidate:1 1 udp 2122260223 192.0.2.10 5000 typ host',
      sdpMid: '0',
      sdpMLineIndex: 0,
      usernameFragment: 'u1',
    },
  }));
  const [controllerCandidate] = await controllerCandidatePromise;
  assert.equal(controllerCandidate.data.type, 'candidate');
  assert.equal(controllerCandidate.data.candidate.usernameFragment, 'u1');

  const answerPromise = nextWebSocketMessage(browser, (message) => message.type === 'answer');
  socketA.emit('soac', {
    client_id: hostRequest.client_id,
    data: {
      app_control_id: hostRequest.app_control_id,
      ice_id: hostRequest.ice_id,
      type: 'answer',
      sdp: '',
      gzip_sdp: gzipSync(Buffer.from('v=0\r\no=host 0 0 IN IP4 127.0.0.1\r\n')),
    },
  });
  assert.match((await answerPromise).sdp, /^v=0/u);

  const hostCandidatePromise = nextWebSocketMessage(browser, (message) => message.type === 'candidate');
  socketA.emit('soac', {
    client_id: hostRequest.client_id,
    data: {
      app_control_id: hostRequest.app_control_id,
      ice_id: hostRequest.ice_id,
      type: 'candidate',
      candidate: {
        candidate: 'candidate:2 1 udp 2122260223 192.0.2.11 5001 typ host',
        sdpMid: '0',
        sdpMLineIndex: 0,
        usernameFragment: 'u2',
      },
    },
  });
  assert.equal((await hostCandidatePromise).candidate.usernameFragment, 'u2');

  const leftPromise = nextSocketIoEvent(socketA, 'left');
  browser.send(JSON.stringify({ type: 'leave' }));
  const [left] = await leftPromise;
  assert.equal(left.ice_id, hostRequest.ice_id);

  const sessionB = await api(origin, '/v1/sessions', {
    token: controllerA2.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(sessionB.response.status, 201);
  const beControlledAgain = nextSocketIoEvent(socketA, 'be-controlled');
  const browserB = await openController(sessionB.value.signalingUrl, sessionB.value.ticket);
  t.after(() => browserB.close());
  const [hostRequestB] = await beControlledAgain;
  assert.ok(hostRequestB.client_id.length > 0);
  const replacedSession = nextWebSocketMessage(browserB, (message) => message.type === 'ended');
  const oldSocketClosed = new Promise((resolve) => socketA.once('disconnect', resolve));
  const replacementRoom = await api(origin, '/v1/host/room', { token: hostA.token, method: 'POST', body: {} });
  assert.equal(replacementRoom.response.status, 200);
  assert.notEqual(replacementRoom.value.ticket, roomA.value.ticket);
  const [replacementEnded] = await Promise.all([replacedSession, oldSocketClosed]);
  assert.equal(replacementEnded.reason, 'host_room_replaced');

  const oldTicketSocket = connectHost(roomA.value.signalingUrl, roomA.value.ticket);
  const oldTicketRejected = new Promise((resolve) => oldTicketSocket.once('connect_error', resolve));
  oldTicketSocket.connect();
  await oldTicketRejected;
  oldTicketSocket.disconnect();

  const whileOffline = await api(origin, '/v1/sessions', {
    token: controllerA.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(whileOffline.response.status, 409);
  assert.equal(whileOffline.value.error, 'host is offline');

  const replacementSocket = connectHost(replacementRoom.value.signalingUrl, replacementRoom.value.ticket);
  t.after(() => replacementSocket.disconnect());
  await connected(replacementSocket);
  const afterRoomRefresh = await api(origin, '/v1/devices', { token: controllerA.token });
  assert.equal(afterRoomRefresh.value.devices[0].controllable, false);
  const deniedUntilReenabled = await api(origin, '/v1/sessions', {
    token: controllerA.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(deniedUntilReenabled.response.status, 409);
  const reenabled = await api(origin, '/v1/host/controllable', {
    token: hostA.token,
    method: 'PUT',
    body: { enabled: true },
  });
  assert.equal(reenabled.response.status, 200);

  const sessionC = await api(origin, '/v1/sessions', {
    token: controllerA2.token,
    method: 'POST',
    body: { deviceId: hostA.deviceId },
  });
  assert.equal(sessionC.response.status, 201);
  const beControlledFinal = nextSocketIoEvent(replacementSocket, 'be-controlled');
  const browserC = await openController(sessionC.value.signalingUrl, sessionC.value.ticket);
  t.after(() => browserC.close());
  const [hostRequestC] = await beControlledFinal;
  const endedPromise = nextWebSocketMessage(browserC, (message) => message.type === 'ended');
  const releasedPromise = nextSocketIoEvent(replacementSocket, 'released');
  assert.equal(revokeDevice(app.db, controllerA2.deviceId), true);
  const ended = await endedPromise;
  const [released] = await releasedPromise;
  assert.equal(ended.reason, 'device_revoked');
  assert.equal(released.ice_id, hostRequestC.ice_id);

  const deviceList = await api(origin, '/v1/devices', { token: controllerA.token });
  assert.equal(deviceList.response.status, 200);
  assert.deepEqual(deviceList.value.devices.map((device) => device.id), [hostA.deviceId]);
  const health = await api(origin, '/healthz');
  assert.equal(health.response.status, 200);
  assert.deepEqual(health.value, { status: 'ok' });
});

test('same-origin static fallback confines paths to web root', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'openuuyc-static-'));
  const web = join(dir, 'web');
  await mkdir(web, { recursive: true });
  await writeFile(join(web, 'index.html'), '<main>self host</main>');
  await writeFile(join(dir, 'outside.txt'), 'secret');
  const { origin } = await fixture(t, { dbPath: join(dir, 'db.sqlite'), staticRoot: web });
  t.after(() => rm(dir, { recursive: true, force: true }));
  const response = await fetch(`${origin}/`);
  assert.equal(response.status, 200);
  assert.equal(await response.text(), '<main>self host</main>');
  const traversal = await fetch(`${origin}/..%2foutside.txt`);
  assert.equal(traversal.status, 404);
});

test('expires host room tickets and permits re-enrollment with the device bearer', async (t) => {
  let currentTime = Date.now();
  const { app, origin } = await fixture(t, { now: () => currentTime });
  const host = await pair(app, 'host', 'windows', 'Expiring host');
  const room = await api(origin, '/v1/host/room', { token: host.token, method: 'POST', body: {} });
  assert.equal(room.response.status, 200);
  const expiresAt = Date.parse(room.value.expiresAt);
  assert.equal(expiresAt - currentTime, 24 * 60 * 60_000);
  const socket = connectHost(room.value.signalingUrl, room.value.ticket);
  t.after(() => socket.disconnect());
  await connected(socket);
  const disconnected = new Promise((resolve) => socket.once('disconnect', resolve));
  currentTime = expiresAt + 1;
  await Promise.race([
    disconnected,
    new Promise((_, reject) => setTimeout(() => reject(new Error('expired host room remained connected')), 1500)),
  ]);
  const devices = await api(origin, '/v1/devices', { token: host.token });
  assert.equal(devices.value.devices[0].online, false);
  const replacement = await api(origin, '/v1/host/room', { token: host.token, method: 'POST', body: {} });
  assert.equal(replacement.response.status, 200);
  assert.notEqual(replacement.value.ticket, room.value.ticket);
});

test('rejects browser origins outside the configured public origin and unsafe public binds', async (t) => {
  const { app, origin } = await fixture(t);
  const host = await pair(app, 'host', 'windows', 'Origin host');
  const devices = await api(origin, '/v1/devices', {
    token: host.token,
    headers: { Origin: 'https://attacker.example' },
  });
  assert.equal(devices.response.status, 403);
  assert.deepEqual(Object.keys(devices.value), ['error']);
  const rejectedSocket = new WebSocket(`${origin.replace(/^http/u, 'ws')}/v1/session`, {
    headers: { Origin: 'https://attacker.example' },
  });
  const rejected = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      rejectedSocket.terminate();
      reject(new Error('unexpectedly accepted a foreign WebSocket origin'));
    }, 1000);
    rejectedSocket.once('error', (error) => { clearTimeout(timer); resolve(error); });
    rejectedSocket.once('open', () => { clearTimeout(timer); rejectedSocket.close(); reject(new Error('foreign WebSocket origin was accepted')); });
  });
  assert.match(rejected.message, /403/u);
  assert.throws(() => createServer({ dbPath: join(tmpdir(), 'unused.sqlite'), host: '0.0.0.0' }), /plaintext HTTP may bind only to loopback/u);
});
