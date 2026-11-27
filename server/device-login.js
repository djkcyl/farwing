import { hashSecret, randomSecret } from './db.js';

const lifetime = 5 * 60_000;
const retryWindow = 30_000;
const qrSecret = /^[A-Za-z0-9_-]{43}$/u;
export const DEVICE_LOGIN_APPROVAL_LIFETIME = 24 * 60 * 60_000;

export function createDeviceLogin({ db, owner, now, fail, portalOrigin, pollMs = 50_000, sessionMs }) {
  const requests = new Map();
  const ownerId = owner.id;

  function ownsDevice(id) {
    return db.prepare(`SELECT d.id FROM account_devices ad JOIN devices d ON d.id = ad.device_id
      JOIN address_members m ON m.device_id = d.id
      WHERE d.id = ? AND ad.account_id = ? AND d.revoked_at IS NULL`).get(id, ownerId);
  }

  function accountDevice(id) {
    return db.prepare(`SELECT d.id FROM account_device_tokens t
      JOIN devices d ON d.id = t.device_id AND d.token_hash = t.installation_hash
      JOIN account_devices ad ON ad.device_id = d.id AND ad.account_id = t.account_id
      JOIN address_members m ON m.device_id = d.id
      WHERE d.id = ? AND t.account_id = ? AND d.revoked_at IS NULL AND t.expires_at > ?`).get(id, ownerId, now());
  }

  function authenticate(token) {
    return db.prepare(`SELECT d.id, d.role, d.platform, d.name, d.bound_host_id AS boundHostId,
      d.controllable, d.revoked_at AS revokedAt, 1 AS addressMember
      FROM account_device_tokens t JOIN devices d ON d.id = t.device_id AND d.token_hash = t.installation_hash
      JOIN account_devices ad ON ad.device_id = d.id AND ad.account_id = t.account_id
      JOIN address_members m ON m.device_id = d.id
      WHERE t.token_hash = ? AND t.account_id = ? AND t.expires_at > ? AND d.revoked_at IS NULL`)
      .get(hashSecret(token), ownerId, now());
  }

  function loginDevice(installationHash) {
    const device = db.prepare(`SELECT d.id, d.name, d.platform, d.revoked_at AS revokedAt,
        ad.account_id AS accountId FROM devices d JOIN address_members m ON m.device_id = d.id
      LEFT JOIN account_devices ad ON ad.device_id = d.id WHERE d.token_hash = ?`).get(installationHash);
    if (device && device.revokedAt === null) return { ...device, approved: true };
    const entry = db.prepare(`SELECT device_id AS id, name, platform, approved_at IS NOT NULL AS approved
      FROM address_enrollment_requests WHERE token_hash = ? AND requested_at > ?`)
      .get(installationHash, now() - DEVICE_LOGIN_APPROVAL_LIFETIME);
    if (entry && (!device || entry.id === device.id)) return entry;
    fail(401, '设备尚未登记，请重新连接', 1006);
  }

  function status(entry) {
    if (entry.canceled || entry.expiresAt <= now()) return 3;
    if (!entry.scanned) return 1;
    if (!entry.confirmed) return 2;
    return loginDevice(entry.installationHash).approved ? 4 : 2;
  }

  function get(id) {
    const entry = typeof id === 'string' && qrSecret.test(id) && requests.get(id);
    if (!entry) fail(404, '扫码请求已失效');
    return entry;
  }

  function fromUrl(value) {
    if (typeof value !== 'string' || value.length > 512) fail(400, '扫码链接格式无效');
    const url = URL.parse(value);
    const id = url?.searchParams.get('qr');
    const entry = get(id);
    if (entry.url !== value) fail(400, '扫码链接格式无效');
    return entry;
  }

  function generate(installationHash) {
    const device = loginDevice(installationHash);
    if (device.accountId && device.accountId !== ownerId) fail(403, '设备已关联其他账号');
    for (const [id, entry] of requests) {
      if (entry.expiresAt <= now() || entry.installationHash === installationHash) {
        entry.canceled = true;
        requests.delete(id);
      }
    }
    if (requests.size >= 1024) fail(429, '扫码请求过多，请稍后重试');
    const id = randomSecret();
    const url = new URL('/account', portalOrigin());
    url.searchParams.set('qr', id);
    const token = randomSecret();
    const pollTicket = randomSecret();
    requests.set(id, { url: url.href, device, installationHash, tokenHash: hashSecret(token),
      pollHash: hashSecret(pollTicket), expiresAt: now() + lifetime });
    return { qrcode_jump_url: url.href, token, status_query_ticket: pollTicket };
  }

  function matches(entry, installationHash, token, expectedHash) {
    if (entry.installationHash !== installationHash || typeof token !== 'string'
      || !qrSecret.test(token) || hashSecret(token) !== expectedHash) fail(403, '扫码票据无效');
  }

  async function poll(res, body, installationHash) {
    const entry = fromUrl(body.qrcode_jump_url);
    matches(entry, installationHash, body.token, entry.pollHash);
    if (![1, 2].includes(body.login_status)) fail(400, '扫码状态无效');
    if (entry.polling) fail(429, '该设备已有扫码查询');
    const until = Date.now() + pollMs;
    const abort = new AbortController();
    const close = () => abort.abort();
    res.once('close', close);
    entry.polling = true;
    try {
      for (;;) {
        const state = status(entry);
        if (res.destroyed || state !== body.login_status || Date.now() >= until) return { login_status: state };
        await new Promise((resolve) => {
          const finish = () => {
            clearTimeout(timer);
            abort.signal.removeEventListener('abort', finish);
            resolve();
          };
          const timer = setTimeout(finish, Math.min(250, Math.max(1, until - Date.now())));
          abort.signal.addEventListener('abort', finish, { once: true });
        });
      }
    } finally {
      entry.polling = false;
      res.off('close', close);
    }
  }

  function action(accountId, selector, action, native = false) {
    if (accountId !== ownerId) fail(403, '仅所有者账号可以确认登录');
    const entry = native ? fromUrl(selector) : get(selector);
    if (entry.expiresAt <= now() || entry.canceled || entry.exchange) fail(409, '扫码请求已结束');
    if (action !== 'scan' && action !== 'confirm' && action !== 'cancel') fail(400, '扫码操作无效');
    loginDevice(entry.installationHash);
    entry.scanned = true;
    if (action === 'confirm') entry.confirmed = true;
    if (action === 'cancel') entry.canceled = true;
    return { device_id: entry.device.id, name: entry.device.name, platform: entry.device.platform, login_status: status(entry) };
  }

  function retireRooms(id, reason) {
    db.prepare('UPDATE room_tickets SET retired_at = ? WHERE device_id = ? AND retired_at IS NULL').run(now(), id);
    db.prepare(`UPDATE sessions SET ended_at = ?, end_reason = ?
      WHERE ended_at IS NULL AND (host_id = ? OR controller_id = ?)`).run(now(), reason, id, id);
  }

  function exchange(body, installationHash) {
    const entry = fromUrl(body.qrcode_jump_url);
    matches(entry, installationHash, body.token, entry.tokenHash);
    if (status(entry) !== 4) fail(403, '设备登录尚未获准');
    const device = loginDevice(installationHash);
    if (device.revokedAt !== null || device.id !== entry.device.id) fail(403, '请先完成设备登记');
    if (entry.exchange) {
      if (entry.exchange.result && entry.exchange.until > now() && authenticate(entry.exchange.result.token)) {
        return { login: entry.exchange.result, deviceId: device.id, replay: true };
      }
      fail(409, '扫码票据已兑换，请重新扫码');
    }
    if (device.accountId && device.accountId !== ownerId) fail(403, '设备已关联其他账号');
    const token = randomSecret();
    db.exec('BEGIN IMMEDIATE');
    try {
      const current = loginDevice(installationHash);
      if (current.revokedAt !== null || current.id !== device.id || (current.accountId && current.accountId !== ownerId)) {
        fail(403, '设备登记或账号绑定已变更');
      }
      db.prepare('INSERT INTO account_devices (device_id, account_id) VALUES (?, ?) ON CONFLICT(device_id) DO NOTHING')
        .run(device.id, ownerId);
      db.prepare(`INSERT INTO account_device_tokens (device_id, account_id, token_hash, installation_hash, expires_at)
        VALUES (?, ?, ?, ?, ?) ON CONFLICT(device_id) DO UPDATE SET
        account_id = excluded.account_id, token_hash = excluded.token_hash,
        installation_hash = excluded.installation_hash, expires_at = excluded.expires_at`)
        .run(device.id, ownerId, hashSecret(token), installationHash, now() + sessionMs);
      retireRooms(device.id, 'account_replaced');
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
    const result = { token, user_id: ownerId, nickname: owner.username };
    entry.exchange = { result, until: now() + retryWindow };
    return { login: result, deviceId: device.id, replay: false };
  }

  function invalidate(id) {
    db.exec('BEGIN IMMEDIATE');
    try {
      db.prepare('DELETE FROM account_device_tokens WHERE device_id = ?').run(id);
      retireRooms(id, 'account_logged_out');
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
    for (const entry of requests.values()) {
      if (entry.device.id === id) {
        entry.canceled = true;
        delete entry.exchange;
      }
    }
  }

  function prune() {
    for (const [id, entry] of requests) {
      if (entry.expiresAt <= now()) requests.delete(id);
      else if (entry.exchange?.result && entry.exchange.until <= now()) delete entry.exchange.result;
    }
  }

  return { ownsDevice, accountDevice, authenticate, generate, poll, action, exchange, invalidate, prune };
}
