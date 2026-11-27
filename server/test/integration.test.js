import test from 'node:test';
import assert from 'node:assert/strict';
import { createHash, generateKeyPairSync, randomBytes, randomUUID, sign } from 'node:crypto';
import { mkdtemp, mkdir, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { gzipSync } from 'node:zlib';
import { get as httpGet } from 'node:http';
import { io as connectSocketIo } from 'socket.io-client';
import WebSocket from 'ws';
import { isoCBOR } from '@simplewebauthn/server/helpers';
import { createServer } from '../index.js';
import { createPairingCode, hashSecret, openDatabase, revokeDevice } from '../db.js';
import { createConnectOptions } from '../protocol.js';
import { approveEnrollmentRequest, listEnrollmentRequests } from '../desktop.js';

async function fixture(t, options = {}) {
  const dir = await mkdtemp(join(tmpdir(), 'openuuyc-server-'));
  const app = createServer({
    dbPath: join(dir, 'devices.sqlite'),
    host: '127.0.0.1',
    port: 0,
    maintenanceIntervalMs: 50,
    wsHeartbeatIntervalMs: 100,
    requireDeviceApproval: false,
    ...options,
  });
  const address = await app.listen(0, '127.0.0.1');
  t.after(async () => {
    await app.close();
    await rm(dir, { recursive: true, force: true });
  });
  return { app, dir, origin: app.publicOrigin(), address };
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

test('new address-based devices need operator approval for enrollment and remote access', async (t) => {
  const { app, address } = await fixture(t, {
    requireDeviceApproval: true, publicOrigin: 'https://uur.example:8443', desktopClientNetworks: '127.0.0.1/32',
  });
  const origin = `http://${address.address}:${address.port}`;
  const hostToken = randomBytes(32).toString('hex');
  const registerHost = () => api(origin, '/api/v1/device/windows/init', {
    token: hostToken, method: 'POST', body: { name: 'Windows host' },
  });
  assert.equal((await registerHost()).value.code, 4031);
  const [hostRequest] = listEnrollmentRequests(app.db);
  assert.equal(hostRequest.platform, 'windows');
  assert.equal(approveEnrollmentRequest(app.db, hostRequest.id), true);
  const hostId = (await registerHost()).value.data.device_id;
  assert.equal(hostId, hostRequest.id);
  const inventory = () => api(origin, '/api/v1/device/groups/of/my', { token: hostToken });

  const iosToken = randomBytes(32).toString('hex');
  const enroll = (token, platform, id) => api(origin, '/v1/desktop/register', {
    token, method: 'POST', body: { platform, name: 'Pending device', persistent_id: id },
  });
  const ios = (await enroll(iosToken, 'ios')).value;
  assert.match(ios.device_id, /^[a-f0-9]{16}$/u);
  assert.equal(ios.status, 'pending');
  assert.deepEqual((await enroll(iosToken, 'ios')).value, ios);
  assert.equal((await api(origin, '/api/v1/device/ios/init', {
    token: iosToken, method: 'POST', body: { name: 'Pending device' },
  })).value.code, 4031);
  const list = () => api(origin, '/api/v1/device/list', { token: iosToken });
  const joinHost = () => api(origin, `/api/v1/room/join/by_device/${hostId}`, {
    token: iosToken, method: 'POST', body: {},
  });
  assert.equal((await list()).response.status, 401);
  assert.equal((await joinHost()).response.status, 401);
  assert.deepEqual((await inventory()).value.data.mobile_devices, []);
  assert.equal(app.db.prepare('SELECT count(*) AS count FROM devices').get().count, 1);

  const otherToken = randomBytes(32).toString('hex');
  const web = await enroll(otherToken, 'web');
  assert.equal(web.response.status, 202);
  assert.equal((await api(origin, '/v1/sessions', {
    token: otherToken, method: 'POST', body: { deviceId: hostId },
  })).response.status, 401);
  assert.equal(approveEnrollmentRequest(app.db, ios.device_id), true);
  assert.equal((await enroll(iosToken, 'ios')).response.status, 200);
  assert.equal((await list()).response.status, 200);
  assert.equal((await enroll(otherToken, 'web')).response.status, 202);
  assert.deepEqual((await inventory()).value.data.mobile_devices.map(({ device_id }) => device_id), [ios.device_id]);
  assert.equal(approveEnrollmentRequest(app.db, ios.device_id), false);
});

test('loopback administration approves, preapproves and revokes device access', async (t) => {
  const { app, origin } = await fixture(t, { adminPort: 0, requireDeviceApproval: true });
  const local = app.adminOrigin();
  assert.equal(new URL(local).hostname, '127.0.0.1');
  assert.match(await (await fetch(local)).text(), /设备准入管理/u);
  const state = () => api(local, '/api/devices');
  const change = (action, id, source = local) => api(local, `/api/devices/${action}`, {
    method: 'POST', body: { id }, headers: { Origin: source },
  });
  const register = (token) => api(origin, '/v1/desktop/register', {
    token, method: 'POST', body: { platform: 'ios', name: 'Test iPad' },
  });
  const token = randomBytes(32).toString('hex');
  const { device_id: id } = (await register(token)).value;
  assert.equal((await api(origin, '/api/devices')).response.status, 404);
  const wrongHost = await new Promise((resolve, reject) => {
    httpGet(local + '/api/devices', { headers: { Host: 'wrong.example' } }, (response) => {
      response.resume();
      resolve(response.statusCode);
    }).on('error', reject);
  });
  assert.equal(wrongHost, 403);
  assert.equal((await api(local, '/api/devices', { headers: { 'X-Forwarded-For': '127.0.0.1' } })).response.status, 403);
  assert.equal((await api(local, '/api/devices/approve', { method: 'POST', body: { id } })).response.status, 403);
  assert.equal((await change('approve', id, 'https://untrusted.example')).response.status, 403);
  assert.equal((await state()).value.requests[0].id, id);
  assert.equal((await change('delete', id)).response.status, 200);
  assert.deepEqual((await state()).value.requests, []);
  assert.equal((await register(token)).value.status, 'pending');
  assert.equal((await change('approve', id)).response.status, 200);
  assert.equal((await register(token)).response.status, 200);
  assert.equal((await state()).value.devices[0].id, id);

  const replacement = randomBytes(32).toString('hex');
  const replacementId = hashSecret(replacement).slice(0, 16);
  assert.equal((await change('preapprove', replacementId)).response.status, 200);
  assert.equal((await state()).value.grants[0].id, replacementId);
  assert.equal((await change('delete', replacementId)).response.status, 200);
  assert.deepEqual((await state()).value.grants, []);
  assert.equal((await change('preapprove', replacementId)).response.status, 200);
  assert.equal((await register(replacement)).response.status, 200);
  assert.deepEqual((await state()).value.grants, []);
  assert.equal((await change('revoke', id)).response.status, 200);
  assert.equal((await api(origin, '/v1/devices', { token })).response.status, 401);
  const retry = await register(token);
  assert.equal(retry.response.status, 202);
  assert.deepEqual(retry.value, { device_id: id, status: 'pending' });
  assert.equal((await api(origin, '/v1/devices', { token })).response.status, 401);
  assert.deepEqual((await state()).value.devices.map((device) => device.id), [replacementId]);
  assert.equal((await change('approve', id)).response.status, 200);
  assert.equal((await register(token)).value.device_id, id);
  assert.equal((await api(origin, '/v1/devices', { token })).response.status, 200);
  assert.equal((await change('preapprove', id)).response.status, 409);
  assert.deepEqual((await state()).value.requests, []);
});

test('deployment accounts link an enrolled iOS device without changing its Windows inventory identity', async (t) => {
  const { origin, app } = await fixture(t);
  const password = 'test-password-with-12-characters';
  const post = (path, body, cookie) => api(origin, `/v1/account/${path}`, {
    method: 'POST', body, headers: { Origin: origin, ...(cookie && { Cookie: cookie }) },
  });
  const page = await fetch(`${origin}/account`);
  assert.equal(page.status, 200);
  assert.match(await page.text(), /OpenUUYC 账号/);
  const registration = await post('register', { username: 'Test.User', password });
  assert.equal(registration.response.status, 200);
  assert.equal(registration.value.username, 'test.user');
  assert.equal((await post('login', { username: 'test.user', password: 'wrong' })).response.status, 401);
  assert.equal((await post('register', { username: 'test.user', password })).response.status, 409);
  assert.ok(!app.db.prepare('SELECT password_hash FROM accounts').get().password_hash.includes(password));
  const login = await post('login', { username: 'test.user', password });
  assert.equal(login.response.status, 200);
  const cookie = login.response.headers.get('set-cookie').split(';', 1)[0];
  const iosToken = randomBytes(32).toString('hex');
  const persistentId = randomBytes(8).toString('hex');
  const enroll = (token, id) => api(origin, '/v1/desktop/register', {
    method: 'POST', token, body: { name: 'Test iPad', platform: 'ios', persistent_id: id },
  });
  const first = await enroll(iosToken, persistentId);
  assert.equal(first.response.status, 200);
  assert.equal((await enroll(iosToken, persistentId)).value.device_id, first.value.device_id);
  assert.equal((await enroll(iosToken, randomBytes(8).toString('hex'))).response.status, 409);
  assert.equal((await enroll(randomBytes(32).toString('hex'), persistentId)).response.status, 409);
  const windows = await registerAddressDevice(origin, 'windows', 'Windows host');
  const inventory = await api(origin, '/api/v1/device/groups/of/my', { token: windows.token });
  assert.deepEqual(inventory.value.data.mobile_devices.map(({ device_id, alias, platform, controllable }) =>
    [device_id, alias, platform, controllable]), [[persistentId, 'Test iPad', 3, false]]);
  const detail = await api(origin, `/api/v1/device/${persistentId}/detail`, { token: windows.token });
  assert.equal(detail.value.data.alias, 'Test iPad');
  assert.equal((await post('device-code', {})).response.status, 401);
  const grant = await post('device-code', {}, cookie);
  const link = () => api(origin, '/v1/desktop/account', { token: iosToken, method: 'POST', body: { code: grant.value.code } });
  assert.equal((await link()).value.username, 'test.user');
  assert.equal((await link()).response.status, 400);
  const status = () => api(origin, '/v1/account/status', { headers: { Cookie: cookie } });
  assert.equal((await status()).value.devices[0].device_id, persistentId);
  const replacement = randomBytes(32).toString('hex');
  const provisionalId = randomBytes(8).toString('hex');
  const provisional = await enroll(replacement, provisionalId);
  assert.equal(provisional.response.status, 200);
  const videoPath = '/v1/desktop/video-preferences';
  const preferences = { NTES_customVideoConfig: 'e30=' };
  await api(origin, videoPath, { token: iosToken, method: 'PUT', body: { preferences } });
  const issueTransfer = (id, session = cookie) => post('device-transfer-code', { device_id: id }, session);
  assert.equal((await issueTransfer(persistentId, '')).response.status, 401);
  assert.equal((await issueTransfer(provisionalId)).response.status, 404);
  const transfer = (code, token = replacement) => api(origin, '/v1/desktop/account/transfer', {
    token, method: 'POST', body: { code },
  });
  const transferGrant = await issueTransfer(persistentId);
  assert.equal(transferGrant.response.status, 200);
  const supersedingGrant = await issueTransfer(persistentId);
  assert.equal((await transfer(transferGrant.value.code)).response.status, 400);
  assert.equal((await transfer(supersedingGrant.value.code, windows.token)).response.status, 403);
  assert.deepEqual((await transfer(supersedingGrant.value.code)).value, { username: 'test.user', persistent_id: persistentId });
  assert.equal((await transfer(supersedingGrant.value.code)).response.status, 400);
  assert.equal((await enroll(replacement, provisionalId)).value.device_id, first.value.device_id);
  assert.equal((await enroll(iosToken, persistentId)).response.status, 401);
  assert.deepEqual((await api(origin, videoPath, { token: replacement })).value, { preferences });
  assert.equal((await api(origin, videoPath, { token: iosToken })).response.status, 401);
  const migratedInventory = await api(origin, '/api/v1/device/groups/of/my', { token: windows.token });
  assert.deepEqual(migratedInventory.value.data.mobile_devices.map(({ device_id }) => device_id), [persistentId]);
  assert.equal((await post('logout', {}, cookie)).response.status, 200);
  assert.equal((await status()).value.username, null);
  assert.equal((await api(origin, '/v1/desktop/account', { token: replacement })).value.username, 'test.user');
});

test('persists iOS video preferences by device and refreshes the Windows inventory name', async (t) => {
  const { origin, app, dir } = await fixture(t);
  const path = '/v1/desktop/video-preferences';
  const token = randomBytes(32).toString('hex');
  const secondToken = randomBytes(32).toString('hex');
  const windowsToken = randomBytes(32).toString('hex');
  const persistentId = randomBytes(8).toString('hex');
  const enroll = (deviceToken, name, platform = 'ios', id) => api(origin, '/v1/desktop/register', {
    method: 'POST', token: deviceToken, body: { platform, name, persistent_id: id },
  });
  const first = await enroll(token, 'UU 远程 iOS', 'ios', persistentId);
  assert.equal(first.response.status, 200);
  assert.equal((await enroll(secondToken, 'Another iPad')).response.status, 200);
  assert.equal((await enroll(windowsToken, 'Desktop', 'windows')).response.status, 200);
  assert.equal((await api(origin, path)).response.status, 401);
  assert.equal((await api(origin, path, { token: windowsToken })).response.status, 403);
  assert.deepEqual((await api(origin, path, { token })).value, { preferences: null });
  const preferences = { NTES_customVideoConfig: Buffer.from('{"fps":60}').toString('base64') };
  const write = (value) => api(origin, path, { token, method: 'PUT', body: { preferences: value } });
  assert.equal((await write({ unrelated: 'YQ==' })).response.status, 400);
  assert.equal((await write({ NTES_customVideoConfig: Buffer.alloc(8193).toString('base64') })).response.status, 400);
  assert.deepEqual((await write(preferences)).value, { preferences });
  assert.equal((await enroll(token, 'Test iPad', 'ios', persistentId)).value.device_id, first.value.device_id);
  const windows = await api(origin, '/api/v1/device/groups/of/my', { token: windowsToken });
  assert.ok(windows.value.data.mobile_devices.some((device) => device.device_id === persistentId && device.alias === 'Test iPad'));
  assert.deepEqual((await api(origin, path, { token })).value, { preferences });
  assert.deepEqual((await api(origin, path, { token: secondToken })).value, { preferences: null });
  const reopened = openDatabase(join(dir, 'devices.sqlite'));
  try {
    const row = reopened.prepare('SELECT preferences FROM ios_video_preferences WHERE device_id = ?').get(first.value.device_id);
    assert.deepEqual(JSON.parse(row.preferences), preferences);
  } finally {
    reopened.close();
  }
  revokeDevice(app.db, first.value.device_id);
  assert.equal((await api(origin, path, { token })).response.status, 401);
});

test('verifies Passkey registration and discoverable login against the deployment origin', async (t) => {
  const accountOrigin = 'https://remote.example.test:8788';
  const rpId = 'remote.example.test';
  const { origin } = await fixture(t, { accountOrigin });
  const post = (path, body, source = accountOrigin) => api(origin, `/v1/account/passkey/${path}`, {
    method: 'POST', body, headers: { Origin: source },
  });
  const registration = await post('register/options', { username: 'passkey.user' });
  assert.equal(registration.response.status, 200);
  const { privateKey, publicKey } = generateKeyPairSync('ec', { namedCurve: 'prime256v1' });
  const { x, y } = publicKey.export({ format: 'jwk' });
  const coseKey = isoCBOR.encode(new Map([[1, 2], [3, -7], [-1, 1],
    [-2, Buffer.from(x, 'base64url')], [-3, Buffer.from(y, 'base64url')]]));
  const rawId = randomBytes(32);
  const id = rawId.toString('base64url');
  const clientData = (type, challenge, expectedOrigin = accountOrigin) =>
    Buffer.from(JSON.stringify({ type, challenge, origin: expectedOrigin, crossOrigin: false }));
  const authenticatorData = (flags, counter) => {
    const buffer = Buffer.alloc(37);
    createHash('sha256').update(rpId).digest().copy(buffer);
    buffer[32] = flags;
    buffer.writeUInt32BE(counter, 33);
    return buffer;
  };
  const registrationData = Buffer.concat([authenticatorData(0x45, 0), Buffer.alloc(16), Buffer.from([0, rawId.length]), rawId, coseKey]);
  const attestationObject = Buffer.from(isoCBOR.encode(new Map([
    ['fmt', 'none'], ['attStmt', new Map()], ['authData', registrationData],
  ]))).toString('base64url');
  const verifyRegistration = await post('register/verify', {
    requestId: registration.value.requestId,
    credential: { id, rawId: id, type: 'public-key', clientExtensionResults: {}, response: {
      clientDataJSON: clientData('webauthn.create', registration.value.options.challenge).toString('base64url'), attestationObject,
    } },
  });
  assert.equal(verifyRegistration.response.status, 200, JSON.stringify(verifyRegistration.value));
  assert.match(verifyRegistration.response.headers.get('set-cookie'), /Secure/);
  const login = await post('login/options', {});
  const assertion = (clientOrigin) => {
    const data = clientData('webauthn.get', login.value.options.challenge, clientOrigin);
    const auth = authenticatorData(0x05, 1);
    const signature = sign('sha256', Buffer.concat([auth, createHash('sha256').update(data).digest()]), privateKey);
    return { requestId: login.value.requestId, credential: { id, rawId: id, type: 'public-key', clientExtensionResults: {}, response: {
      userHandle: registration.value.options.user.id, authenticatorData: auth.toString('base64url'),
      clientDataJSON: data.toString('base64url'), signature: signature.toString('base64url'),
    } } };
  };
  assert.equal((await post('login/verify', assertion(accountOrigin), 'https://foreign.example.test')).response.status, 403);
  const verifyLogin = await post('login/verify', assertion(accountOrigin));
  assert.equal(verifyLogin.response.status, 200, JSON.stringify(verifyLogin.value));
  assert.equal(verifyLogin.value.username, 'passkey.user');
  assert.equal((await post('login/verify', assertion(accountOrigin))).response.status, 400);
});

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

function connectHost(url, ticket, controlling = false) {
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
      'X-NRD-CONTROLLING': controlling ? '1' : '0',
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

async function registerAddressDevice(origin, platform, name, token = randomBytes(32).toString('hex')) {
  const registration = await api(origin, '/v1/desktop/register', { token, method: 'POST', body: { platform, name } });
  assert.equal(registration.response.status, 200, JSON.stringify(registration.value));
  return { id: registration.value.device_id, token };
}

function emitAck(socket, event, ...args) {
  return new Promise((resolve, reject) => {
    socket.timeout(1500).emit(event, ...args, (error, ...result) => {
      if (error) reject(error);
      else resolve(result);
    });
  });
}

test('native address enrollment, signaling, reconnection, and takeover', async (t) => {
  const iceServers = [{ urls: 'stun:stun.example.test:3478', username: '', credential: '' }];
  const { app, origin } = await fixture(t, { iceServers: JSON.stringify(iceServers) });
  const host = await registerAddressDevice(origin, 'windows', 'Address host');
  const client = await registerAddressDevice(origin, 'windows', 'Address client');
  const replacement = await registerAddressDevice(origin, 'windows', 'Replacement');
  const paired = await pair(app, 'host', 'windows', 'Legacy paired host');
  assert.deepEqual(await registerAddressDevice(origin, 'windows', 'Address host', host.token), host);
  const catalog = (await api(origin, '/v1/desktop/devices', { token: client.token })).value;
  assert.deepEqual(catalog.my_binded_devices.map((device) => device.device_id).sort(), [host.id, replacement.id].sort());
  assert.equal(catalog.current_device.device_id, client.id);
  assert.ok(!catalog.my_binded_devices.some((device) => device.device_id === paired.deviceId));
  const post = (path, token, body = {}) => api(origin, `/v1/desktop/${path}`, { token, method: 'POST', body });
  const join = (controller, force = false) => post('join', controller.token, { device_id: host.id, force_join: force });
  const room = (await post('room', host.token)).value;
  const openSignal = async (ticket, controlling = false) => {
    const socket = connectHost(room.signalingUrl, ticket, controlling);
    t.after(() => socket.close());
    await connected(socket);
    assert.deepEqual(await emitAck(socket, 'room_info'), [{ room_id: host.id }]);
    return socket;
  };
  let hostSocket = await openSignal(room.ticket);
  assert.equal((await join(client)).response.status, 409);
  assert.equal((await post('controllable', host.token, { controllable: true })).response.status, 200);

  const openNativeSession = async (reservation) => {
    assert.equal(reservation.response.status, 200, JSON.stringify(reservation.value));
    const socket = await openSignal(reservation.value.ticket);
    const appControlId = randomUUID();
    const appData = Buffer.from([0, 0x31, 0x04, 0xff]);
    const hostRequest = nextSocketIoEvent(hostSocket, 'be-controlled');
    const [status, control] = await emitAck(socket, 'control', {
      app_control_id: appControlId,
      app_data: appData,
      streamer_data: '{}',
    });
    assert.equal(status, 'success');
    assert.deepEqual(control.iceServers, iceServers);
    const [hostSignal] = await hostRequest;
    assert.equal(hostSignal.app_control_id, appControlId);
    assert.equal(hostSignal.controller_device_id.length, 16);
    assert.equal(hostSignal.ice_id, control.ice_id);
    assert.deepEqual(hostSignal.app_data, appData);
    return { socket, control, appControlId, ticket: reservation.value.ticket };
  };
  const session = await openNativeSession(await join(client));
  assert.equal((await join(replacement)).response.status, 409);
  const sdp = gzipSync(Buffer.from('v=0\r\n'));
  const ids = { app_control_id: session.appControlId, ice_id: session.control.ice_id };
  const offer = nextSocketIoEvent(hostSocket, 'soac');
  session.socket.emit('soac', { data: { ...ids, type: 'offer', gzip_sdp: sdp } });
  assert.deepEqual(await offer, [{ client_id: session.control.client_id, data: { ...ids, type: 'offer', gzip_sdp: sdp } }]);

  const clientClosed = nextSocketIoEvent(session.socket, 'disconnect');
  const serverSawClose = nextSocketIoEvent(app.io.of('/').sockets.get(session.socket.id), 'disconnect');
  session.socket.io.engine.close();
  await Promise.all([clientClosed, serverSawClose]);
  const queuedAnswer = { client_id: session.control.client_id, data: { ...ids, type: 'answer', gzip_sdp: sdp } };
  hostSocket.emit('soac', queuedAnswer);
  await emitAck(hostSocket, 'room_info');
  const restoredClient = connectHost(room.signalingUrl, session.ticket, true);
  t.after(() => restoredClient.close());
  const queuedForClient = nextSocketIoEvent(restoredClient, 'soac');
  await connected(restoredClient);
  assert.deepEqual(await queuedForClient, [queuedAnswer]);
  const clientReconnected = restoredClient;
  const hostClosed = nextSocketIoEvent(hostSocket, 'disconnect');
  hostSocket.io.engine.close();
  await hostClosed;
  hostSocket = await openSignal(room.ticket);
  const answer = nextSocketIoEvent(clientReconnected, 'soac');
  hostSocket.emit('soac', { client_id: session.control.client_id, data: { ...ids, type: 'answer', gzip_sdp: sdp } });
  assert.deepEqual(await answer, [{ client_id: session.control.client_id, data: { ...ids, type: 'answer', gzip_sdp: sdp } }]);

  const released = nextSocketIoEvent(hostSocket, 'released');
  const replaced = nextSocketIoEvent(clientReconnected, 'leave');
  const takeover = await openNativeSession(await join(replacement, true));
  assert.equal((await released)[0].ice_id, session.control.ice_id);
  await replaced;

  const pending = await join(client, true);
  assert.equal(pending.response.status, 200);
  const ended = nextSocketIoEvent(takeover.socket, 'leave');
  assert.equal((await post('controllable', host.token, { controllable: false })).response.status, 200);
  await ended;
  await post('controllable', host.token, { controllable: true });
  const invalidated = connectHost(room.signalingUrl, pending.value.ticket, true);
  t.after(() => invalidated.close());
  await assert.rejects(connected(invalidated), /unauthorized/u);

  revokeDevice(app.db, replacement.id);
  assert.equal((await join(replacement)).response.status, 401);
  assert.equal((await post('register', replacement.token, { platform: 'windows', name: 'Replacement' })).response.status, 401);
});

test('address enrollment authorizes iOS and web controllers only on the trusted network', async (t) => {
  const { origin } = await fixture(t, { desktopClientNetworks: '127.0.0.1/32' });
  const host = await registerAddressDevice(origin, 'windows', 'Address host');
  const ios = await registerAddressDevice(origin, 'ios', 'iPad');
  const web = await registerAddressDevice(origin, 'web', 'Browser');
  for (const controller of [ios, web]) {
    const catalog = await api(origin, '/v1/devices', { token: controller.token });
    assert.deepEqual(catalog.value.devices.map((device) => device.id), [host.id]);
    assert.equal((await api(origin, '/v1/desktop/room', { token: controller.token, method: 'POST', body: {} })).response.status, 403);
  }
  const room = (await api(origin, '/v1/desktop/room', { token: host.token, method: 'POST', body: {} })).value;
  const hostSocket = connectHost(room.signalingUrl, room.ticket);
  t.after(() => hostSocket.close());
  await connected(hostSocket);
  await api(origin, '/v1/desktop/controllable', { token: host.token, method: 'POST', body: { controllable: true } });
  for (const controller of [ios, web]) {
    const reservation = await api(origin, '/v1/sessions', { token: controller.token, method: 'POST', body: { deviceId: host.id } });
    assert.equal(reservation.response.status, 201, JSON.stringify(reservation.value));
    const hostRequest = nextSocketIoEvent(hostSocket, 'be-controlled');
    const socket = await openController(reservation.value.signalingUrl, reservation.value.ticket);
    t.after(() => socket.close());
    const [request] = await hostRequest;
    assert.equal(request.controller_device_id, controller.id);
    if (controller === ios) {
      const left = nextSocketIoEvent(hostSocket, 'left');
      socket.send(JSON.stringify({ type: 'leave' }));
      await left;
    } else {
      const ended = nextWebSocketMessage(socket, (message) => message.type === 'ended');
      const released = nextSocketIoEvent(hostSocket, 'released');
      const disabled = await api(origin, '/v1/desktop/controllable', {
        token: host.token, method: 'POST', body: { controllable: false },
      });
      assert.equal(disabled.response.status, 200);
      assert.equal((await ended).reason, 'host_disabled');
      assert.equal((await released)[0].ice_id, request.ice_id);
      const reconnect = await api(origin, '/v1/sessions', {
        token: controller.token, method: 'POST', body: { deviceId: host.id },
      });
      assert.equal(reconnect.response.status, 409);
    }
  }

  const denied = await fixture(t, { desktopClientNetworks: '192.0.2.0/24' });
  const rejection = await api(denied.origin, '/v1/desktop/register', {
    token: ios.token, method: 'POST', body: { platform: 'ios', name: 'iPad' },
    headers: { 'X-Forwarded-For': '192.0.2.10' },
  });
  assert.equal(rejection.response.status, 403);
});

async function registerWindows(origin, name) {
  const token = randomBytes(32).toString('hex');
  const call = (path, method = 'GET', body) => api(origin, `/api/${path}`, { token, method, body });
  const { response, value } = await call('v1/device/windows/init', 'POST', {
    name, platform: 1, version_name: '4.48.1.5606', os: 'Windows 11', memory: 16384,
  });
  assert.equal(response.status, 200);
  assert.equal(value.code, 0);
  return { token, id: value.data.device_id, call };
}

async function windowsPresence(t, device) {
  const { value } = await device.call('v1/room/create', 'POST', { last_controlled_interval: -1 });
  assert.equal(value.code, 0);
  const room = value.data;
  const socket = connectHost(room.signaling_server, room.token);
  t.after(() => socket.close());
  await connected(socket);
  const enabled = await device.call('v1/device/controllable', 'POST', { controllable: true });
  assert.equal(enabled.value.code, 0);
  return { room, socket };
}

function nextPush(socket, type, predicate = () => true) {
  return new Promise((resolve, reject) => {
    const timeout = setTimeout(() => {
      socket.off('bmsg_push', onPush);
      reject(new Error(`timed out waiting for push ${type}`));
    }, 1500);
    const onPush = (push) => {
      if (push.type !== type || !predicate(push.data)) return;
      clearTimeout(timeout);
      socket.off('bmsg_push', onPush);
      resolve(push.data);
    };
    socket.on('bmsg_push', onPush);
  });
}

test('Windows API contract publishes online status and shares device management changes', async (t) => {
  const { origin } = await fixture(t);
  const controller = await registerWindows(origin, 'Native controller');
  const { socket } = await windowsPresence(t, controller);
  const bound = nextPush(socket, 'device_binded');
  const host = await registerWindows(origin, 'Native host');
  assert.equal((await bound).device_id, host.id);
  const offline = await controller.call(`v1/room/join/by_device/${host.id}`, 'POST', { force_join: false });
  assert.equal(offline.value.code, 2006);

  const online = nextPush(socket, 'device_info_changed', (data) => data.device_id === host.id && data.status === 'CONNECTED');
  const { room } = await windowsPresence(t, host);
  await online;
  const rename = nextPush(socket, 'device_info_changed', (data) => data.alias === 'Renamed host');
  assert.equal((await controller.call(`v1/device/${host.id}`, 'PUT', { alias: 'Renamed host' })).value.code, 0);
  await rename;
  const detail = (await controller.call(`v1/device/${host.id}/detail`)).value;
  assert.ok(detail.data.details.some(([label, value]) => label === '内存' && value === '16.0 GiB'));
  const groups = (await controller.call('v1/device/groups/of/my')).value.data;
  const currentHost = groups.desktop_devices.find((device) => device.device_id === host.id);
  assert.equal(currentHost.alias, 'Renamed host');
  assert.equal(currentHost.version_name, '4.48.1.5606');
  assert.equal(currentHost.controllable, true);
  const rights = (await controller.call('v1/member/entitlements')).value.data;
  assert.equal(rights.features.screen_quality.periods[0].limit.quality, null);
  assert.ok(rights.expired_at > rights.server_time);
  assert.equal((await controller.call(`v1/device/${controller.id}/unbind`, 'POST')).response.status, 400);

  const removed = nextPush(socket, 'device_unbind');
  assert.equal((await controller.call(`v1/device/${host.id}/unbind`, 'POST')).value.code, 0);
  assert.deepEqual(await removed, { device_id: host.id });
  assert.equal((await host.call('v1/user/info')).value.code, 1120);
  assert.equal((await host.call('v1/device/windows/init', 'POST', { name: 'Native host' })).value.code, 1120);
  const revokedSocket = connectHost(room.signaling_server, room.token);
  t.after(() => revokedSocket.close());
  await assert.rejects(connected(revokedSocket), /unauthorized/u);
});

test('Windows assistance binds password, local confirmation, history and refetch to the requesting device', async (t) => {
  const { origin } = await fixture(t);
  const host = await registerWindows(origin, 'Host');
  const controller = await registerWindows(origin, 'Controller');
  const other = await registerWindows(origin, 'Another controller');
  const { socket } = await windowsPresence(t, host);
  const share = (await host.call('v1/device/share/info/refresh', 'POST')).value.data;
  assert.match(share.connect_id, /^\d{9}$/u);
  assert.deepEqual((await host.call('v1/device/share/info/refresh', 'POST')).value.data, share);

  const modePush = nextPush(socket, 'get_control_mode');
  const mode = controller.call('v2/room/share/control_mode', 'POST', share);
  const { control_id } = await modePush;
  const modeReply = { control_id, allow_control: true, control_mode: 'password_confirmation' };
  assert.equal((await other.call('v2/room/share/upload_control_mode', 'POST', modeReply)).response.status, 404);
  assert.equal((await host.call('v2/room/share/upload_control_mode', 'POST', modeReply)).value.code, 0);
  assert.deepEqual((await mode).value.data, { can_remote_control: true, control_mode: 'password_confirmation' });

  const verifyCode = async (code) => {
    const push = nextPush(socket, 'remote_control');
    const join = controller.call('v2/room/join/share/by_code', 'POST', { ...share, connect_code: code });
    const challenge = await push;
    await host.call('v2/room/share/upload_sign', 'POST', {
      control_id: challenge.control_id, can_remote_control: true, control_mode: 'password_confirmation', need_confirmation: true,
      sign: hashSecret(`${challenge.salt}123456`), backup_sign: hashSecret(`${challenge.salt}custom-code`),
    });
    return (await join).value;
  };
  assert.equal((await verifyCode('wrong')).code, 1131);
  const pending = await verifyCode('custom-code');
  assert.equal(pending.code, 1136);
  const confirmation = { ...share, control_id: pending.data.control_id };
  assert.equal((await other.call('v2/room/join/share/by_confirmation', 'POST', confirmation)).value.code, 1133);

  const prompt = nextPush(socket, 'remote_control_by_confirmation');
  const interrupted = controller.call('v2/room/join/share/by_confirmation', 'POST', confirmation);
  assert.equal((await prompt).name, 'Controller');
  const withdrew = nextPush(socket, 'remote_control_by_confirmation_cancel');
  assert.equal((await controller.call('v2/room/share/cancel_remote_assist', 'POST', share)).value.code, 0);
  assert.equal((await withdrew).control_id, confirmation.control_id);
  assert.equal((await interrupted).response.status, 403);
  assert.equal((await host.call('v2/room/share/confirmation', 'POST', { ...confirmation, allow_control: true })).response.status, 404);

  const retry = await verifyCode('123456');
  const approved = nextPush(socket, 'remote_control_by_confirmation');
  const join = controller.call('v2/room/join/share/by_confirmation', 'POST', { ...share, control_id: retry.data.control_id });
  await approved;
  await host.call('v2/room/share/confirmation', 'POST', { control_id: retry.data.control_id, allow_control: true, need_password: true });
  const { value: accepted } = await join;
  assert.equal(accepted.code, 0);
  const viewer = connectHost(accepted.data.signaling_list[0], accepted.data.token);
  t.after(() => viewer.close());
  await connected(viewer);
  const controlled = nextSocketIoEvent(socket, 'be-controlled');
  const [status] = await emitAck(viewer, 'control', { app_control_id: randomUUID(), app_data: Buffer.from([1]), streamer_data: '{}' });
  assert.equal(status, 'success');
  assert.equal((await controlled)[0].connection_type, 2);

  const recent = 'v1/device/share/saved/recent';
  const favorites = 'v1/device/share/saved/favorites';
  assert.equal((await controller.call(recent)).value.data.saved[0].publisher_device_id, host.id);
  assert.equal((await other.call(recent)).value.data.saved.length, 0);
  const saved = (await controller.call(favorites, 'POST', { favorites: [{ ...share, remark: 'My host' }] })).value.data.saved;
  assert.equal(saved[0].remark, 'My host');
  assert.equal((await controller.call(`${favorites}/${host.id}`, 'DELETE')).value.code, 0);
  assert.equal((await controller.call(favorites)).value.data.saved.length, 0);
  assert.equal((await controller.call(recent)).value.data.saved.length, 1);

  const refresh = 'v1/room/join/refetch';
  const token = { token: accepted.data.token };
  assert.equal((await other.call(refresh, 'POST', token)).response.status, 403);
  const refreshPush = nextPush(socket, 'get_control_mode');
  const refreshed = controller.call(refresh, 'POST', token);
  const refreshChallenge = await refreshPush;
  await host.call('v2/room/share/upload_control_mode', 'POST', { ...modeReply, ...refreshChallenge });
  assert.notEqual((await refreshed).value.data.token, accepted.data.token);
  assert.equal((await controller.call(refresh, 'POST', token)).response.status, 403);
});

test('Windows power operations validate the target and require a receipt from its active host room', async (t) => {
  const { origin } = await fixture(t);
  const host = await registerWindows(origin, 'Power target');
  const controller = await registerWindows(origin, 'Controller');
  const { socket: hostSocket } = await windowsPresence(t, host);
  const { socket: helperSocket } = await windowsPresence(t, controller);
  const setting = 'v1/device/set_support_wol';
  const info = 'v1/device/wake_on_lan/info';
  const wake = `v1/device/${host.id}/wake_on_lan/req`;
  const target = { mac: '02:01:02:03:04:05', inner_ip: '192.168.9.5', subnet_mask: '255.255.255.0' };
  assert.equal((await host.call(setting, 'POST', { support_wol: true })).response.status, 409);
  assert.equal((await host.call(info, 'POST', { ...target, subnet_mask: '255.0.255.0' })).response.status, 400);
  assert.equal((await host.call(info, 'POST', target)).value.code, 0);
  assert.equal((await controller.call(info, 'POST', { ...target, mac: '02:01:02:03:04:06', inner_ip: '192.168.9.6' })).value.code, 0);
  assert.equal((await host.call(setting, 'POST', { support_wol: true })).value.code, 0);
  assert.equal((await controller.call(wake, 'POST')).response.status, 409);

  const verifyPower = async (path, type, code, body) => {
    const pushed = nextPush(hostSocket, type);
    const action = controller.call(path, 'POST', body);
    const { call_id, need_response } = await pushed;
    assert.equal(need_response, true);
    assert.equal((await controller.call('v1/transport/rpc/response', 'POST', { call_id, code })).response.status, 404);
    assert.equal((await host.call('v1/transport/rpc/response', 'POST', { call_id, code })).value.code, 0);
    return (await action).response.status;
  };
  assert.equal(await verifyPower(`v1/device/${host.id}/reboot`, 'device_reboot', -1), 403);
  assert.equal(await verifyPower(`v1/transport/${host.id}/rpc/request`, 'transport_rpc', 0, { p: 'shutdown' }), 200);
  assert.equal((await host.call('v1/user/logout', 'POST')).value.code, 0);
  const pushed = nextPush(helperSocket, 'wake_on_lan');
  assert.equal((await controller.call(wake, 'POST')).value.data.current_device_in_same_network, true);
  assert.deepEqual((await pushed).target_device, { ...target, mac: '020102030405' });
});

test('Windows wallpaper upload is bounded, single-use and bound to the authorized device', async (t) => {
  const { origin } = await fixture(t);
  const host = await registerWindows(origin, 'Wallpaper host');
  const controller = await registerWindows(origin, 'Controller');
  const { value: { data: grant } } = await host.call('v1/tool/fp/token?filetype=1');
  const upload = (content) => fetch(grant.req_url, { method: 'POST', headers: { Authorization: grant.token }, body: content });
  const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+a4LsAAAAASUVORK5CYII=', 'base64');
  assert.equal((await upload('not an image')).status, 415);
  const response = await upload(png);
  assert.equal(response.status, 200);
  const image = await response.json();
  assert.equal(image.mime, 'image/png');
  assert.equal((await upload(png)).status, 401);
  assert.equal((await controller.call('v1/device/wallpaper', 'POST', { wallpaper_url: image.url })).response.status, 404);
  assert.equal((await host.call('v1/device/wallpaper', 'POST', { wallpaper_url: image.url })).value.code, 0);
  const list = (await controller.call('v1/device/list')).value.data;
  assert.equal(list.my_binded_devices[0].wallpaper_url, image.url);
  const downloaded = await fetch(image.url);
  assert.deepEqual(Buffer.from(await downloaded.arrayBuffer()), png);
});

test('enrolled iOS controllers can reserve native signaling with host consent', async (t) => {
  const { app, origin } = await fixture(t);
  const host = await registerAddressDevice(origin, 'windows', 'Host');
  const ios = await registerAddressDevice(origin, 'ios', 'iPhone');
  const web = await registerAddressDevice(origin, 'web', 'Browser');
  const reserve = (token, transport = 'socket.io') =>
    api(origin, '/v1/sessions', { method: 'POST', token, body: { deviceId: host.id, transport } });
  const postHost = (path, body) =>
    api(origin, `/v1/desktop/${path}`, { method: 'POST', token: host.token, body });

  const room = (await postHost('room', {})).value;
  const hostSocket = connectHost(room.signalingUrl, room.ticket);
  t.after(() => hostSocket.close());
  await connected(hostSocket);
  assert.equal((await reserve(ios.token)).response.status, 409);
  assert.equal((await postHost('controllable', { controllable: true })).response.status, 200);
  assert.equal((await reserve(web.token)).response.status, 403);
  assert.equal((await reserve(host.token)).response.status, 403);
  assert.equal((await reserve(ios.token, 'unknown')).response.status, 400);

  const abandoned = (await reserve(ios.token)).value;
  const abandonedSocket = connectHost(abandoned.signalingUrl, abandoned.ticket);
  t.after(() => abandonedSocket.close());
  await connected(abandonedSocket);
  const serverSocket = app.io.sockets.sockets.get(abandonedSocket.id);
  const disconnected = new Promise((resolve) => serverSocket.once('disconnect', resolve));
  abandonedSocket.io.engine.close();
  assert.equal(await disconnected, 'transport close');

  const reservation = await reserve(ios.token);
  assert.equal(reservation.response.status, 201);
  assert.equal(new URL(reservation.value.signalingUrl).pathname, '/socket.io/');
  const socket = connectHost(reservation.value.signalingUrl, reservation.value.ticket);
  t.after(() => socket.close());
  await connected(socket);
  assert.deepEqual(await emitAck(socket, 'room_info'), [{ room_id: host.id }]);

  const hostRequest = nextSocketIoEvent(hostSocket, 'be-controlled');
  const appControlId = '296ED92C-3891-4CB9-8ABB-ACB33474A9CB';
  const appData = createConnectOptions(ios.id, 'ios');
  const [status, details] = await emitAck(socket, 'control', {
    app_control_id: appControlId,
    app_data: Buffer.concat([Buffer.from([0x04]), appData]),
    streamer_data: '{}',
  });
  assert.equal(status, 'success');
  const [request] = await hostRequest;
  assert.equal(request.controller_device_id, ios.id);
  assert.equal(request.app_control_id, appControlId);
  assert.deepEqual(request.app_data, appData);
  assert.equal(request.ice_id, details.ice_id);
  const answer = nextSocketIoEvent(socket, 'soac');
  const sdp = gzipSync(Buffer.from('v=0\r\n'));
  const response = {
    client_id: request.client_id,
    data: { app_control_id: appControlId, ice_id: details.ice_id, type: 'answer', gzip_sdp: sdp },
  };
  hostSocket.emit('soac', response);
  assert.deepEqual(await answer, [{ ...response, data: { ...response.data, gzip_sdp: Buffer.concat([Buffer.from([0x04]), sdp]) } }]);
  assert.equal((await reserve(ios.token)).response.status, 409);
  const left = nextSocketIoEvent(hostSocket, 'left');
  socket.emit('leave');
  await left;

  const pending = await reserve(ios.token);
  assert.equal(pending.response.status, 201);
  revokeDevice(app.db, ios.id);
  const revokedSocket = connectHost(pending.value.signalingUrl, pending.value.ticket);
  t.after(() => revokedSocket.close());
  await assert.rejects(connected(revokedSocket), /unauthorized/u);
  assert.equal((await reserve(ios.token)).response.status, 401);
});

test('original iOS bootstrap registers only a local device identity', async (t) => {
  const { app, origin } = await fixture(t);
  const init = '/api/v1/device/ios/init';
  const config = '/api/v1/tool/configures_with_version';
  const token = randomBytes(32).toString('hex');
  const start = (options) => api(origin, init, { method: 'POST', body: { name: 'UU iOS' }, ...options });

  assert.deepEqual((await api(origin, config, { method: 'POST', body: {} })).value, { code: 0, msg: '', data: {} });
  assert.equal((await start()).response.status, 401);
  const first = await start({ token });
  assert.equal(first.response.status, 200);
  assert.match(first.value.data.device_id, /^[a-f0-9]{16}$/u);
  assert.deepEqual(first.value.data, { device_id: first.value.data.device_id, alias: 'UU iOS', status: 'DISCONNECTED' });
  assert.deepEqual((await start({ token })).value, first.value);
  const iosConfig = (await api(origin, config, { token, method: 'POST', body: {} })).value;
  const map = iosConfig.data.controlled_ability_map;
  assert.equal(map.status, 0);
  assert.match(map.version, /^\d+$/u);
  const windows = map.data.find(({ platform }) => platform === 1);
  assert.equal(windows.abilitys.length, 60);
  for (const feature of ['pad_touch', 'voice_setting', 'smart_mouse', 'remote_clipboard_v5', 'file_trans_v4']) {
    assert.ok(windows.abilitys.some((ability) => ability.feature === feature && ability.original_version));
  }
  assert.equal((await api(origin, '/v1/devices', { token })).response.status, 200);
  assert.equal((await api(origin, '/v1/host/room', { method: 'POST', token, body: {} })).response.status, 403);
  revokeDevice(app.db, first.value.data.device_id);
  assert.equal((await start({ token })).response.status, 401);
  assert.deepEqual((await api(origin, config, { token, method: 'POST', body: {} })).value.data, {});

  const rejected = await fixture(t, { desktopClientNetworks: '192.0.2.0/24' });
  assert.equal((await api(rejected.origin, config, { method: 'POST', body: {} })).response.status, 403);
  assert.equal((await api(rejected.origin, init, { method: 'POST', token, body: { name: 'Outside network' } })).response.status, 403);
});

test('Windows API enrollment opens a native host room visible to iOS', async (t) => {
  const { app, origin } = await fixture(t);
  const token = randomBytes(32).toString('hex');
  const initPath = '/api/v1/device/windows/init';
  const body = { name: 'Windows test host', platform: 1, controllable: true };
  const missingCredential = await api(origin, initPath, { method: 'POST', body });
  assert.equal(missingCredential.response.status, 401);
  assert.equal(missingCredential.value.code, 1120);

  const registered = await api(origin, initPath, { token, method: 'POST', body });
  assert.equal(registered.response.status, 200);
  const { device_id: hostId } = registered.value.data;
  assert.match(hostId, /^[a-f0-9]{16}$/u);
  assert.deepEqual((await api(origin, initPath, { token, method: 'POST', body })).value, registered.value);
  const profilePath = '/api/v1/user/info';
  assert.equal((await api(origin, profilePath)).response.status, 401);
  const profile = await api(origin, profilePath, { token });
  assert.equal(profile.response.status, 200);
  assert.deepEqual(profile.value, { code: 0, msg: '', data: { nickname: '自建服务器' } });
  const hostList = await api(origin, '/api/v1/device/list', { token });
  assert.equal(hostList.value.data.current_device.device_id, hostId);
  assert.equal(hostList.value.data.current_device.controllable, false);
  assert.deepEqual(hostList.value.data.my_binded_devices, []);

  const ios = await registerAddressDevice(origin, 'ios', 'iPad');
  const createRoom = (credential) => api(origin, '/api/v1/room/create', {
    token: credential, method: 'POST', body: { last_controlled_interval: -1 },
  });
  assert.equal((await createRoom(ios.token)).response.status, 409);
  const { value: { data: room } } = await createRoom(token);
  const socket = connectHost(room.signaling_server, room.token);
  t.after(() => socket.close());
  await connected(socket);
  assert.equal((await api(origin, '/api/v1/device/controllable', {
    token, method: 'POST', body: { controllable: true },
  })).value.code, 0);
  const catalog = await api(origin, '/api/v1/device/list', { token: ios.token });
  assert.deepEqual(catalog.value.data.my_binded_devices.map(({ device_id, status, controllable }) =>
    [device_id, status, controllable]), [[hostId, 'CONNECTED', true]]);

  revokeDevice(app.db, hostId);
  assert.equal((await api(origin, initPath, { token, method: 'POST', body })).response.status, 401);
  assert.equal((await api(origin, profilePath, { token })).response.status, 401);
});

test('original iOS device directory and join use the admitted devices and native room', async (t) => {
  const { address, app } = await fixture(t, {
    publicOrigin: 'https://uur.example:8443',
    desktopClientNetworks: '127.0.0.1/32',
  });
  const origin = `http://${address.address}:${address.port}`;
  const host = await registerWindows(origin, 'Online host');
  const ios = await registerAddressDevice(origin, 'ios', 'iPhone');
  const secondIos = await registerAddressDevice(origin, 'ios', 'iPad');
  const web = await registerAddressDevice(origin, 'web', 'Browser');
  const paired = createPairingCode(app.db, { role: 'host' });
  const privateHost = await api(origin, '/v1/pair', {
    method: 'POST', body: { role: 'host', platform: 'windows', name: 'Paired host', code: paired.code },
  });
  assert.equal(privateHost.response.status, 201);

  const catalog = (token) => api(origin, '/api/v1/device/list', { token });
  const joinRoom = (token, id = host.id) => api(origin, `/api/v1/room/join/by_device/${id}`, {
    method: 'POST', token, body: {},
  });
  const { value: room } = await api(origin, '/v1/desktop/room', { token: host.token, method: 'POST', body: {} });
  const listenerSocketUrl = new URL('/socket.io/', origin);
  const hostSocket = connectHost(listenerSocketUrl, room.ticket);
  t.after(() => hostSocket.close());
  await connected(hostSocket);

  const createPresence = (token) => api(origin, '/api/v1/room/create', { token, method: 'POST', body: {} });
  assert.equal((await createPresence()).response.status, 401);
  assert.equal((await createPresence(web.token)).response.status, 403);
  const presence = await createPresence(ios.token);
  assert.equal(presence.response.status, 200);
  assert.deepEqual(presence.value.data.signaling_list, ['wss://uur.example:8443/socket.io/']);
  const presenceTicket = presence.value.data.token;
  const invalidController = connectHost(listenerSocketUrl, presenceTicket, true);
  t.after(() => invalidController.close());
  await assert.rejects(connected(invalidController), /unauthorized/u);
  const presenceSocket = connectHost(listenerSocketUrl, presenceTicket);
  t.after(() => presenceSocket.close());
  await connected(presenceSocket);
  assert.deepEqual(await emitAck(presenceSocket, 'room_info'), [{ room_id: ios.id }]);

  assert.equal((await catalog(web.token)).response.status, 403);
  assert.equal((await catalog()).response.status, 401);
  assert.equal((await joinRoom(web.token)).response.status, 403);
  assert.equal((await joinRoom(ios.token, privateHost.value.deviceId)).response.status, 403);
  assert.equal((await joinRoom(ios.token)).response.status, 409);
  assert.equal((await api(origin, '/v1/desktop/controllable', {
    token: host.token, method: 'POST', body: { controllable: true },
  })).response.status, 200);

  const list = await catalog(ios.token);
  assert.equal(list.response.status, 200);
  assert.equal(list.value.data.current_device.device_id, ios.id);
  assert.deepEqual(list.value.data.my_binded_devices.map(({ device_id, status, controllable }) =>
    [device_id, status, controllable]), [[host.id, 'CONNECTED', true]]);
  assert.deepEqual(list.value.data.others_shared_devices, []);

  const groups = (token) => api(origin, '/api/v1/device/groups/of/my', { token });
  assert.equal((await groups()).response.status, 401);
  assert.equal((await groups(web.token)).response.status, 403);
  const grouped = await groups(ios.token);
  assert.equal(grouped.response.status, 200);
  assert.equal(grouped.value.data.current_device_id, ios.id);
  assert.deepEqual(grouped.value.data.desktop_devices.map(({ device_id }) => device_id), [host.id]);
  assert.deepEqual(grouped.value.data.mobile_devices, []);
  assert.deepEqual(grouped.value.data.tv_devices, []);

  const detail = (token, id = host.id) => api(origin, `/api/v1/device/${id}/detail`, { token });
  assert.equal((await detail()).response.status, 401);
  assert.equal((await detail(web.token)).response.status, 403);
  assert.equal((await detail(ios.token, privateHost.value.deviceId)).response.status, 404);
  const inspected = await detail(ios.token);
  assert.equal(inspected.response.status, 200);
  assert.deepEqual(inspected.value, {
    code: 0, msg: 'ok', data: {
      alias: 'Online host', os_version: '11',
      details: [['设备名称', 'Online host'], ['平台', 'Windows'], ['设备 ID', host.id], ['系统', 'Windows 11'], ['内存', '16.0 GiB']],
    },
  });

  const joined = await joinRoom(ios.token);
  assert.equal(joined.response.status, 200);
  assert.equal(joined.value.code, 0);
  const { signaling_server: signaling, signaling_list: destinations, token } = joined.value.data;
  assert.equal(signaling, 'wss://uur.example:8443/socket.io/');
  assert.deepEqual(destinations, [signaling]);
  assert.equal(joined.value.data.publisher_device_id, host.id);
  assert.equal(joined.value.data.device_name, 'Online host');
  assert.equal(joined.value.data.publisher_platform, 1);
  assert.equal(joined.value.data.publisher_version_name, '4.48.1.5606');
  const retry = await joinRoom(ios.token);
  assert.equal(retry.response.status, 200);
  assert.deepEqual(retry.value.data, joined.value.data);
  assert.equal((await joinRoom(secondIos.token)).response.status, 409);
  const controllerSocket = connectHost(listenerSocketUrl, token);
  t.after(() => controllerSocket.close());
  await connected(controllerSocket);
  assert.deepEqual(await emitAck(controllerSocket, 'room_info'), [{ room_id: host.id }]);
  const [status] = await emitAck(controllerSocket, 'control', {
    app_control_id: randomUUID(),
    app_data: createConnectOptions(ios.id, 'ios'),
    streamer_data: '{}',
  });
  assert.equal(status, 'success');
  assert.equal((await joinRoom(ios.token)).response.status, 409);
  assert.equal((await joinRoom(secondIos.token)).response.status, 409);
});

test('owner QR login waits for approval and exchanges distinct tickets into a revocable device session', async (t) => {
  const dir = await mkdtemp(join(tmpdir(), 'openuuyc-qr-'));
  const db = openDatabase(join(dir, 'accounts.sqlite'));
  const owner = randomUUID();
  const cookie = randomBytes(32).toString('base64url');
  db.prepare('INSERT INTO accounts VALUES (?, ?, ?, ?)').run(owner, 'owner', null, Date.now());
  db.prepare('INSERT INTO account_sessions VALUES (?, ?, ?)').run(hashSecret(cookie), owner, Date.now() + 60_000);
  const app = createServer({ db, port: 0, adminPort: 0, deviceAccountOwner: 'owner', qrPollMs: 20 });
  t.after(async () => {
    await app.close();
    await rm(dir, { recursive: true, force: true });
  });
  await app.listen();
  const origin = app.publicOrigin();
  const admin = app.adminOrigin();
  const installation = randomBytes(32).toString('hex');
  const otherInstallation = randomBytes(32).toString('hex');
  const native = (path, token, method = 'POST', body) => api(origin, '/api/v1/' + path, { token, method, body });
  const adminAction = (action, id) => api(admin, '/api/devices/' + action, {
    method: 'POST', headers: { Origin: admin }, body: { id },
  });
  const register = (token) => native('device/windows/init', token, 'POST', { name: 'Test Windows' });
  const id = (await register(installation)).value.data.device_id;
  assert.equal((await register(installation)).value.data.device_id, id);
  await register(otherInstallation);
  assert.equal((await native('device/list', installation, 'GET')).value.code, 1001);

  const barcode = (await native('qrcode/gen/login', installation)).value.data;
  const { qrcode_jump_url, token: exchangeTicket, status_query_ticket } = barcode;
  const qr = new URL(qrcode_jump_url).searchParams.get('qr');
  assert.notEqual(qr, exchangeTicket);
  assert.notEqual(qr, status_query_ticket);
  assert.notEqual(exchangeTicket, status_query_ticket);
  const status = (login_status, token = status_query_ticket) => native('qrcode/login/status', installation, 'POST',
    { qrcode_jump_url, login_status, token });
  const exchange = (token = installation, ticket = exchangeTicket) => native('login/by_qrcode', token, 'POST',
    { qrcode_jump_url, token: ticket });
  const action = (action, headers) => api(origin, '/v1/account/qr', {
    method: 'POST', headers: { Origin: origin, ...headers }, body: { id: qr, action },
  });
  assert.equal((await status(1)).value.data.login_status, 1);
  assert.equal((await status(1, exchangeTicket)).response.status, 403);
  assert.equal((await action('confirm')).response.status, 401);
  assert.equal((await action('confirm', { Origin: 'https://other.example', Cookie: 'openuuyc_account=' + cookie })).response.status, 403);
  const headers = { Cookie: 'openuuyc_account=' + cookie };
  assert.equal((await action('scan', headers)).value.login_status, 2);
  assert.equal((await action('confirm', headers)).value.login_status, 2);
  assert.equal((await status(1)).value.data.login_status, 2);
  assert.equal((await exchange()).response.status, 403);
  const waiting = status(2);
  assert.equal((await adminAction('approve', id)).response.status, 200);
  assert.equal((await waiting).value.data.login_status, 4);
  assert.equal((await register(installation)).value.data.device_id, id);
  assert.equal((await exchange(otherInstallation)).response.status, 403);
  assert.equal((await exchange(installation, status_query_ticket)).response.status, 403);
  const login = (await exchange()).value.data;
  assert.equal(login.user_id, owner);
  assert.deepEqual((await exchange()).value.data, login);
  assert.equal((await native('user/info', login.token, 'GET')).value.data.nickname, 'owner');
  const list = (await native('device/groups/of/my', login.token, 'GET')).value.data;
  assert.deepEqual(list.desktop_devices.map((device) => device.device_id), [id]);
  assert.equal((await native('device/list', installation, 'GET')).value.code, 1001);

  const scan = (barcode) => native('qrcode/scanned', login.token, 'POST', { qrcode_jump_url: barcode.qrcode_jump_url });
  const nativeAction = (barcode, action) => native('qrcode/login/action', login.token, 'POST',
    { qrcode_jump_url: barcode.qrcode_jump_url, action });
  const canceled = (await native('qrcode/gen/login', otherInstallation)).value.data;
  assert.equal((await scan(canceled)).value.data.login_status, 2);
  assert.equal((await nativeAction(canceled, 'cancel')).value.data.login_status, 3);
  const next = (await native('qrcode/gen/login', otherInstallation)).value.data;
  assert.equal((await nativeAction(next, 'confirm')).value.data.login_status, 2);
  const otherId = (await register(otherInstallation)).value.data.device_id;
  assert.equal((await adminAction('approve', otherId)).response.status, 200);
  await register(otherInstallation);
  const other = (await native('login/by_qrcode', otherInstallation, 'POST',
    { qrcode_jump_url: next.qrcode_jump_url, token: next.token })).value.data;
  const room = (await native('room/create', login.token, 'POST', {})).value.data;
  const socket = connectHost(room.signaling_server, room.token);
  t.after(() => socket.close());
  await connected(socket);
  const disconnected = new Promise((resolve) => socket.once('disconnect', resolve));
  assert.equal((await native('user/logout', login.token)).value.code, 0);
  await disconnected;
  assert.equal((await native('user/info', login.token, 'GET')).value.code, 1001);
  assert.equal((await exchange()).response.status, 403);
  const inventory = () => native('device/groups/of/my', other.token, 'GET');
  const offline = (await inventory()).value.data.desktop_devices.find((device) => device.device_id === id);
  assert.equal(offline.status, 'DISCONNECTED');
  assert.equal((await adminAction('revoke', id)).response.status, 200);
  assert.deepEqual((await inventory()).value.data.desktop_devices.map((device) => device.device_id), [otherId]);
  const staleSocket = connectHost(room.signaling_server, room.token);
  t.after(() => staleSocket.close());
  await assert.rejects(connected(staleSocket), /unauthorized/);
});
