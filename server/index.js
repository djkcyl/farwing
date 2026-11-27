import { createServer as createHttpServer } from 'node:http';
import { createServer as createHttpsServer } from 'node:https';
import { readFileSync } from 'node:fs';
import { realpathSync, statSync, createReadStream, existsSync } from 'node:fs';
import { resolve, extname, sep } from 'node:path';
import { randomUUID } from 'node:crypto';
import { Server as SocketIoServer } from 'socket.io';
import { WebSocketServer, WebSocket } from 'ws';
import { createAccounts } from './accounts.js';
import { DEVICE_LOGIN_APPROVAL_LIFETIME, createDeviceLogin } from './device-login.js';
import { createAdmin } from './admin.js';
import { createAssistance } from './assistance.js';
import { createPower } from './power.js';
import { createWallpapers } from './wallpaper.js';
import { DatabaseSync, onShutdown, runWithSqliteFlag, SQLITE_UNAVAILABLE } from './sqlite-runtime.js';
import {
  DEFAULT_DB_PATH,
  hashSecret,
  openDatabase,
  randomDeviceId,
  randomSecret,
  revokeDevice,
} from './db.js';
import { ADDRESS_TOKEN, addressHost, clientNetworkPolicy, enrollAddressDevice, hostedIceServers, nativeDesktopPath, windowsHardwareDetails } from './desktop.js';
import {
  createConnectOptions,
  createStreamerData,
  decompressHostSdp,
  isRecord,
  validateCandidate,
  validateSdp,
} from './protocol.js';

const SESSION_PATH = '/v1/session';
const SOCKET_IO_PATH = '/socket.io/';
const HOST_ROOM_TTL_MS = 24 * 60 * 60_000;
const SESSION_TICKET_TTL_MS = 60_000;
const HOST_RECONNECT_GRACE_MS = 15_000;
const HTTP_BODY_LIMIT = 32 * 1024;
const WS_MESSAGE_LIMIT = 512 * 1024;
const ACTIVE_CHECK_MS = 500;
const MAX_QUEUED_SIGNALS = 64;
// Windows row of the official 4.48.1 controlled_ability_map response.
const IOS_WINDOWS_ABILITIES = JSON.parse(readFileSync(new URL('./ios-windows-abilities.json', import.meta.url), 'utf8'));
const IOS_VIDEO_KEYS = new Set(['NTES_customVideoConfig', 'NTES_deviceCustomVideoConfig']);
const IOS_VIDEO_VALUE_LIMIT = 8 * 1024;

class HttpError extends Error {
  constructor(status, message, code = status === 401 ? 1120 : status) {
    super(message);
    this.status = status;
    this.code = code;
  }
}

function fail(status, message, code) {
  throw new HttpError(status, message, code);
}

function isoTime(timestamp) {
  return new Date(timestamp).toISOString();
}

function normalizeName(value) {
  if (typeof value !== 'string') fail(400, 'name must be a string');
  const name = value.trim();
  if (!name || Buffer.byteLength(name, 'utf8') > 240 || /[\u0000-\u001f\u007f]/u.test(name)) {
    fail(400, 'name must contain 1 to 80 printable characters');
  }
  if ([...name].length > 80) fail(400, 'name must contain 1 to 80 printable characters');
  return name;
}

function validatePublicOrigin(value) {
  const url = new URL(value);
  if (!['http:', 'https:'].includes(url.protocol) || !url.hostname || url.username || url.password
    || (url.pathname !== '/' && url.pathname !== '') || url.search || url.hash) {
    throw new Error('PUBLIC_ORIGIN must be an HTTP(S) origin without a path, query, or fragment');
  }
  const hostname = url.hostname.replace(/^\[|\]$/gu, '').toLowerCase();
  if (url.protocol === 'http:' && !['127.0.0.1', '::1', 'localhost'].includes(hostname)) {
    throw new Error('PUBLIC_ORIGIN must use HTTPS outside loopback');
  }
  return url.origin;
}

function originIsAllowed(origin, getPublicOrigin) {
  if (!origin) return true;
  try {
    const parsed = new URL(origin);
    return parsed.origin === getPublicOrigin() && origin === parsed.origin;
  } catch {
    return false;
  }
}

function jsonResponse(res, status, body, headOnly = false) {
  const output = Buffer.from(JSON.stringify(body));
  res.writeHead(status, {
    'Content-Type': 'application/json; charset=utf-8',
    'Content-Length': output.length,
    'Cache-Control': 'no-store',
    'X-Content-Type-Options': 'nosniff',
    'Referrer-Policy': 'no-referrer',
  });
  res.end(headOnly ? undefined : output);
}

async function readJson(req) {
  const length = Number(req.headers['content-length']);
  if (Number.isFinite(length) && length > HTTP_BODY_LIMIT) fail(413, 'request body is too large');
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > HTTP_BODY_LIMIT) fail(413, 'request body is too large');
    chunks.push(chunk);
  }
  if (size === 0) return {};
  const contentType = String(req.headers['content-type'] || '').split(';', 1)[0].trim().toLowerCase();
  if (contentType !== 'application/json') fail(415, 'content-type must be application/json');
  let value;
  try {
    value = JSON.parse(Buffer.concat(chunks).toString('utf8'));
  } catch {
    fail(400, 'request body is not valid JSON');
  }
  if (!isRecord(value)) fail(400, 'request body must be a JSON object');
  return value;
}

function bearerToken(req) {
  const header = req.headers.authorization;
  if (typeof header !== 'string') fail(401, 'device authentication required');
  const match = /^Bearer ([A-Za-z0-9_-]{32,128})$/u.exec(header);
  if (!match) fail(401, 'device authentication required');
  return match[1];
}

function requestPath(req) {
  try {
    return new URL(req.url, 'http://localhost').pathname;
  } catch {
    fail(400, 'invalid request path');
  }
}

function mimeType(extension) {
  return ({
    '.html': 'text/html; charset=utf-8',
    '.js': 'text/javascript; charset=utf-8',
    '.mjs': 'text/javascript; charset=utf-8',
    '.css': 'text/css; charset=utf-8',
    '.json': 'application/json; charset=utf-8',
    '.svg': 'image/svg+xml',
    '.png': 'image/png',
    '.jpg': 'image/jpeg',
    '.jpeg': 'image/jpeg',
    '.webp': 'image/webp',
    '.ico': 'image/x-icon',
    '.woff': 'font/woff',
    '.woff2': 'font/woff2',
    '.map': 'application/json; charset=utf-8',
  })[extension.toLowerCase()] || 'application/octet-stream';
}

function staticCandidate(root, rawPath, accept) {
  if (!root) return null;
  let pathname;
  try {
    pathname = decodeURIComponent(rawPath);
  } catch {
    fail(400, 'invalid static file path');
  }
  if (pathname.includes('\0') || pathname.includes('\\')) fail(400, 'invalid static file path');
  if (pathname.split('/').some((part) => part.startsWith('.'))) fail(404, 'not found');
  const safeRoot = resolve(root);
  let candidate = resolve(safeRoot, `.${pathname}`);
  if (candidate !== safeRoot && !candidate.startsWith(`${safeRoot}${sep}`)) fail(404, 'not found');
  if (pathname === '/' || (!extname(candidate) && String(accept || '').includes('text/html'))) {
    candidate = resolve(safeRoot, 'index.html');
  }
  let rootReal;
  let candidateReal;
  try {
    rootReal = realpathSync(safeRoot);
    candidateReal = realpathSync(candidate);
  } catch {
    return null;
  }
  if (candidateReal !== rootReal && !candidateReal.startsWith(`${rootReal}${sep}`)) fail(404, 'not found');
  try {
    const stat = statSync(candidateReal);
    if (!stat.isFile()) return null;
    return { file: candidateReal, size: stat.size };
  } catch {
    return null;
  }
}

function makeRoomUrl(origin) {
  const url = new URL(SOCKET_IO_PATH, origin);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
}

function makeSessionUrl(origin) {
  const url = new URL(SESSION_PATH, origin);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  return url.toString();
}

function sendSocketJson(socket, payload) {
  if (!socket || socket.readyState !== WebSocket.OPEN) return false;
  try {
    socket.send(JSON.stringify(payload));
    return true;
  } catch {
    return false;
  }
}

function emitHostRelease(room, session, event = 'released') {
  if (!room?.socket?.connected) return;
  room.socket.emit(event, {
    client_id: session.clientId,
    ice_id: session.iceId,
    app_control_id: session.appControlId,
  });
}

