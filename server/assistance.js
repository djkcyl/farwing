import { randomInt, randomUUID, timingSafeEqual } from 'node:crypto';
import { hashSecret, randomSecret } from './db.js';

const MODES = new Set(['by_password', 'by_confirmation', 'password_confirmation']);
const MODE_TIMEOUT = 5_000;
const CONFIRM_TIMEOUT = 60_000;
const REFRESH_TTL = 120_000;
const SAVED_PATH = '/api/v1/device/share/saved';

export function createAssistance({ db, now, fail, roomId, pushTo, reserveSession, signalingUrl }) {
  const challenges = new Map();
  const grants = new Map();
  const attempts = new Map();

  function connectIdFor(id) {
    db.prepare('INSERT OR IGNORE INTO desktop_devices (device_id) VALUES (?)').run(id);
    const read = db.prepare('SELECT connect_id AS connectId FROM desktop_devices WHERE device_id = ?');
    const write = db.prepare('UPDATE OR IGNORE desktop_devices SET connect_id = ? WHERE device_id = ? AND connect_id IS NULL');
    for (let attempt = 0; attempt < 8; attempt += 1) {
      const { connectId } = read.get(id);
      if (connectId) return connectId;
      const candidate = String(randomInt(100_000_000, 1_000_000_000));
      if (write.run(candidate, id).changes) return candidate;
    }
    fail(503, 'could not allocate assistance device ID');
  }

  function lookup(id) {
    if (typeof id !== 'string' || !/^\d{9}$/u.test(id)) fail(400, 'invalid assistance device ID');
    return db.prepare(`SELECT d.id, d.name, d.controllable, s.connect_id AS connectId, s.version_name AS versionName
      FROM desktop_devices s JOIN devices d ON d.id = s.device_id
      WHERE s.connect_id = ? AND d.revoked_at IS NULL`).get(id);
  }

  function available(host, controller) {
    return host && host.id !== controller.id && host.controllable && roomId(host.id);
  }

  function checkAttempts(controllerId) {
    const timestamp = now();
    const rate = attempts.get(controllerId);
    if (!rate || rate.expiresAt <= timestamp) {
      attempts.set(controllerId, { count: 1, expiresAt: timestamp + 60_000 });
      return;
    }
    if (rate.count >= 20) fail(429, 'too many assistance requests');
    rate.count += 1;
  }

  function closeChallenge(challenge, notify = false) {
    if (challenges.get(challenge.id) !== challenge) return;
    challenges.delete(challenge.id);
    challenge.resolve?.(null);
    if (notify && roomId(challenge.hostId) === challenge.roomId) {
      pushTo(challenge.hostId, 'remote_control_by_confirmation_cancel', { control_id: challenge.id });
    }
  }

  function createChallenge(controller, host, res) {
    const challenge = { id: randomUUID(), controllerId: controller.id, hostId: host.id, roomId: roomId(host.id) };
    challenges.set(challenge.id, challenge);
    res.once('close', () => {
      if (!res.writableEnded) closeChallenge(challenge, true);
    });
    return challenge;
  }

  function ask(challenge, stage, type, data = {}, timeout = MODE_TIMEOUT) {
    return new Promise((resolve) => {
      if (challenges.get(challenge.id) !== challenge || roomId(challenge.hostId) !== challenge.roomId) {
        resolve(null);
        return;
      }
      challenge.stage = stage;
      const timer = setTimeout(() => closeChallenge(challenge, true), timeout);
      timer.unref?.();
      challenge.resolve = (answer) => {
        clearTimeout(timer);
        challenge.resolve = null;
        resolve(answer);
      };
      if (!pushTo(challenge.hostId, type, { control_id: challenge.id, ...data })) closeChallenge(challenge);
    });
  }

  function respond(device, stage, body) {
    const challenge = challenges.get(body.control_id);
    if (!challenge || challenge.hostId !== device.id || challenge.roomId !== roomId(device.id)) {
      fail(404, 'assistance request is no longer available');
    }
    if (challenge.stage === stage) challenge.resolve?.(body);
  }

  function issue(controller, host, controlId, replacementId = null) {
    const { session, ticket } = reserveSession(controller, host.id, {
      native: true,
      assistance: true,
      replacementId,
    });
    grants.set(session.ticketHash, {
      sessionId: session.id,
      hostId: host.id,
      controllerId: controller.id,
      roomId: session.hostRoomId,
      connectId: host.connectId,
      expiresAt: session.expiresAt,
    });
    return {
      token: ticket,
      share_id: host.connectId,
      device_name: host.name,
      publisher_platform: 1,
      publisher_version_name: host.versionName,
      publisher_device_id: host.id,
      signaling_list: [signalingUrl()],
      ws_connect_timeout_ms: 10_000,
      streamer_retry_delta_ms: 1_000,
      control_id: controlId,
    };
  }

  async function mode(controller, host, res) {
    if (!available(host, controller)) return { can_remote_control: false, control_mode: '' };
    checkAttempts(controller.id);
    const challenge = createChallenge(controller, host, res);
    try {
      const reply = await ask(challenge, 'mode', 'get_control_mode');
      const allowed = reply?.allow_control === true && MODES.has(reply.control_mode);
      return { can_remote_control: allowed, control_mode: allowed ? reply.control_mode : '' };
    } finally {
      closeChallenge(challenge);
    }
  }

  async function joinByCode(controller, host, body, res, send) {
    if (!available(host, controller)) fail(409, 'host is not accepting assistance');
    const code = body.connect_code;
    if (typeof code !== 'string' || !code || Buffer.byteLength(code) > 256 || /[\p{Cc}]/u.test(code)) {
      fail(400, 'assistance code is required', 1133);
    }
    checkAttempts(controller.id);
    const challenge = createChallenge(controller, host, res);
    let waitForConfirmation = false;
    try {
      const salt = randomSecret(24);
      const reply = await ask(challenge, 'sign', 'remote_control', { salt });
      if (reply?.can_remote_control !== true || !MODES.has(reply.control_mode) || reply.control_mode === 'by_confirmation') {
        fail(409, 'host is not accepting password assistance');
      }
      const expected = Buffer.from(hashSecret(salt + code), 'hex');
      const valid = [reply.sign, reply.backup_sign].some((sign) => typeof sign === 'string'
        && /^[a-f0-9]{64}$/u.test(sign) && timingSafeEqual(expected, Buffer.from(sign, 'hex')));
      if (!valid) fail(403, 'assistance code is incorrect', 1131);
      if (reply.control_mode === 'password_confirmation' || reply.need_confirmation === true) {
        challenge.stage = 'verified';
        challenge.expiresAt = now() + CONFIRM_TIMEOUT;
        waitForConfirmation = true;
        send({ control_id: challenge.id }, 1136, 'host confirmation is required');
      } else {
        send(issue(controller, host, challenge.id));
      }
    } finally {
      if (!waitForConfirmation) closeChallenge(challenge);
    }
  }

  async function joinByConfirmation(controller, host, body, res) {
    if (!available(host, controller)) fail(409, 'host is not accepting assistance');
    const id = body.control_id;
    let challenge;
    if (id === '') {
      const status = await mode(controller, host, res);
      if (!status.can_remote_control || status.control_mode !== 'by_confirmation') {
        fail(403, 'assistance code is required', 1133);
      }
      if (res.destroyed) return;
      challenge = createChallenge(controller, host, res);
    } else {
      challenge = challenges.get(id);
      if (!challenge || challenge.stage !== 'verified' || challenge.expiresAt <= now()
        || challenge.controllerId !== controller.id || challenge.hostId !== host.id || challenge.roomId !== roomId(host.id)) {
        fail(403, 'assistance verification expired', 1133);
      }
      res.once('close', () => {
        if (!res.writableEnded) closeChallenge(challenge, true);
      });
    }
    try {
      const reply = await ask(challenge, 'confirm', 'remote_control_by_confirmation', { name: controller.name }, CONFIRM_TIMEOUT);
      if (reply?.allow_control !== true) fail(403, 'host did not approve assistance');
      if (reply.need_password === true && id === '') fail(403, 'assistance code is required', 1133);
      return issue(controller, host, challenge.id);
    } finally {
      closeChallenge(challenge);
    }
  }

  function saved(controllerId, kind) {
    const time = kind === 'favorites' ? 'favorited_at' : 'last_connected_at';
    const rows = db.prepare(`SELECT s.host_id AS publisher_device_id, d.connect_id, s.remark, s.last_connected_at,
        s.favorited_at FROM desktop_saved_devices s JOIN desktop_devices d ON d.device_id = s.host_id
      JOIN devices h ON h.id = s.host_id WHERE s.controller_id = ? AND h.revoked_at IS NULL
        AND s.${time} IS NOT NULL ORDER BY s.${time} DESC, s.host_id`).all(controllerId);
    return { saved: rows.map((row) => ({ ...row, is_favorite: row.favorited_at !== null })) };
  }

  function manageSaved(device, method, path, body) {
    const match = /^(favorites|recent)(?:\/([a-f0-9]{16}))?$/u.exec(path.slice(SAVED_PATH.length + 1));
    if (!match) fail(404, 'invalid saved devices endpoint');
    const [, kind, hostId] = match;
    if (method === 'GET' && !hostId) return saved(device.id, kind);
    if (method === 'POST' && kind === 'favorites' && !hostId) {
      if (!Array.isArray(body.favorites) || body.favorites.length !== 1) fail(400, 'expected one favorite');
      const item = body.favorites[0];
      const host = lookup(item?.connect_id);
      const remark = item?.remark;
      if (!host || host.id === device.id) fail(404, 'assistance device was not found');
      if (typeof remark !== 'string' || Buffer.byteLength(remark) > 240 || /[\p{Cc}]/u.test(remark)) fail(400, 'invalid remark');
      db.prepare(`INSERT INTO desktop_saved_devices (controller_id, host_id, remark, favorited_at) VALUES (?, ?, ?, ?)
        ON CONFLICT(controller_id, host_id) DO UPDATE SET remark = excluded.remark,
          favorited_at = COALESCE(desktop_saved_devices.favorited_at, excluded.favorited_at)`)
        .run(device.id, host.id, remark.trim(), Math.floor(now() / 1_000));
      return saved(device.id, kind);
    }
    if (method === 'DELETE' && (hostId || kind === 'recent')) {
      if (kind === 'favorites') {
        db.prepare('UPDATE desktop_saved_devices SET favorited_at = NULL, remark = ? WHERE controller_id = ? AND host_id = ?')
          .run('', device.id, hostId);
      } else if (hostId) {
        db.prepare('UPDATE desktop_saved_devices SET last_connected_at = NULL WHERE controller_id = ? AND host_id = ?')
          .run(device.id, hostId);
      } else {
        db.prepare('UPDATE desktop_saved_devices SET last_connected_at = NULL WHERE controller_id = ?').run(device.id);
      }
      db.prepare('DELETE FROM desktop_saved_devices WHERE controller_id = ? AND favorited_at IS NULL AND last_connected_at IS NULL').run(device.id);
      return {};
    }
    fail(404, 'invalid saved devices endpoint');
  }

  async function request(device, method, path, body, res, send) {
    if (path.startsWith(`${SAVED_PATH}/`)) {
      send(manageSaved(device, method, path, body));
      return true;
    }
    if (method !== 'POST') return false;
    switch (path) {
      case '/api/v1/device/share/info/refresh':
        send({ connect_id: connectIdFor(device.id) });
        return true;
      case '/api/v2/room/share/upload_control_mode':
        respond(device, 'mode', body);
        break;
      case '/api/v2/room/share/upload_sign':
        respond(device, 'sign', body);
        break;
      case '/api/v2/room/share/confirmation':
        respond(device, 'confirm', body);
        break;
      case '/api/v2/room/share/control_mode':
        send(await mode(device, lookup(body.connect_id), res));
        return true;
      case '/api/v2/room/join/share/by_code':
        await joinByCode(device, lookup(body.connect_id), body, res, send);
        return true;
      case '/api/v2/room/join/share/by_confirmation': {
        const reply = await joinByConfirmation(device, lookup(body.connect_id), body, res);
        if (!res.destroyed) send(reply);
        return true;
      }
      case '/api/v2/room/share/cancel_remote_assist': {
        const host = lookup(body.connect_id);
        for (const challenge of challenges.values()) {
          if (challenge.controllerId === device.id && challenge.hostId === host?.id) closeChallenge(challenge, true);
        }
        break;
      }
      case '/api/v1/room/join/refetch': {
        const hash = typeof body.token === 'string' && hashSecret(body.token);
        const grant = grants.get(hash);
        if (!grant || grant.controllerId !== device.id || grant.expiresAt <= now() || roomId(grant.hostId) !== grant.roomId) {
          fail(403, 'assistance session is no longer available');
        }
        const host = lookup(grant.connectId);
        const status = await mode(device, host, res);
        if (!status.can_remote_control || grants.get(hash) !== grant) fail(403, 'assistance session is no longer available');
        send(issue(device, host, '', grant.sessionId));
        grants.delete(hash);
        return true;
      }
      default:
        return false;
    }
    send({});
    return true;
  }

  function started(session) {
    const grant = grants.get(session.ticketHash);
    if (!grant) return;
    grant.expiresAt = Infinity;
    db.prepare(`INSERT INTO desktop_saved_devices (controller_id, host_id, last_connected_at) VALUES (?, ?, ?)
      ON CONFLICT(controller_id, host_id) DO UPDATE SET last_connected_at = excluded.last_connected_at`)
      .run(session.controllerId, session.hostId, Math.floor(now() / 1_000));
  }

  function ended(session) {
    const grant = grants.get(session.ticketHash);
    if (grant) grant.expiresAt = now() + REFRESH_TTL;
  }

  function cancelForDevice(id) {
    for (const challenge of challenges.values()) {
      if (challenge.hostId === id || challenge.controllerId === id) closeChallenge(challenge, true);
    }
    for (const [key, grant] of grants) {
      if (grant.hostId === id || grant.controllerId === id) grants.delete(key);
    }
  }

  function prune() {
    const timestamp = now();
    for (const challenge of challenges.values()) {
      if (challenge.expiresAt <= timestamp && challenge.stage === 'verified') closeChallenge(challenge, true);
    }
    for (const [key, grant] of grants) {
      if (grant.expiresAt <= timestamp) grants.delete(key);
    }
    for (const [key, rate] of attempts) {
      if (rate.expiresAt <= timestamp) attempts.delete(key);
    }
  }

  function close() {
    for (const challenge of challenges.values()) closeChallenge(challenge);
  }

  return { request, started, ended, cancelForDevice, prune, close };
}
