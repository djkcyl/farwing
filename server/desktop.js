import { BlockList, isIP } from 'node:net';
import { hashSecret } from './db.js';

const DEVICE_ID = /^[a-f0-9]{16}$/u;
export const ADDRESS_TOKEN = /^[a-f0-9]{64}$/u;

const NATIVE_PATHS = new Map([
  ['/api/v1/device/windows/init', '/register'],
  ['/api/v1/user/info', '/profile'],
  ['/api/v1/user/logout', '/disconnect'],
  ['/api/v1/device/list', '/devices'],
  ['/api/v1/device/groups/of/my', '/groups'],
  ['/api/v1/device/controllable', '/controllable'],
  ['/api/v1/room/create', '/room'],
  ['/api/v1/tool/configures_with_version', '/configures'],
]);

export function nativeDesktopPath(path) {
  const exact = NATIVE_PATHS.get(path);
  if (exact) return exact;
  const join = /^\/api\/v1\/room\/join\/by_device\/([a-f0-9]{16})$/u.exec(path);
  if (join) return `/join/${join[1]}`;
  const device = /^\/api\/v1\/device\/([a-f0-9]{16})(\/(?:detail|unbind))?$/u.exec(path);
  if (device) return `/devices/${device[1]}${device[2] ?? ''}`;
  return path;
}

export function windowsHardwareDetails(body) {
  const text = (value) => typeof value === 'string' && Buffer.byteLength(value) <= 512 && !/[\p{Cc}]/u.test(value) ? value.trim() : '';
  const video = Array.isArray(body.video) ? body.video.slice(0, 8).map(text).filter(Boolean).join(' / ') : '';
  return [
    ['系统', text(body.os)], ['处理器', text(body.cpu)], ['显卡', video], ['显示器', text(body.screen)],
    ['内存', Number.isSafeInteger(body.memory) && body.memory > 0 ? `${(body.memory / 1024).toFixed(1)} GiB` : ''],
  ].filter(([, value]) => value);
}

export function clientNetworkPolicy(config) {
  if (!config) return null;
  const networks = new BlockList();
  for (const entry of config.split(',').map((value) => value.trim())) {
    const [address, prefix, extra] = entry.split('/');
    const family = isIP(address);
    if (!family || extra !== undefined || (prefix !== undefined && !/^\d{1,3}$/u.test(prefix))) {
      throw new Error(`invalid DESKTOP_CLIENT_NETWORKS entry: ${entry}`);
    }
    const type = family === 4 ? 'ipv4' : 'ipv6';
    if (prefix === undefined) networks.addAddress(address, type);
    else networks.addSubnet(address, Number(prefix), type);
  }
  return (address) => {
    const ip = address?.replace(/^::ffff:(?=\d+\.)/u, '');
    return Boolean(ip && networks.check(ip, isIP(ip) === 4 ? 'ipv4' : 'ipv6'));
  };
}

export function hostedIceServers(config) {
  if (!config) return [];
  const entries = JSON.parse(config);
  if (!Array.isArray(entries) || entries.length > 16) throw new Error('invalid SELF_HOST_ICE_SERVERS');
  return entries.map(({ urls, username = '', credential = '' }) => {
    if (typeof urls !== 'string' || !/^(?:stuns?|turns?):[^\s]+$/u.test(urls)
      || typeof username !== 'string' || typeof credential !== 'string'
      || (/^turns?:/u.test(urls) && (!username || !credential))) {
      throw new Error('each ICE server requires one STUN or TURN URL and TURN credentials where applicable');
    }
    return { urls, username, credential };
  });
}

const ENROLLMENT_REQUEST_LIFETIME = 7 * 24 * 60 * 60_000;
const MAX_ENROLLMENT_REQUESTS = 1000;

export function listEnrollmentRequests(db, now = Date.now(), lifetime = ENROLLMENT_REQUEST_LIFETIME) {
  return db.prepare(`SELECT device_id AS id, platform, name, requested_at AS requestedAt, approved_at AS approvedAt
    FROM address_enrollment_requests WHERE requested_at > ? ORDER BY requested_at DESC`)
    .all(now - lifetime);
}

export function approveEnrollmentRequest(db, id, now = Date.now(), lifetime = ENROLLMENT_REQUEST_LIFETIME) {
  return db.prepare(`UPDATE address_enrollment_requests SET approved_at = COALESCE(approved_at, ?)
    WHERE device_id = ? AND requested_at > ?`).run(now, id, now - lifetime).changes === 1;
}

export function listEnrollmentGrants(db, now = Date.now()) {
  return db.prepare(`SELECT device_id AS id, expires_at AS expiresAt FROM address_enrollment_grants
    WHERE expires_at > ? ORDER BY expires_at DESC, device_id`).all(now);
}

export function preapproveEnrollment(db, id, now = Date.now(), requestLifetime) {
  if (approveEnrollmentRequest(db, id, now, requestLifetime)) return true;
  return db.prepare(`INSERT INTO address_enrollment_grants (device_id, expires_at)
    SELECT ?, ? WHERE NOT EXISTS (SELECT 1 FROM devices WHERE id = ?)
      AND NOT EXISTS (SELECT 1 FROM ios_device_ids WHERE persistent_id = ?)
    ON CONFLICT(device_id) DO UPDATE SET expires_at = excluded.expires_at`)
    .run(id, now + ENROLLMENT_REQUEST_LIFETIME, id, id).changes === 1;
}