/** Start the HTTP, native Socket.IO, and controller WebSocket endpoints. */
export function createServer(options = {}) {
  const host = options.host || process.env.HOST || '127.0.0.1';
  const port = Number(options.port ?? process.env.PORT ?? 8787);
  const adminPort = options.adminPort ?? (process.env.ADMIN_PORT === undefined ? null : Number(process.env.ADMIN_PORT));
  const dbPath = options.dbPath || process.env.OPENUUYC_DB_PATH || DEFAULT_DB_PATH;
  const db = options.db || openDatabase(dbPath);
  const tlsCertPath = options.tlsCert || process.env.TLS_CERT_FILE;
  const tlsKeyPath = options.tlsKey || process.env.TLS_KEY_FILE;
  if (Boolean(tlsCertPath) !== Boolean(tlsKeyPath)) throw new Error('TLS_CERT_FILE and TLS_KEY_FILE must be configured together');
  const tlsOptions = tlsCertPath ? {
    cert: readFileSync(tlsCertPath),
    key: readFileSync(tlsKeyPath),
  } : null;
  const suppliedOrigin = options.publicOrigin ?? process.env.PUBLIC_ORIGIN;
  const publicOriginOption = suppliedOrigin ? validatePublicOrigin(suppliedOrigin) : null;
  const accountOrigin = options.accountOrigin ?? process.env.ACCOUNT_PUBLIC_ORIGIN;
  const accountOriginOption = accountOrigin ? validatePublicOrigin(accountOrigin) : null;
  if (accountOriginOption && !accountOriginOption.startsWith('https:')) throw new Error('ACCOUNT_PUBLIC_ORIGIN requires HTTPS');
  const normalizedBindHost = host.replace(/^\[|\]$/gu, '').toLowerCase();
  const loopbackHost = ['127.0.0.1', '::1', 'localhost'].includes(normalizedBindHost);
  if (!tlsOptions && !loopbackHost) {
    throw new Error('plaintext HTTP may bind only to loopback; use TLS or a loopback reverse proxy');
  }
  if (tlsOptions && publicOriginOption?.startsWith('http:')) {
    throw new Error('PUBLIC_ORIGIN must use HTTPS when TLS is enabled');
  }
  const memberNetwork = clientNetworkPolicy(options.desktopClientNetworks ?? process.env.DESKTOP_CLIENT_NETWORKS);
  const ownerName = options.deviceAccountOwner ?? process.env.DEVICE_ACCOUNT_OWNER;
  const owner = ownerName ? db.prepare('SELECT id, username FROM accounts WHERE username = ?').get(ownerName) : null;
  if (ownerName && !owner) throw new Error('DEVICE_ACCOUNT_OWNER must name an existing account');
  const sessionMs = Number(options.deviceAccountSessionMs ?? process.env.DEVICE_ACCOUNT_SESSION_MS ?? 30 * 24 * 60 * 60_000);
  if (owner && (!Number.isSafeInteger(sessionMs) || sessionMs <= 0)) throw new Error('Invalid DEVICE_ACCOUNT_SESSION_MS');
  const requireDeviceApproval = Boolean(owner) || (options.requireDeviceApproval ?? true);
  const iceServers = hostedIceServers(options.iceServers ?? process.env.SELF_HOST_ICE_SERVERS);
  const canSwitchNetwork = iceServers.some(({ urls }) => /^turns?:/u.test(urls));
  function addressClientsAllowed(request) {
    const address = request.socket.remoteAddress;
    if (memberNetwork) return memberNetwork(address);
    const localOrigin = !publicOriginOption || ['127.0.0.1', '[::1]', 'localhost'].includes(new URL(publicOriginOption).hostname);
    return loopbackHost && localOrigin && ['127.0.0.1', '::1', '::ffff:127.0.0.1'].includes(address);
  }
  const staticRootOption = options.staticRoot ?? process.env.WEB_ROOT ?? resolve(import.meta.dirname, '../web');
  const staticRoot = existsSync(staticRootOption) ? resolve(staticRootOption) : null;
  const hostRooms = new Map();
  const sessions = new Map();
  const activeSessionsByHost = new Map();
  const announcedDevices = new Set();
  const pairAttempts = new Map();
  const now = options.now || Date.now;
  let closed = false;
  let boundPort = port;
  const getPublicOrigin = () => publicOriginOption || `${tlsOptions ? 'https' : 'http'}://127.0.0.1:${boundPort}`;
  const deviceLogin = owner ? createDeviceLogin({ db, owner, now, fail, sessionMs,
    portalOrigin: () => accountOriginOption ?? getPublicOrigin(), pollMs: options.qrPollMs }) : null;
  const accounts = createAccounts({ db, now, fail, readJson, jsonResponse, getPublicOrigin,
    configuredOrigin: accountOriginOption, announceDevice, deviceLogin });
  const onlineRoomId = (id) => roomIsOnline(id) ? hostRooms.get(id).ticketId : null;
  const assistance = createAssistance({
    db, now, fail, pushTo, reserveSession, roomId: onlineRoomId,
    signalingUrl: () => makeRoomUrl(getPublicOrigin()),
  });
  const power = createPower({
    db, fail, pushTo, announceDevice, roomId: onlineRoomId,
    hostAllowed: accountDeviceVisible,
    connectedPeers: function* () {
      for (const [id, room] of hostRooms) {
        if (room.addressMember && !room.presenceOnly && room.socket?.connected) yield [id, room.socket.request.socket.remoteAddress];
      }
    },
  });
  const wallpapers = createWallpapers({ db, now, fail, origin: getPublicOrigin, announceDevice });

  const server = tlsOptions
    ? createHttpsServer(tlsOptions, requestHandler)
    : createHttpServer(requestHandler);
  server.headersTimeout = 10_000;
  server.requestTimeout = 15_000;
  server.keepAliveTimeout = 5_000;

  const io = new SocketIoServer(server, {
    path: SOCKET_IO_PATH,
    serveClient: false,
    maxHttpBufferSize: 2 * 1024 * 1024,
    pingInterval: 25_000,
    pingTimeout: 60_000,
    perMessageDeflate: false,
    cors: false,
    allowRequest(request, callback) {
      callback(null, originIsAllowed(request.headers.origin, getPublicOrigin));
    },
  });
  const controllerSockets = new WebSocketServer({
    noServer: true,
    maxPayload: WS_MESSAGE_LIMIT,
    perMessageDeflate: false,
    clientTracking: true,
  });

  function lookupDeviceByToken(token) {
    return db.prepare(`SELECT id, role, platform, name, bound_host_id AS boundHostId,
        controllable, revoked_at AS revokedAt, id IN (SELECT device_id FROM address_members) AS addressMember
      FROM devices WHERE token_hash = ?`).get(hashSecret(token));
  }

  function accountDeviceVisible(id) {
    return !deviceLogin || Boolean(deviceLogin.ownsDevice(id));
  }

  function accountDeviceAllowed(id) {
    return !deviceLogin || Boolean(deviceLogin.accountDevice(id));
  }

  function authenticateHttp(req, role = null) {
    const token = bearerToken(req);
    const device = deviceLogin ? deviceLogin.authenticate(token) : lookupDeviceByToken(token);
    if (!device) fail(401, 'device authentication failed', deviceLogin ? 1001 : 401);
    if (device.revokedAt !== null) fail(401, 'device authentication failed', 1120);
    if (device.addressMember && !addressClientsAllowed(req)) fail(403, 'device network is not authorized');
    if (role && device.role !== role) fail(403, 'device role is not allowed for this endpoint');
    return device;
  }

  function isEnrolledIos(device) {
    return device.addressMember && device.role === 'controller' && device.platform === 'ios';
  }

  function roomTicketFor(deviceId) {
    return db.prepare(`SELECT id, expires_at AS expiresAt, retired_at AS retiredAt
      FROM room_tickets WHERE device_id = ? ORDER BY created_at DESC LIMIT 1`).get(deviceId);
  }

  function roomIsOnline(deviceId) {
    const room = hostRooms.get(deviceId);
    return Boolean(room?.socket?.connected);
  }

  function controllerIsOnline(deviceId) {
    for (const session of sessions.values()) {
      if (session.controllerId === deviceId && session.controllerSocket?.readyState === WebSocket.OPEN) return true;
    }
    return false;
  }

  function controllerCanAccess(controller, hostId) {
    return accountDeviceAllowed(controller.id) && accountDeviceAllowed(hostId)
      && (controller.addressMember ? Boolean(addressHost(db, hostId)) : controller.boundHostId === hostId);
  }

  function nativeControllerIsEnrolled(session) {
    if (!accountDeviceAllowed(session.controllerId) || !accountDeviceAllowed(session.hostId)) return false;
    if (session.controllerPlatform !== 'ios') return Boolean(addressHost(db, session.controllerId));
    return Boolean(db.prepare(`SELECT 1 FROM address_members a JOIN devices d ON d.id = a.device_id
      WHERE d.id = ? AND d.role = 'controller' AND d.platform = 'ios' AND d.revoked_at IS NULL`).get(session.controllerId));
  }

  function desktopHosts() {
    return db.prepare(`SELECT d.id, d.name, d.role, d.controllable, s.version_name AS versionName,
        s.support_wol AS supportWol, s.wallpaper_id AS wallpaperId
      FROM address_members a JOIN devices d ON a.device_id = d.id
      LEFT JOIN desktop_devices s ON s.device_id = d.id
      WHERE d.role = 'host' AND d.revoked_at IS NULL ORDER BY d.name, d.id`).all().filter((row) => accountDeviceVisible(row.id));
  }

  function desktopDevice(row) {
    return {
      device_id: row.id,
      alias: row.name,
      status: roomIsOnline(row.id) ? 'CONNECTED' : 'DISCONNECTED',
      platform: 1,
      controllable: Boolean(row.controllable),
      controlled_support: true,
      support_wol: Boolean(row.supportWol),
      version_name: row.versionName ?? '',
      wallpaper_url: row.wallpaperId ? wallpapers.url(row.wallpaperId) : '',
      participants_info: activeSessionsByHost.get(row.id)?.usedAt != null ? [{}] : [],
    };
  }

  function mobileDevice(row) {
    return {
      device_id: row.persistentId ?? row.id,
      alias: row.name,
      status: [...activeSessionsByHost.values()].some((session) => session.controllerId === row.id
        && session.usedAt !== null && session.nativeSocket?.connected) ? 'CONNECTED' : 'DISCONNECTED',
      platform: 3,
      controllable: false,
      controlled_support: false,
      version_name: '',
      participants_info: [],
    };
  }

  function mobileDevices(id = null) {
    return db.prepare(`SELECT d.id, d.name, i.persistent_id AS persistentId
      FROM address_members a JOIN devices d ON a.device_id = d.id LEFT JOIN ios_device_ids i ON i.device_id = d.id
      WHERE d.platform = 'ios' AND d.revoked_at IS NULL AND (? IS NULL OR d.id = ? OR i.persistent_id = ?)
      ORDER BY d.name, d.id`).all(id, id, id).filter((row) => accountDeviceVisible(row.id));
  }

  function pushTo(deviceId, type, data) {
    const socket = hostRooms.get(deviceId)?.socket;
    if (!socket?.connected) return false;
    socket.emit('bmsg_push', { type, data });
    return true;
  }

  function announceDevice(id, type = 'device_info_changed', visibleId = id) {
    const device = type === 'device_unbind' ? null : addressHost(db, id);
    const mobile = type === 'device_unbind' || device ? null : mobileDevices(id)[0];
    if (!accountDeviceVisible(id) && type !== 'device_unbind') {
      announcedDevices.delete(id);
      return;
    }
    if (!device && !mobile && type !== 'device_unbind') return;
    const data = device ? desktopDevice(device) : mobile ? mobileDevice(mobile) : { device_id: visibleId };
    for (const [receiver, room] of hostRooms) {
      if (room.addressMember && accountDeviceAllowed(receiver)) pushTo(receiver, type, data);
    }
  }

  function removeDesktopDevice(id, visibleId = id) {
    assistance.cancelForDevice(id);
    power.cancelForDevice(id);
    wallpapers.removeDevice(id);
    announcedDevices.delete(id);
    announceDevice(id, 'device_unbind', visibleId);
    closeHostRoom(id, 'device_revoked');
    for (const session of sessions.values()) {
      if (session.controllerId === id) finishSession(session, 'device_revoked');
    }
  }

  const approvalLifetime = deviceLogin ? DEVICE_LOGIN_APPROVAL_LIFETIME : undefined;
  const admin = adminPort === null ? null : createAdmin({ db, now, readJson, HttpError, approvalLifetime, revoke(id) {
    const mobile = db.prepare('SELECT persistent_id AS persistentId FROM ios_device_ids WHERE device_id = ?').get(id);
    if (!revokeDevice(db, id)) return false;
    deviceLogin?.invalidate(id);
    removeDesktopDevice(id, mobile?.persistentId ?? id);
    return true;
  } });

  function roomResponse(ticket, signalingUrl) {
    return {
      token: ticket,
      signaling_server: signalingUrl,
      ws_connect_timeout_ms: 10_000,
      streamer_retry_delta_ms: 1_000,
      max_reconnect_delta: 300,
    };
  }

  function createDeviceRoom(device) {
    const timestamp = now();
    const ticket = randomSecret(32);
    const roomId = randomUUID();
    const expiresAt = timestamp + HOST_ROOM_TTL_MS;
    db.exec('BEGIN IMMEDIATE');
    try {
      db.prepare('UPDATE room_tickets SET retired_at = ? WHERE device_id = ? AND retired_at IS NULL').run(timestamp, device.id);
      if (device.role === 'host') db.prepare('UPDATE devices SET controllable = 0 WHERE id = ?').run(device.id);
      db.prepare(`INSERT INTO room_tickets (id, device_id, token_hash, expires_at, created_at)
        VALUES (?, ?, ?, ?, ?)`).run(roomId, device.id, hashSecret(ticket), expiresAt, timestamp);
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
    closeHostRoom(device.id, 'host_room_replaced');
    return { ticket, signalingUrl: makeRoomUrl(getPublicOrigin()), expiresAt: isoTime(expiresAt) };
  }

  function setHostControllable(id, enabled) {
    if (typeof enabled !== 'boolean') fail(400, 'enabled must be a boolean');
    const updated = db.prepare('UPDATE devices SET controllable = ? WHERE id = ? AND revoked_at IS NULL').run(enabled ? 1 : 0, id);
    if (!updated.changes) fail(401, 'device authentication failed');
    if (!enabled) {
      assistance.cancelForDevice(id);
      power.cancelForDevice(id);
      finishHostSessions(id, 'host_disabled');
    }
    announceDevice(id);
  }

  function reserveSession(controller, hostId, {
    native = false, force = false, assistance = false, replacementId = null, retryPending = false,
  } = {}) {
    if (!accountDeviceAllowed(controller.id) || !accountDeviceAllowed(hostId)) fail(403, 'controller is not authorized for this host');
    if (controller.id === hostId) fail(400, 'cannot control this device from itself');
    const hostDevice = db.prepare(`SELECT controllable, revoked_at AS revokedAt FROM devices
      WHERE id = ? AND role = 'host'`).get(hostId);
    if (!hostDevice || hostDevice.revokedAt !== null) fail(404, 'host device was not found');
    const room = hostRooms.get(hostId);
    if (!room?.socket?.connected) fail(409, 'host is offline', 2006);
    if (!hostDevice.controllable) fail(409, 'host is not accepting sessions', 2007);
    const timestamp = now();
    let previous = activeSessionsByHost.get(hostId);
    if (previous?.usedAt === null && previous.expiresAt <= timestamp) {
      endUnclaimedSession(previous, 'session_ticket_expired');
      previous = undefined;
    }
    if (previous && !force && previous.id !== replacementId) {
      if (retryPending && previous.pendingTicket && previous.controllerId === controller.id) {
        return { session: previous, ticket: previous.pendingTicket };
      }
      fail(409, 'host already has an active controller', 2002);
    }
    const ticket = randomSecret(32);
    const session = {
      id: randomUUID(),
      hostId,
      controllerId: controller.id,
      ticketHash: hashSecret(ticket),
      pendingTicket: retryPending ? ticket : null,
      expiresAt: timestamp + SESSION_TICKET_TTL_MS,
      clientId: randomUUID(),
      iceId: randomUUID(),
      appControlId: native ? null : randomUUID(),
      controllerPlatform: controller.platform,
      native,
      force,
      assistance,
      replacementId,
      usedAt: null,
      toHost: [],
      toController: [],
      hostRoomId: room.ticketId,
    };
    db.prepare(`INSERT INTO sessions (id, host_id, controller_id, ticket_hash, expires_at, created_at)
      VALUES (?, ?, ?, ?, ?, ?)`).run(session.id, hostId, controller.id, session.ticketHash, session.expiresAt, timestamp);
    sessions.set(session.id, session);
    if (!previous) activeSessionsByHost.set(hostId, session);
    return { session, ticket };
  }

  function enrollAddressClient(req, platform, rawName, persistentId) {
    const token = bearerToken(req);
    if (!ADDRESS_TOKEN.test(token)) fail(400, 'address credential is invalid');
    if (!['windows', 'ios', 'web'].includes(platform)) fail(400, 'invalid device platform');
    if (persistentId !== undefined && (platform !== 'ios' || typeof persistentId !== 'string' || !/^[a-f0-9]{16}$/u.test(persistentId))) {
      fail(400, 'invalid persistent iOS device ID');
    }
    const name = normalizeName(rawName);
    const existing = lookupDeviceByToken(token);
    if (!existing) rateLimitPair(req);
    const id = enrollAddressDevice(db, token, name, platform, now(), persistentId, requireDeviceApproval, approvalLifetime);
    if (id?.conflict) fail(409, 'persistent iOS device ID is already assigned');
    if (id?.queueFull) fail(429, '设备准入队列已满');
    if (!id) fail(401, 'device credential is revoked or invalid');
    return { id, existing, name };
  }

  function desktopDeviceDetails(target) {
    const hardware = db.prepare('SELECT details FROM desktop_hardware WHERE device_id = ?').get(target.id);
    const reported = JSON.parse(hardware?.details ?? '[]');
    const system = reported.find(([label]) => label === '系统')?.[1] ?? '';
    return {
      alias: target.name,
      os_version: /^Windows\s+(\d+(?:\.\d+)*)\b/iu.exec(system)?.[1] ?? '',
      details: [['设备名称', target.name], ['平台', 'Windows'], ['设备 ID', target.id], ...reported],
    };
  }

  function iosDeviceRequest(device, method, path, send) {
    switch (`${method} ${path}`) {
      case 'POST /room': {
        if (!getPublicOrigin().startsWith('https:')) fail(409, 'iOS signaling requires an HTTPS public origin');
        const room = createDeviceRoom(device);
        send({ ...roomResponse(room.ticket, room.signalingUrl), signaling_list: [room.signalingUrl] });
        return;
      }
      case 'GET /profile':
        send({ nickname: '自建服务器' });
        return;
      case 'GET /devices':
        send({
          current_device: {
            device_id: device.id, alias: device.name, status: 'DISCONNECTED', platform: 3,
            controllable: false, controlled_support: false, participants_info: [], update_started_at: 0,
          },
          my_binded_devices: desktopHosts().map((host) => {
            const entry = desktopDevice(host);
            return {
              ...entry, status: entry.participants_info.length ? 'CONTROLLED' : entry.status,
              participants_info: [], update_started_at: 0,
            };
          }),
          others_shared_devices: [],
        });
        return;
      case 'GET /groups':
        send({ current_device_id: device.id, desktop_devices: desktopHosts().map(desktopDevice), mobile_devices: [], tv_devices: [] });
        return;
      default:
        break;
    }
    const detail = /^\/devices\/([a-f0-9]{16})\/detail$/u.exec(path);
    if (method === 'GET' && detail) {
      const target = addressHost(db, detail[1]);
      if (!target || !accountDeviceVisible(target.id)) fail(404, 'device was not found');
      send(desktopDeviceDetails(target), 0, 'ok');
      return;
    }
    if (method === 'POST' && path.startsWith('/join/')) {
      const hostId = path.slice('/join/'.length);
      const host = addressHost(db, hostId);
      if (!host) fail(403, 'controller is not authorized for this host');
      const signalingUrl = makeRoomUrl(getPublicOrigin());
      if (!signalingUrl.startsWith('wss:')) fail(409, 'iOS signaling requires an HTTPS public origin');
      const { ticket } = reserveSession(device, hostId, { native: true, retryPending: true });
      send({
        ...roomResponse(ticket, signalingUrl), signaling_list: [signalingUrl],
        publisher_device_id: host.id, device_name: host.name,
        publisher_platform: 1, publisher_version_name: host.versionName ?? '',
      });
      return;
    }
    fail(403, 'device role is not allowed for this endpoint');
  }

  async function desktopRequest(req, res, path, native = false) {
    if (!addressClientsAllowed(req)) fail(403, 'server does not admit devices from this network');
    const body = req.method === 'GET' ? {} : await readJson(req);
    const send = (data, code = 0, msg = '') => {
      if (!res.destroyed) jsonResponse(res, 200, native ? { code, msg, data } : data);
    };
    if (deviceLogin && native && req.method === 'POST') {
      if (path === '/api/v1/qrcode/gen/login' || path === '/api/v1/qrcode/login/status' || path === '/api/v1/login/by_qrcode') {
        const installationHash = hashSecret(bearerToken(req));
        res.setHeader('Cache-Control', 'no-store');
        if (path === '/api/v1/qrcode/gen/login') {
          rateLimitPair(req);
          send(deviceLogin.generate(installationHash));
        } else if (path === '/api/v1/qrcode/login/status') {
          send(await deviceLogin.poll(res, body, installationHash));
        } else {
          const { login, deviceId, replay } = deviceLogin.exchange(body, installationHash);
          if (!replay) {
            closeDeviceLoginRooms(deviceId, 'account_replaced');
            announceDevice(deviceId, 'device_binded');
            announcedDevices.add(deviceId);
          }
          send(login);
        }
        return;
      }
      if (path === '/api/v1/qrcode/scanned' || path === '/api/v1/qrcode/login/action') {
        authenticateHttp(req);
        send(deviceLogin.action(owner.id, body.qrcode_jump_url, path.endsWith('/scanned') ? 'scan' : body.action, true));
        return;
      }
    }
    if (native && req.method === 'POST' && path === '/api/v1/device/ios/init') {
      const { id, existing, name } = enrollAddressClient(req, 'ios', body.name, body.persistent_id);
      if (id.pending) {
        if (deviceLogin) {
          send({ device_id: id.id, alias: name });
          return;
        }
        jsonResponse(res, 403, { code: 4031, msg: `设备 ${id.id} 等待管理员批准`, data: null });
        return;
      }
      if (!announcedDevices.has(id) || existing?.revokedAt != null) {
        announcedDevices.add(id);
        announceDevice(id, 'device_binded');
      } else if (existing && existing.name !== name) {
        announceDevice(id);
      }
      send({ device_id: id, alias: name, status: 'DISCONNECTED' });
      return;
    }
    if (native && req.method === 'POST' && path === '/configures' && Object.keys(body).length === 0) {
      // Existing internal iOS builds send an empty query on this route.
      const token = req.headers.authorization ? bearerToken(req) : null;
      const device = token && (deviceLogin ? deviceLogin.authenticate(token) : lookupDeviceByToken(token));
      const ios = device && device.revokedAt === null && isEnrolledIos(device);
      send(ios ? { controlled_ability_map: IOS_WINDOWS_ABILITIES } : {});
      return;
    }
    if (req.method === 'POST' && path === '/register') {
      if (deviceLogin && !native) fail(404, 'not found');
      const platform = native ? 'windows' : body.platform ?? 'windows';
      const { id, existing, name } = enrollAddressClient(req, platform, body.name, body.persistent_id);
      if (id.pending) {
        if (deviceLogin) {
          send({ device_id: id.id, alias: name });
          return;
        }
        if (native) {
          jsonResponse(res, 403, { code: 4031, msg: `设备 ${id.id} 等待管理员批准`, data: null });
        } else {
          jsonResponse(res, 202, { device_id: id.id, status: 'pending' });
        }
        return;
      }
      if (native && typeof body.version_name === 'string' && /^\d+(?:\.\d+){2,3}$/u.test(body.version_name) && body.version_name.length <= 32) {
        db.prepare(`INSERT INTO desktop_devices (device_id, version_name) VALUES (?, ?)
          ON CONFLICT(device_id) DO UPDATE SET version_name = excluded.version_name`).run(id, body.version_name);
      }
      if (native) {
        const details = windowsHardwareDetails(body);
        if (details.length) db.prepare(`INSERT INTO desktop_hardware (device_id, details) VALUES (?, ?)
          ON CONFLICT(device_id) DO UPDATE SET details = excluded.details`).run(id, JSON.stringify(details));
      }
      if ((!announcedDevices.has(id) || existing?.revokedAt != null) && ['windows', 'ios'].includes(platform)) {
        announcedDevices.add(id);
        announceDevice(id, 'device_binded');
      } else if (existing && (native || (platform === 'ios' && existing.name !== name))) {
        announceDevice(id);
      }
      send({ device_id: id });
      return;
    }
    const device = authenticateHttp(req);
    if (deviceLogin && native) {
      if (path === '/profile' && req.method === 'GET') {
        send({ user_id: owner.id, nickname: owner.username, member_type: 0 });
        return;
      }
      if (path === '/disconnect' && req.method === 'POST') {
        deviceLogin.invalidate(device.id);
        closeDeviceLoginRooms(device.id, 'account_logged_out');
        send({});
        return;
      }
    }
    if (deviceLogin && (path === '/account' || path === '/account/transfer')) fail(404, 'not found');
    if (!native && req.method === 'POST' && path === '/account/transfer') {
      if (!isEnrolledIos(device)) fail(403, 'iOS enrollment required');
      rateLimitPair(req);
      const [previousEntry] = mobileDevices(device.id);
      const { id, ...account } = accounts.transferDevice(device, body.code);
      for (const session of sessions.values()) {
        if (session.controllerId === device.id || session.controllerId === id) {
          finishSession(session, 'device_transferred');
        }
      }
      removeDesktopDevice(device.id, previousEntry?.persistentId ?? device.id);
      closeHostRoom(id, 'device_transferred');
      announceDevice(id);
      send(account);
      return;
    }
    if (!native && path === '/video-preferences') {
      if (!isEnrolledIos(device)) fail(403, 'iOS enrollment required');
      if (req.method === 'GET') {
        const row = db.prepare('SELECT preferences FROM ios_video_preferences WHERE device_id = ?').get(device.id);
        send({ preferences: row ? JSON.parse(row.preferences) : null });
      } else if (req.method === 'PUT') {
        const preferences = body.preferences;
        if (!isRecord(preferences) || Object.entries(preferences).some(([key, value]) => {
          if (!IOS_VIDEO_KEYS.has(key) || typeof value !== 'string' || !value.length) return true;
          const decoded = Buffer.from(value, 'base64');
          return decoded.length > IOS_VIDEO_VALUE_LIMIT || decoded.toString('base64') !== value;
        })) fail(400, 'invalid iOS video preferences');
        db.prepare(`INSERT INTO ios_video_preferences (device_id, preferences) VALUES (?, ?)
          ON CONFLICT(device_id) DO UPDATE SET preferences = excluded.preferences`).run(device.id, JSON.stringify(preferences));
        send({ preferences });
      } else {
        fail(404, 'not found');
      }
      return;
    }
    if (path === '/account') {
      if (!isEnrolledIos(device)) fail(403, 'iOS enrollment required');
      if (req.method === 'GET') send({ ...accounts.deviceAccount(device.id), portal: `${accounts.portalOrigin()}/account` });
      else if (req.method === 'POST') send(accounts.linkDevice(device, body.code));
      else if (req.method === 'DELETE') send(accounts.unlinkDevice(device) ?? {});
      else fail(404, 'not found');
      return;
    }
    if (native && isEnrolledIos(device)) {
      iosDeviceRequest(device, req.method, path, send);
      return;
    }
    if (device.role !== 'host') fail(403, 'device role is not allowed for this endpoint');
    if (!device.addressMember) fail(403, 'address enrollment required');
    if (!deviceLogin && native && await assistance.request(device, req.method, path, body, res, send)) return;
    if (native && await power.request(device, path, body, req, res, send)) return;
    switch (`${req.method} ${path}`) {
      case 'GET /profile':
        send({ nickname: '自建服务器' });
        return;
      case 'GET /devices':
        send({ current_device: desktopDevice(addressHost(db, device.id)), my_binded_devices: desktopHosts().filter((row) => row.id !== device.id).map(desktopDevice) });
        return;
      case 'GET /groups':
        send({ current_device_id: device.id, desktop_devices: desktopHosts().map(desktopDevice),
          mobile_devices: mobileDevices().map(mobileDevice), tv_devices: [] });
        return;
      case 'POST /room': {
        const room = createDeviceRoom(device);
        send(native ? roomResponse(room.ticket, room.signalingUrl) : room);
        return;
      }
      case 'POST /controllable':
        setHostControllable(device.id, body.controllable);
        send({});
        return;
      case 'POST /disconnect':
        assistance.cancelForDevice(device.id);
        power.cancelForDevice(device.id);
        db.prepare('UPDATE room_tickets SET retired_at = ? WHERE device_id = ? AND retired_at IS NULL').run(now(), device.id);
        closeHostRoom(device.id, 'host_disconnected');
        for (const session of sessions.values()) {
          if (session.controllerId === device.id) finishSession(session, 'controller_disconnected');
        }
        send({});
        return;
      case 'POST /configures':
        if (!Array.isArray(body.configures) || body.configures.length < 1 || body.configures.length > 30
          || body.configures.some((entry) => typeof entry?.name !== 'string' || typeof entry.version !== 'string')) {
          fail(400, 'configuration query requires 1 to 30 entries');
        }
        send(Object.fromEntries(body.configures.map(({ name }) => {
          const data = name === 'win_keylock_optimize' ? { enable: true }
            : name === 'app_white_list' ? { gamewhitelist: [] }
            : name === 'custom_bitrate_limit' ? { enable: false } : null;
          return [name, { version: data ? '1' : '', status: data ? 0 : 404, data }];
        })));
        return;
      case 'GET /api/v1/member/entitlements': {
        const serverTime = Math.floor(now() / 1_000);
        send({
          version: 1,
          platform: 1,
          server_time: serverTime,
          expired_at: serverTime + 3600,
          features: { screen_quality: { periods: [{ start_time: null, end_time: null, grant_type: 0, limit: { quality: null } }] } },
        });
        return;
      }
      case 'GET /api/v1/tool/fp/token':
        if (new URL(req.url, getPublicOrigin()).searchParams.get('filetype') !== '1') fail(400, 'unsupported file type');
        send(wallpapers.grant(device));
        return;
      case 'POST /api/v1/device/wallpaper':
        wallpapers.bind(device, body.wallpaper_url);
        send({});
        return;
      default:
        break;
    }
    if (req.method === 'POST' && (path === '/join' || path.startsWith('/join/'))) {
      const id = native ? path.slice('/join/'.length) : body.device_id;
      if (typeof id !== 'string' || !addressHost(db, id)) fail(404, 'device was not found', 4042);
      if (typeof body.force_join !== 'boolean') fail(400, 'force_join must be a boolean');
      const { ticket } = reserveSession(device, id, { native: true, force: body.force_join, retryPending: Boolean(deviceLogin) && !body.force_join });
      const signalingUrl = makeRoomUrl(getPublicOrigin());
      send(native ? roomResponse(ticket, signalingUrl) : { ticket, signalingUrl });
      return;
    }
    const match = /^\/devices\/([a-f0-9]{16})(\/(?:unbind|detail))?$/u.exec(path);
    const target = match && accountDeviceVisible(match[1]) && addressHost(db, match[1]);
    if (!target && match && req.method === 'GET' && (native ? match[2] === '/detail' : !match[2])) {
      const mobile = mobileDevices(match[1])[0];
      if (mobile) {
        send({ alias: mobile.name, os_version: '', details: [['设备名称', mobile.name], ['平台', 'iOS'], ['设备 ID', mobile.persistentId ?? mobile.id]] });
        return;
      }
    }
    if (!target) fail(404, 'device was not found');
    if (req.method === 'POST' && match[2] === '/unbind') {
      if (target.id === device.id) fail(400, 'cannot remove the current device');
      revokeDevice(db, target.id);
      deviceLogin?.invalidate(target.id);
      removeDesktopDevice(target.id);
      send({});
      return;
    }
    if (req.method === 'GET' && (native ? match[2] === '/detail' : !match[2])) {
      send(desktopDeviceDetails(target));
      return;
    }
    if (req.method === 'PUT' && !match[2]) {
      const alias = normalizeName(body.alias);
      const updated = db.prepare('UPDATE devices SET name = ? WHERE id = ? AND revoked_at IS NULL').run(alias, target.id);
      if (!updated.changes) fail(404, 'device was not found');
      announceDevice(target.id);
      send({ device_id: target.id, alias });
      return;
    }
    fail(404, 'not found');
  }

  function sessionRow(id) {
    return db.prepare(`SELECT id, host_id AS hostId, controller_id AS controllerId, ticket_hash AS ticketHash,
        expires_at AS expiresAt, used_at AS usedAt, ended_at AS endedAt, end_reason AS endReason
      FROM sessions WHERE id = ?`).get(id);
  }

  function finishSession(session, reason, { hostEvent = 'released', sendHost = true, sendEnded = true } = {}) {
    if (!session || session.ended) return;
    session.ended = true;
    session.pendingTicket = null;
    db.prepare(`UPDATE sessions SET ended_at = COALESCE(ended_at, ?), end_reason = COALESCE(end_reason, ?)
      WHERE id = ?`).run(now(), reason, session.id);
    if (session.disconnectTimer) clearTimeout(session.disconnectTimer);
    if (sendHost && session.usedAt !== null) {
      const room = hostRooms.get(session.hostId);
      if (room?.ticketId === session.hostRoomId) {
        if (!room.socket?.connected) room.pendingRelease = { session, event: hostEvent };
        else emitHostRelease(room, session, hostEvent);
      }
    }
    if (session.nativeSocket?.connected) {
      if (sendEnded) session.nativeSocket.emit(reason === 'host_disconnected' ? 'publisher_disconnect' : 'leave');
      session.nativeSocket.disconnect(true);
    }
    if (sendEnded) sendSocketJson(session.controllerSocket, { type: 'ended', reason });
    if (session.controllerSocket?.readyState === WebSocket.OPEN) {
      try { session.controllerSocket.close(1000, 'session ended'); } catch { /* already closing */ }
    }
    if (sessions.get(session.id) === session) sessions.delete(session.id);
    if (activeSessionsByHost.get(session.hostId) === session) activeSessionsByHost.delete(session.hostId);
    if (session.assistance) assistance.ended(session);
    if (session.usedAt !== null) announceDevice(session.hostId);
  }

  function finishHostSessions(deviceId, reason, options) {
    for (const session of sessions.values()) {
      if (session.hostId === deviceId) finishSession(session, reason, options);
    }
  }

  function closeDeviceLoginRooms(deviceId, reason) {
    closeHostRoom(deviceId, reason);
    for (const session of sessions.values()) {
      if (session.controllerId === deviceId) finishSession(session, reason);
    }
  }

  function closeHostRoom(deviceId, reason) {
    const room = hostRooms.get(deviceId);
    if (!room) return;
    if (room.disconnectTimer) clearTimeout(room.disconnectTimer);
    room.disconnectTimer = null;
    finishHostSessions(deviceId, reason);
    assistance.cancelForDevice(deviceId);
    power.cancelForDevice(deviceId);
    hostRooms.delete(deviceId);
    if (room.socket?.connected) room.socket.disconnect(true);
    announceDevice(deviceId);
  }

  function endUnclaimedSession(session, reason) {
    if (session.usedAt !== null) return;
    finishSession(session, reason, { sendHost: true, sendEnded: false });
  }

  function relayNativeSignal(session, socket, queue, event, payload) {
    if (socket?.connected) socket.emit(event, payload);
    else if (queue.length < MAX_QUEUED_SIGNALS) queue.push([event, payload]);
    else finishSession(session, 'signaling_backlog_exceeded');
  }

  function flushNativeSignals(socket, queue) {
    for (const [event, payload] of queue.splice(0)) socket.emit(event, payload);
  }

  function rateLimitPair(req) {
    const key = req.socket.remoteAddress || 'unknown';
    const current = now();
    const previous = pairAttempts.get(key);
    if (!previous || current - previous.startedAt >= 60_000) {
      pairAttempts.set(key, { startedAt: current, count: 1 });
      return;
    }
    previous.count += 1;
    if (previous.count > 30) fail(429, 'pairing rate limit exceeded');
  }

  function claimPairing(body) {
    if (!['host', 'controller'].includes(body.role)) fail(400, 'role must be host or controller');
    if (!['windows', 'ios', 'web'].includes(body.platform)) fail(400, 'platform must be windows, ios, or web');
    if (body.role === 'host' && body.platform !== 'windows') fail(400, 'host devices must use the windows platform');
    if (body.role === 'controller' && !['ios', 'web'].includes(body.platform)) {
      fail(400, 'controller devices must use ios or web');
    }
    const name = normalizeName(body.name);
    if (typeof body.code !== 'string' || body.code.length < 20 || body.code.length > 128) fail(400, 'pairing code is invalid');
    const timestamp = now();
    const token = randomSecret(32);
    let deviceId;
    db.exec('BEGIN IMMEDIATE');
    try {
      const grant = db.prepare(`SELECT role, bound_host_id AS boundHostId, expires_at AS expiresAt, consumed_at AS consumedAt
        FROM pairing_codes WHERE code_hash = ?`).get(hashSecret(body.code));
      if (!grant || grant.consumedAt !== null || grant.expiresAt <= timestamp) fail(400, 'pairing code is invalid or expired');
      if (grant.role !== body.role) fail(400, 'pairing code is invalid for this role');
      if (grant.role === 'controller') {
        const enrolledHost = db.prepare("SELECT id FROM devices WHERE id = ? AND role = 'host' AND revoked_at IS NULL")
          .get(grant.boundHostId);
        if (!enrolledHost) fail(400, 'pairing code is invalid or expired');
      }
      const claimed = db.prepare(`UPDATE pairing_codes SET consumed_at = ?
        WHERE code_hash = ? AND consumed_at IS NULL AND expires_at > ?`).run(timestamp, hashSecret(body.code), timestamp);
      if (claimed.changes !== 1) fail(400, 'pairing code is invalid or expired');
      for (let attempt = 0; attempt < 4; attempt += 1) {
        const candidateId = randomDeviceId();
        try {
          db.prepare(`INSERT INTO devices (id, role, platform, name, token_hash, bound_host_id, paired_at)
            VALUES (?, ?, ?, ?, ?, ?, ?)`).run(
            candidateId,
            body.role,
            body.platform,
            name,
            hashSecret(token),
            body.role === 'controller' ? grant.boundHostId : null,
            timestamp,
          );
          deviceId = candidateId;
          break;
        } catch (error) {
          if (!String(error).includes('UNIQUE')) throw error;
        }
      }
      if (!deviceId) throw new Error('could not allocate device id');
      db.exec('COMMIT');
    } catch (error) {
      try { db.exec('ROLLBACK'); } catch { /* transaction already closed */ }
      throw error;
    }
    return { deviceId, token };
  }

  async function serveStatic(req, res, pathname) {
    if (req.method !== 'GET' && req.method !== 'HEAD') return false;
    const candidate = staticCandidate(staticRoot, pathname, req.headers.accept);
    if (!candidate) return false;
    res.writeHead(200, {
      'Content-Type': mimeType(extname(candidate.file)),
      'Content-Length': candidate.size,
      'Cache-Control': extname(candidate.file) === '.html' ? 'no-cache' : 'public, max-age=300',
      'X-Content-Type-Options': 'nosniff',
      'Referrer-Policy': 'no-referrer',
    });
    if (req.method === 'HEAD') {
      res.end();
      return true;
    }
    const stream = createReadStream(candidate.file);
    stream.on('error', () => {
      if (!res.headersSent) jsonResponse(res, 404, { error: 'not found' });
      else res.destroy();
    });
    stream.pipe(res);
    return true;
  }

  async function requestHandler(req, res) {
    let native = false;
    try {
      const pathname = requestPath(req);
      native = /^\/api\/v[12](?:\/|$)/u.test(pathname);
      if (pathname === '/account' || pathname.startsWith('/account/') || pathname.startsWith('/v1/account/')) {
        if (req.headers.origin && req.headers.origin !== accounts.portalOrigin()) fail(403, 'origin is not allowed');
        await accounts.request(req, res, pathname);
        return;
      }
      if (!originIsAllowed(req.headers.origin, getPublicOrigin)) fail(403, 'origin is not allowed');
      if (req.method === 'GET' && pathname === '/healthz') {
        jsonResponse(res, 200, { status: 'ok' });
        return;
      }
      if (pathname.startsWith('/v1/wallpapers/')) {
        if (!addressClientsAllowed(req)) fail(403, 'server does not admit devices from this network');
        if (req.method === 'POST' && pathname === '/v1/wallpapers/upload') {
          jsonResponse(res, 200, await wallpapers.upload(req));
        } else {
          wallpapers.download(req, res, pathname.slice('/v1/wallpapers/'.length));
        }
        return;
      }
      if (req.method === 'POST' && pathname === '/v1/pair') {
        if (deviceLogin) fail(404, 'not found');
        rateLimitPair(req);
        const body = await readJson(req);
        jsonResponse(res, 201, claimPairing(body));
        return;
      }
      if (pathname.startsWith('/v1/desktop/')) {
        await desktopRequest(req, res, pathname.slice('/v1/desktop'.length));
        return;
      }
      if (native) {
        await desktopRequest(req, res, nativeDesktopPath(pathname), true);
        return;
      }
      if (req.method === 'POST' && pathname === '/v1/host/room') {
        await readJson(req);
        jsonResponse(res, 200, createDeviceRoom(authenticateHttp(req, 'host')));
        return;
      }
      if (req.method === 'PUT' && pathname === '/v1/host/controllable') {
        const body = await readJson(req);
        setHostControllable(authenticateHttp(req, 'host').id, body.enabled);
        jsonResponse(res, 200, {});
        return;
      }
      if (req.method === 'GET' && pathname === '/v1/devices') {
        const device = authenticateHttp(req);
        let rows;
        if (device.addressMember && device.role === 'controller') {
          rows = desktopHosts();
        } else if (device.role === 'host') {
          rows = db.prepare(`SELECT id, role, platform, name, controllable FROM devices
            WHERE (id = ? OR bound_host_id = ?) AND revoked_at IS NULL ORDER BY role, paired_at, id`).all(device.id, device.id);
        } else {
          rows = db.prepare(`SELECT id, role, platform, name, controllable FROM devices
            WHERE id = ? AND revoked_at IS NULL`).all(device.boundHostId);
        }
        const devices = rows.map((row) => ({
          id: row.id,
          name: row.name,
          online: row.role === 'host' ? roomIsOnline(row.id) : controllerIsOnline(row.id),
          controllable: row.role === 'host' && Boolean(row.controllable),
        }));
        jsonResponse(res, 200, { devices });
        return;
      }
      if (req.method === 'POST' && pathname === '/v1/sessions') {
        const body = await readJson(req);
        const controller = authenticateHttp(req, 'controller');
        if (typeof body.deviceId !== 'string' || !/^[a-f0-9]{16}$/u.test(body.deviceId)) fail(400, 'deviceId is invalid');
        if (!controllerCanAccess(controller, body.deviceId)) fail(403, 'controller is not authorized for this host');
        if (body.transport !== undefined && body.transport !== 'socket.io') fail(400, 'unknown signaling transport');
        const useNativeSignaling = body.transport === 'socket.io';
        if (useNativeSignaling && !isEnrolledIos(controller)) fail(403, 'native signaling requires an enrolled iOS controller');
        const { session, ticket } = reserveSession(controller, body.deviceId, { native: useNativeSignaling });
        jsonResponse(res, 201, {
          sessionId: session.id,
          ticket,
          signalingUrl: useNativeSignaling ? makeRoomUrl(getPublicOrigin()) : makeSessionUrl(getPublicOrigin()),
          iceServers: iceServers.map((server) => ({ ...server, urls: [server.urls] })),
          expiresAt: isoTime(session.expiresAt),
        });
        return;
      }
      if (pathname.startsWith('/v1/') || pathname === '/v1') fail(404, 'not found');
      if (pathname === '/socket.io' || pathname.startsWith('/socket.io/')) fail(404, 'not found');
      if (await serveStatic(req, res, pathname)) return;
      fail(404, 'not found');
    } catch (error) {
      if (res.destroyed) return;
      if (res.headersSent) {
        res.destroy();
        return;
      }
      const status = error instanceof HttpError ? error.status : 500;
      const msg = error instanceof HttpError ? error.message : 'internal server error';
      const code = error instanceof HttpError ? error.code : status;
      jsonResponse(res, status, native ? { code, msg, data: null } : { error: msg }, req.method === 'HEAD');
    }
  }

  io.use((socket, next) => {
    try {
      const headers = socket.request.headers;
      if (!originIsAllowed(headers.origin, getPublicOrigin)) throw new Error('rejected');
      const token = headers['x-nrd-auth'];
      if (typeof token !== 'string' || !/^[A-Za-z0-9_-]{32,128}$/u.test(token)) throw new Error('rejected');
      const controlling = headers['x-nrd-controlling'];
      if (controlling !== '0' && controlling !== '1') throw new Error('rejected');
      const ticketHash = hashSecret(token);
      // The native controller sets X-NRD-CONTROLLING after the control ACK.
      // The ticket identifies its role from the initial connection onward.
      const pending = db.prepare('SELECT id FROM sessions WHERE ticket_hash = ? AND ended_at IS NULL').get(ticketHash);
      if (pending) {
        const session = sessions.get(pending.id);
        if (!addressClientsAllowed(socket.request) || !session?.native || session.ended
          || (session.usedAt === null && session.expiresAt <= now())) throw new Error('rejected');
        const host = addressHost(db, session.hostId);
        const room = hostRooms.get(session.hostId);
        if (!nativeControllerIsEnrolled(session) || !host?.controllable || room?.ticketId !== session.hostRoomId
          || (session.usedAt === null && !room.socket?.connected)) throw new Error('rejected');
        socket.data.nativeSession = session;
        next();
        return;
      }
      if (controlling !== '0') throw new Error('rejected');
      const row = db.prepare(`SELECT rt.id AS ticketId, rt.device_id AS deviceId, rt.expires_at AS expiresAt,
          rt.retired_at AS retiredAt, d.role, d.platform, d.revoked_at AS revokedAt,
          d.id IN (SELECT device_id FROM address_members) AS addressMember
        FROM room_tickets rt JOIN devices d ON d.id = rt.device_id WHERE rt.token_hash = ?`).get(ticketHash);
      const presenceOnly = row?.role === 'controller' && row.platform === 'ios' && Boolean(row.addressMember);
      if (!row || (row.role !== 'host' && !presenceOnly) || row.revokedAt !== null || row.retiredAt !== null || row.expiresAt <= now()) {
        throw new Error('rejected');
      }
      if (!accountDeviceAllowed(row.deviceId) || (row.addressMember && !addressClientsAllowed(socket.request))) throw new Error('rejected');
      socket.data.roomAuth = { ticketId: row.ticketId, deviceId: row.deviceId, addressMember: Boolean(row.addressMember), presenceOnly };
      next();
    } catch {
      next(new Error('unauthorized'));
    }
  });

  function connectNativeController(socket, session) {
    const previousSocket = session.nativeSocket;
    session.nativeSocket = socket;
    if (session.disconnectTimer) clearTimeout(session.disconnectTimer);
    session.disconnectTimer = null;
    if (previousSocket?.connected) previousSocket.disconnect(true);
    flushNativeSignals(socket, session.toController);

    socket.on('error', () => {});
    socket.on('room_info', (ack) => {
      if (typeof ack === 'function') ack({ room_id: session.hostId });
    });
    socket.on('refresh_reconnect_key', (ack) => {
      if (typeof ack === 'function') ack('success', { reconnect_key: randomSecret(24) });
    });
    socket.on('control', (payload, ack) => {
      if (typeof ack !== 'function') return;
      if (session.ended || session.nativeSocket !== socket || session.usedAt !== null || session.expiresAt <= now()) return ack('error', {});
      const room = hostRooms.get(session.hostId);
      const current = activeSessionsByHost.get(session.hostId);
      const host = addressHost(db, session.hostId);
      const attachment = payload?.app_data;
      const usesBinaryPrefix = Buffer.isBuffer(attachment) && session.controllerPlatform === 'ios' && attachment[0] === 0x04;
      const appData = usesBinaryPrefix ? attachment.subarray(1) : attachment;
      const valid = typeof payload?.app_control_id === 'string'
        && /^[a-f0-9]{8}(?:-[a-f0-9]{4}){3}-[a-f0-9]{12}$/iu.test(payload.app_control_id)
        && Buffer.isBuffer(appData) && appData.length > 0 && attachment.length <= 64 * 1024
        && typeof payload.streamer_data === 'string' && payload.streamer_data.length <= 64 * 1024;
      if (!valid || !nativeControllerIsEnrolled(session) || !host?.controllable || !room?.socket?.connected
        || room.ticketId !== session.hostRoomId
        || (current && current !== session && !session.force && current.id !== session.replacementId)) {
        return ack('error', {});
      }
      const timestamp = now();
      const claimed = db.prepare('UPDATE sessions SET used_at = ? WHERE id = ? AND used_at IS NULL AND ended_at IS NULL')
        .run(timestamp, session.id);
      if (!claimed.changes) return ack('error', {});
      if (current && current !== session) finishSession(current, 'controller_replaced');
      session.appControlId = payload.app_control_id;
      session.usesBinaryPrefix = usesBinaryPrefix;
      session.usedAt = timestamp;
      session.pendingTicket = null;
      activeSessionsByHost.set(session.hostId, session);
      if (session.assistance) assistance.started(session);
      const ids = { client_id: session.clientId, ice_id: session.iceId };
      room.socket.emit('be-controlled', {
        ...ids,
        app_control_id: session.appControlId,
        app_data: appData,
        controller_device_id: session.controllerId,
        connection_type: session.assistance ? 2 : 1,
        streamer_data: payload.streamer_data,
        auto_switch_network: canSwitchNetwork,
        iceServers,
        data: { iceServers },
      });
      ack('success', { code: 0, ...ids, iceServers, auto_switch_network: canSwitchNetwork });
      announceDevice(session.hostId);
    });
    for (const event of ['soac', 'forward_setting']) {
      socket.on(event, (payload) => {
        if (session.ended || session.usedAt === null || session.nativeSocket !== socket) return;
        const room = hostRooms.get(session.hostId);
        const data = payload?.data;
        if (!isRecord(data) || room?.ticketId !== session.hostRoomId
          || (event === 'soac' && (data.ice_id !== session.iceId || data.app_control_id !== session.appControlId))) return;
        relayNativeSignal(session, room.socket, session.toHost, event, { client_id: session.clientId, data });
      });
    }
    socket.on('leave', () => {
      if (session.nativeSocket === socket) finishSession(session, 'controller_left', { hostEvent: 'left', sendEnded: false });
    });
    socket.on('disconnect', (reason) => {
      if (session.ended || session.nativeSocket !== socket) return;
      session.nativeSocket = null;
      if (reason === 'client namespace disconnect') {
        finishSession(session, 'controller_left', { hostEvent: 'left', sendEnded: false });
        return;
      }
      if (session.usedAt === null) {
        endUnclaimedSession(session, 'controller_disconnected');
        return;
      }
      session.disconnectTimer = setTimeout(() => {
        finishSession(session, 'controller_disconnected', { hostEvent: 'left', sendEnded: false });
      }, HOST_RECONNECT_GRACE_MS);
      session.disconnectTimer.unref?.();
    });
  }

  io.on('connection', (socket) => {
    if (socket.data.nativeSession) {
      connectNativeController(socket, socket.data.nativeSession);
      return;
    }
    const auth = socket.data.roomAuth;
    let room = hostRooms.get(auth.deviceId);
    if (room?.disconnectTimer) clearTimeout(room.disconnectTimer);
    if (room && room.ticketId !== auth.ticketId) {
      closeHostRoom(auth.deviceId, 'host_room_replaced');
      room = null;
    }
    if (!room) {
      room = { ticketId: auth.ticketId, addressMember: auth.addressMember, presenceOnly: auth.presenceOnly,
        socket: null, disconnectTimer: null, pendingRelease: null };
      hostRooms.set(auth.deviceId, room);
    }
    const previousSocket = room.socket;
    room.socket = socket;
    if (previousSocket && previousSocket !== socket && previousSocket.connected) previousSocket.disconnect(true);
    if (room.pendingRelease) {
      emitHostRelease(room, room.pendingRelease.session, room.pendingRelease.event);
      room.pendingRelease = null;
    }
    const current = activeSessionsByHost.get(auth.deviceId);
    if (current?.native && current.hostRoomId === room.ticketId) flushNativeSignals(socket, current.toHost);
    if (room.addressMember) {
      const event = announcedDevices.has(auth.deviceId) ? 'device_info_changed' : 'device_binded';
      announcedDevices.add(auth.deviceId);
      announceDevice(auth.deviceId, event);
    }

    socket.on('error', () => {});
    socket.on('room_info', (ack) => {
      if (typeof ack === 'function') ack({ room_id: auth.deviceId });
    });
    socket.on('refresh_reconnect_key', (ack) => {
      const reconnectKey = randomSecret(24);
      const result = db.prepare(`UPDATE room_tickets SET reconnect_key_hash = ?
        WHERE id = ? AND retired_at IS NULL AND expires_at > ?`).run(hashSecret(reconnectKey), auth.ticketId, now());
      if (typeof ack === 'function') {
        if (result.changes === 1) ack('success', { reconnect_key: reconnectKey });
        else ack('error', {});
      }
    });
    if (auth.presenceOnly) {
      socket.on('disconnect', () => {
        if (room.socket === socket && hostRooms.get(auth.deviceId) === room) hostRooms.delete(auth.deviceId);
      });
      return;
    }
    socket.on('forward_setting', (payload) => {
      const active = activeSessionsByHost.get(auth.deviceId);
      const data = payload?.data;
      if (!active || active.ended || payload?.client_id !== active.clientId || !isRecord(data)) return;
      if (active.native) {
        relayNativeSignal(active, active.nativeSocket, active.toController, 'forward_setting', payload);
        return;
      }
      if (data.type !== 'device_capability' || !isRecord(data.device_capability)
        || data.device_capability.ice_id !== active.iceId) return;
      sendSocketJson(active.controllerSocket, { type: 'capability', capability: data.device_capability });
    });
    socket.on('soac', (payload) => {
      const active = activeSessionsByHost.get(auth.deviceId);
      const data = payload?.data;
      if (!active || active.ended || payload?.client_id !== active.clientId || !isRecord(data)
        || data.ice_id !== active.iceId || data.app_control_id !== active.appControlId) return;
      if (active.native) {
        const sdp = data.gzip_sdp;
        const outgoing = active.usesBinaryPrefix && Buffer.isBuffer(sdp) && sdp[0] === 0x1f && sdp[1] === 0x8b
          ? { ...payload, data: { ...data, gzip_sdp: Buffer.concat([Buffer.from([0x04]), sdp]) } }
          : payload;
        relayNativeSignal(active, active.nativeSocket, active.toController, 'soac', outgoing);
        return;
      }
      try {
        if (data.type === 'answer' || data.type === 'restart_ice') {
          const sdpValue = data.sdp || data.gzip_sdp;
          const sdp = decompressHostSdp(sdpValue);
          sendSocketJson(active.controllerSocket, { type: 'answer', sdp });
        } else if (data.type === 'candidate') {
          const candidate = validateCandidate(data.candidate);
          sendSocketJson(active.controllerSocket, { type: 'candidate', candidate });
        }
      } catch {
        finishSession(active, 'invalid_host_signal', { sendHost: false });
      }
    });
    socket.on('left', (payload) => {
      const active = activeSessionsByHost.get(auth.deviceId);
      if (active && matchesSessionEnvelope(payload, active)) finishSession(active, 'host_ended', { sendHost: false });
    });
    socket.on('released', (payload) => {
      const active = activeSessionsByHost.get(auth.deviceId);
      if (active && matchesSessionEnvelope(payload, active)) finishSession(active, 'host_ended', { sendHost: false });
    });
    socket.on('clear_out', (payload) => {
      const active = activeSessionsByHost.get(auth.deviceId);
      if (active && (payload === undefined ? !active.native : matchesSessionEnvelope(payload, active))) finishSession(active, 'host_cleared', { sendHost: false });
    });
    socket.on('leave', () => {
      socket.emit('leave');
      const active = activeSessionsByHost.get(auth.deviceId);
      if (active) finishSession(active, 'host_left', { sendHost: false });
      setTimeout(() => socket.disconnect(true), 20).unref?.();
    });
    socket.on('disconnect', () => {
      if (room.socket !== socket || hostRooms.get(auth.deviceId) !== room) return;
      room.socket = null;
      if (closed) return;
      if (room.addressMember) announceDevice(auth.deviceId);
      room.disconnectTimer = setTimeout(() => {
        if (room.socket || hostRooms.get(auth.deviceId) !== room) return;
        finishHostSessions(auth.deviceId, 'host_disconnected', { sendHost: false });
        assistance.cancelForDevice(auth.deviceId);
        power.cancelForDevice(auth.deviceId);
        hostRooms.delete(auth.deviceId);
        room.disconnectTimer = null;
      }, HOST_RECONNECT_GRACE_MS);
      room.disconnectTimer.unref?.();
    });
  });

  function matchesSessionEnvelope(payload, session) {
    return payload?.client_id === session.clientId
      && payload?.ice_id === session.iceId
      && payload?.app_control_id === session.appControlId;
  }

  server.on('upgrade', (request, socket, head) => {
    let pathname;
    try { pathname = new URL(request.url, 'http://localhost').pathname; } catch {
      socket.destroy();
      return;
    }
    if (pathname !== SESSION_PATH) return;
    if (!originIsAllowed(request.headers.origin, getPublicOrigin)) {
      socket.write('HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n');
      socket.destroy();
      return;
    }
    controllerSockets.handleUpgrade(request, socket, head, (ws) => {
      controllerSockets.emit('connection', ws, request);
    });
  });

  controllerSockets.on('connection', (ws, request) => {
    ws.isAlive = true;
    ws.on('pong', () => { ws.isAlive = true; });
    let session = null;
    let authenticated = false;
    let authTimer = setTimeout(() => {
      closeWithError(ws, 'authentication required');
    }, 5_000);
    authTimer.unref?.();

    ws.on('message', (raw, isBinary) => {
      if (isBinary) {
        closeWithError(ws, 'binary messages are not supported');
        if (session) finishSession(session, 'invalid_controller_message', { hostEvent: 'left' });
        return;
      }
      let message;
      try {
        message = JSON.parse(raw.toString('utf8'));
      } catch {
        closeWithError(ws, 'message is not valid JSON');
        if (session) finishSession(session, 'invalid_controller_message', { hostEvent: 'left' });
        return;
      }
      if (!isRecord(message) || typeof message.type !== 'string') {
        closeWithError(ws, 'message type is required');
        if (session) finishSession(session, 'invalid_controller_message', { hostEvent: 'left' });
        return;
      }
      if (!authenticated) {
        if (message.type !== 'authenticate' || typeof message.ticket !== 'string') {
          closeWithError(ws, 'authenticate with a session ticket first');
          return;
        }
        const ticketHash = hashSecret(message.ticket);
        const row = db.prepare(`SELECT id, host_id AS hostId, controller_id AS controllerId,
            expires_at AS expiresAt, used_at AS usedAt, ended_at AS endedAt
          FROM sessions WHERE ticket_hash = ?`).get(ticketHash);
        if (!row || row.usedAt !== null || row.endedAt !== null || row.expiresAt <= now()) {
          closeWithError(ws, 'session ticket is invalid or expired');
          return;
        }
        const state = sessions.get(row.id);
        const hostRoom = hostRooms.get(row.hostId);
        const hostDevice = db.prepare(`SELECT controllable, revoked_at AS revokedAt FROM devices
          WHERE id = ? AND role = 'host'`).get(row.hostId);
        const controllerDevice = db.prepare(`SELECT revoked_at AS revokedAt, bound_host_id AS boundHostId,
            id IN (SELECT device_id FROM address_members) AS addressMember FROM devices
          WHERE id = ? AND role = 'controller'`).get(row.controllerId);
        if (!state || state.native || state.ended || !hostRoom?.socket?.connected || !hostDevice?.controllable
          || hostDevice.revokedAt !== null || !controllerDevice || controllerDevice.revokedAt !== null
          || (controllerDevice.addressMember && !addressClientsAllowed(request))
          || !controllerCanAccess(controllerDevice, row.hostId) || hostRoom.ticketId !== state.hostRoomId) {
          closeWithError(ws, 'session is no longer available');
          if (state) finishSession(state, 'session_unavailable', { sendHost: true, sendEnded: false });
          return;
        }
        const claimed = db.prepare(`UPDATE sessions SET used_at = ?
          WHERE id = ? AND used_at IS NULL AND ended_at IS NULL AND expires_at > ?`).run(now(), row.id, now());
        if (claimed.changes !== 1) {
          closeWithError(ws, 'session ticket is invalid or expired');
          return;
        }
        session = state;
        session.usedAt = now();
        session.controllerSocket = ws;
        session.authenticated = true;
        authenticated = true;
        clearTimeout(authTimer);
        ws.send(JSON.stringify({ type: 'ready' }));
        const host = hostRooms.get(session.hostId)?.socket;
        if (!host?.connected) {
          finishSession(session, 'host_disconnected', { sendHost: false });
          return;
        }
        host.emit('be-controlled', {
          client_id: session.clientId,
          ice_id: session.iceId,
          app_control_id: session.appControlId,
          app_data: createConnectOptions(session.controllerId, session.controllerPlatform),
          controller_device_id: session.controllerId,
          connection_type: 1,
          streamer_data: createStreamerData(session.iceId),
          iceServers,
        });
        announceDevice(session.hostId);
        return;
      }
      if (session?.ended) return;
      try {
        if (message.type === 'offer') {
          const sdp = validateSdp(message.sdp);
          const host = hostRooms.get(session.hostId)?.socket;
          if (!host?.connected) throw new Error('host is offline');
          host.emit('soac', {
            client_id: session.clientId,
            data: {
              app_control_id: session.appControlId,
              ice_id: session.iceId,
              type: 'offer',
              ice_network_type: 0,
              sdp,
            },
          });
        } else if (message.type === 'candidate') {
          const candidate = validateCandidate(message.candidate);
          const host = hostRooms.get(session.hostId)?.socket;
          if (!host?.connected) throw new Error('host is offline');
          host.emit('soac', {
            client_id: session.clientId,
            data: {
              app_control_id: session.appControlId,
              ice_id: session.iceId,
              type: 'candidate',
              candidate,
            },
          });
        } else if (message.type === 'leave') {
          finishSession(session, 'controller_left', { hostEvent: 'left' });
        } else {
          throw new Error('unsupported message type');
        }
      } catch (error) {
        closeWithError(ws, error.message || 'invalid message');
        if (session && !session.ended) finishSession(session, 'invalid_controller_message', { hostEvent: 'left' });
      }
    });

    ws.on('close', () => {
      clearTimeout(authTimer);
      if (session && !session.ended) finishSession(session, 'controller_disconnected', { hostEvent: 'left', sendEnded: false });
    });
    ws.on('error', () => {});
  });

  function closeWithError(ws, message) {
    if (ws.readyState === WebSocket.OPEN) {
      try { ws.send(JSON.stringify({ type: 'error', message })); } catch { /* ignore */ }
      try { ws.close(1008, 'protocol error'); } catch { /* already closing */ }
    }
  }

  const heartbeat = setInterval(() => {
    for (const ws of controllerSockets.clients) {
      if (ws.readyState !== WebSocket.OPEN) continue;
      if (ws.isAlive === false) {
        ws.terminate();
        continue;
      }
      ws.isAlive = false;
      try { ws.ping(); } catch { ws.terminate(); }
    }
  }, options.wsHeartbeatIntervalMs || 30_000);
  heartbeat.unref?.();

  const maintenance = setInterval(() => {
    if (closed) return;
    assistance.prune();
    wallpapers.prune();
    const timestamp = now();
    for (const [deviceId, room] of hostRooms) {
      if (!accountDeviceAllowed(deviceId)) {
        closeHostRoom(deviceId, 'account_expired');
        continue;
      }
      const row = db.prepare(`SELECT rt.expires_at AS expiresAt, rt.retired_at AS retiredAt,
          d.revoked_at AS revokedAt
        FROM room_tickets rt JOIN devices d ON d.id = rt.device_id WHERE rt.id = ?`).get(room.ticketId);
      if (!row || row.retiredAt !== null || row.expiresAt <= timestamp || row.revokedAt !== null) {
        if (row?.revokedAt != null && room.addressMember) removeDesktopDevice(deviceId);
        else closeHostRoom(deviceId, 'host_room_expired');
      } else if (room.addressMember && room.socket?.connected && row.expiresAt - timestamp < HOST_ROOM_TTL_MS / 2) {
        db.prepare('UPDATE room_tickets SET expires_at = ? WHERE id = ? AND retired_at IS NULL')
          .run(timestamp + HOST_ROOM_TTL_MS, room.ticketId);
      }
    }
    deviceLogin?.prune();
    for (const session of [...sessions.values()]) {
      if (!accountDeviceAllowed(session.controllerId) || !accountDeviceAllowed(session.hostId)) {
        finishSession(session, 'account_expired');
        continue;
      }
      const row = sessionRow(session.id);
      if (!row || row.endedAt !== null) {
        finishSession(session, row?.endReason || 'session_ended', { sendHost: true });
        continue;
      }
      if (row.usedAt === null && row.expiresAt <= timestamp) {
        endUnclaimedSession(session, 'session_ticket_expired');
        continue;
      }
      const controller = db.prepare('SELECT revoked_at AS revokedAt FROM devices WHERE id = ?').get(session.controllerId);
      const controlledHost = db.prepare('SELECT revoked_at AS revokedAt FROM devices WHERE id = ?').get(session.hostId);
      if (controller?.revokedAt !== null || controlledHost?.revokedAt !== null) {
        finishSession(session, controller?.revokedAt !== null ? 'controller_revoked' : 'host_revoked');
      }
    }
  }, options.maintenanceIntervalMs || ACTIVE_CHECK_MS);
  maintenance.unref?.();

  async function listen(listenPort = port, listenHost = host) {
    await new Promise((resolveListen, reject) => {
      const onError = (error) => {
        server.off('listening', onListening);
        reject(error);
      };
      const onListening = () => {
        server.off('error', onError);
        const address = server.address();
        if (address && typeof address === 'object') boundPort = address.port;
        resolveListen();
      };
      server.once('error', onError);
      server.once('listening', onListening);
      server.listen(listenPort, listenHost);
    });
    if (admin) {
      try {
        adminOrigin = await admin.listen(adminPort);
      } catch (error) {
        await close();
        throw error;
      }
    }
    return server.address();
  }

  async function close() {
    if (closed) return;
    closed = true;
    clearInterval(heartbeat);
    clearInterval(maintenance);
    if (admin) await admin.close();
    assistance.close();
    power.close();
    for (const room of hostRooms.values()) {
      if (room.disconnectTimer) clearTimeout(room.disconnectTimer);
      if (room.socket?.connected) room.socket.disconnect(true);
    }
    hostRooms.clear();
    for (const session of [...sessions.values()]) finishSession(session, 'server_shutdown', { sendHost: false });
    for (const ws of controllerSockets.clients) {
      try { ws.close(1001, 'server shutdown'); } catch { /* already closed */ }
    }
    await new Promise((resolveClose) => io.close(() => resolveClose()));
    await new Promise((resolveClose) => {
      if (controllerSockets.clients.size === 0) {
        controllerSockets.close(() => resolveClose());
      } else {
        setTimeout(() => controllerSockets.close(() => resolveClose()), 25).unref?.();
      }
    });
    db.close();
  }

  let adminOrigin = null;
  return {
    server,
    io,
    db,
    listen,
    close,
    address: () => server.address(),
    publicOrigin: getPublicOrigin,
    adminOrigin: () => adminOrigin,
  };
}

export async function start() {
  const envFile = resolve(import.meta.dirname, '.env');
  if (existsSync(envFile)) process.loadEnvFile(envFile);
  const app = createServer();
  const address = await app.listen();
  const origin = app.publicOrigin();
  process.stdout.write(`OpenUUYC self-host server listening at ${origin} (${typeof address === 'object' ? address.port : ''})\n`);
  if (app.adminOrigin()) process.stdout.write(`Local device administration at ${app.adminOrigin()}\n`);
  const shutdown = () => app.close().finally(() => { process.exitCode = 0; });
  onShutdown(shutdown);
  return app;
}

if (process.argv[1] && resolve(process.argv[1]) === resolve(import.meta.filename)
  && !runWithSqliteFlag(import.meta.url)) {
  start().catch(() => {
    const message = DatabaseSync
      ? 'Server startup failed. Check configuration and database permissions.'
      : SQLITE_UNAVAILABLE;
    process.stderr.write(`${message}\n`);
    process.exitCode = 1;
  });
}