export function deleteEnrollmentEntry(db, id) {
  db.exec('BEGIN IMMEDIATE');
  try {
    const requests = db.prepare('DELETE FROM address_enrollment_requests WHERE device_id = ?').run(id).changes;
    const grants = db.prepare('DELETE FROM address_enrollment_grants WHERE device_id = ?').run(id).changes;
    db.exec('COMMIT');
    return requests + grants > 0;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}

function requestEnrollment(db, id, hash, name, platform, timestamp, lifetime, acceptGrant = false) {
  db.prepare('DELETE FROM address_enrollment_requests WHERE requested_at <= ?').run(timestamp - lifetime);
  db.prepare('DELETE FROM address_enrollment_grants WHERE expires_at <= ?').run(timestamp);
  const request = db.prepare(`SELECT token_hash, platform, approved_at FROM address_enrollment_requests
    WHERE device_id = ?`).get(id);
  if (request && (request.token_hash !== hash || request.platform !== platform)) return { denied: true };
  const grant = acceptGrant && db.prepare('SELECT 1 FROM address_enrollment_grants WHERE device_id = ?').get(id);
  if (request?.approved_at != null || grant) {
    db.prepare('DELETE FROM address_enrollment_requests WHERE device_id = ?').run(id);
    db.prepare('DELETE FROM address_enrollment_grants WHERE device_id = ?').run(id);
    return null;
  }
  if (!request) {
    if (db.prepare('SELECT count(*) AS count FROM address_enrollment_requests').get().count >= MAX_ENROLLMENT_REQUESTS) {
      return { queueFull: true };
    }
    db.prepare(`INSERT INTO address_enrollment_requests (device_id, token_hash, platform, name, requested_at)
      VALUES (?, ?, ?, ?, ?)`).run(id, hash, platform, name, timestamp);
  }
  return { pending: true, id };
}

export function enrollAddressDevice(db, token, name, platform, timestamp, persistentId, requireApproval = true, requestLifetime = ENROLLMENT_REQUEST_LIFETIME) {
  const hash = hashSecret(token);
  const initialId = hash.slice(0, 16);
  db.exec('BEGIN IMMEDIATE');
  try {
    const existing = db.prepare(`SELECT d.id, d.token_hash AS tokenHash, d.platform, d.revoked_at AS revokedAt,
        a.device_id IS NOT NULL AS addressMember
      FROM devices d LEFT JOIN address_members a ON a.device_id = d.id
      WHERE d.token_hash = ? OR d.id = ? ORDER BY (d.token_hash = ?) DESC LIMIT 1`).get(hash, initialId, hash);
    const id = existing?.id ?? initialId;
    if (existing) {
      if (!existing.addressMember || existing.tokenHash !== hash || existing.platform !== platform) {
        db.exec('ROLLBACK');
        return null;
      }
      if (existing.revokedAt !== null) {
        // Transfers retain retired credentials on an ID that differs from their original hash prefix.
        if (!requireApproval || id !== initialId) {
          db.exec('ROLLBACK');
          return null;
        }
        const decision = requestEnrollment(db, id, hash, name, platform, timestamp, requestLifetime);
        if (decision) {
          db.exec('COMMIT');
          return decision.denied ? null : decision;
        }
        db.prepare('UPDATE devices SET revoked_at = NULL WHERE id = ?').run(id);
      }
    } else {
      if (db.prepare('SELECT 1 FROM ios_device_ids WHERE persistent_id = ?').get(id)
        || (persistentId !== undefined && (db.prepare('SELECT 1 FROM ios_device_ids WHERE persistent_id = ?').get(persistentId)
          || db.prepare('SELECT 1 FROM devices WHERE id = ?').get(persistentId)))) {
        db.exec('ROLLBACK');
        return { conflict: true };
      }
      if (requireApproval) {
        const decision = requestEnrollment(db, id, hash, name, platform, timestamp, requestLifetime, true);
        if (decision) {
          db.exec('COMMIT');
          return decision.denied ? null : decision;
        }
      }
      db.prepare(`INSERT INTO devices (id, role, platform, name, token_hash, paired_at)
          VALUES (?, ?, ?, ?, ?, ?)`).run(id, platform === 'windows' ? 'host' : 'controller', platform, name, hash, timestamp);
      db.prepare('INSERT INTO address_members (device_id) VALUES (?)').run(id);
    }
    // The authenticated record already holds the canonical ID after a transfer.
    if (persistentId !== undefined && id === initialId) {
      const assigned = db.prepare('SELECT persistent_id FROM ios_device_ids WHERE device_id = ?').get(id);
      if (assigned?.persistent_id !== persistentId && (assigned
        || db.prepare('SELECT 1 FROM devices WHERE id = ? AND id <> ?').get(persistentId, id)
        || !db.prepare('INSERT INTO ios_device_ids VALUES (?, ?) ON CONFLICT DO NOTHING').run(id, persistentId).changes)) {
        db.exec('ROLLBACK');
        return { conflict: true };
      }
    }
    if (existing && platform === 'ios') {
      db.prepare('UPDATE devices SET name = ? WHERE id = ? AND name <> ?').run(name, id, name);
    }
    db.exec('COMMIT');
    return id;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}

export function addressHost(db, id) {
  if (!DEVICE_ID.test(id)) return null;
  return db.prepare(`SELECT d.id, d.name, d.controllable, s.version_name AS versionName,
      s.connect_id AS connectId, s.support_wol AS supportWol, s.lan_info AS lanInfo, s.wallpaper_id AS wallpaperId
    FROM address_members a JOIN devices d ON a.device_id = d.id
    LEFT JOIN desktop_devices s ON s.device_id = d.id
    WHERE d.id = ? AND d.role = 'host' AND d.revoked_at IS NULL`).get(id);
}
