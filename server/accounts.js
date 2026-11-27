import { randomBytes, scrypt, timingSafeEqual } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { isIP } from 'node:net';
import { promisify } from 'node:util';
import { generateAuthenticationOptions, generateRegistrationOptions, verifyAuthenticationResponse, verifyRegistrationResponse } from '@simplewebauthn/server';
import { hashSecret, randomSecret } from './db.js';

const derive = promisify(scrypt);
const sessionLifetime = 7 * 24 * 60 * 60_000;
const challengeLifetime = 5 * 60_000;
const credentialPattern = /^[A-Za-z0-9_-]{43}$/u;
const legacyCookie = 'openuuyc_account';
const secureCookie = '__Host-openuuyc_account';

const assets = new Map([
  ['/account', ['text/html; charset=utf-8', './account/index.html']],
  ['/account/', ['text/html; charset=utf-8', './account/index.html']],
  ['/account/app.js', ['text/javascript; charset=utf-8', './account/app.js']],
  ['/account/style.css', ['text/css; charset=utf-8', './account/style.css']],
  ['/account/webauthn.js', ['text/javascript; charset=utf-8', './node_modules/@simplewebauthn/browser/dist/bundle/index.umd.min.js']],
]);

async function passwordHash(password, salt = randomBytes(16).toString('hex')) {
  const key = await derive(password, salt, 64);
  return `${salt}:${key.toString('hex')}`;
}

export function createAccounts({ db, now, fail, readJson, jsonResponse, getPublicOrigin, configuredOrigin, announceDevice, deviceLogin }) {
  const challenges = new Map();
  const attempts = new Map();
  const staticAssets = new Map([...assets].map(([path, [mime, file]]) => [path, [mime, readFileSync(new URL(file, import.meta.url))]]));
  const portalOrigin = () => configuredOrigin ?? getPublicOrigin();
  const cookieName = () => portalOrigin().startsWith('https:') ? secureCookie : legacyCookie;
  const cookie = (token, maxAge) => `${cookieName()}=${token}; Path=/; HttpOnly; SameSite=Strict; Max-Age=${maxAge}`
    + (cookieName() === secureCookie ? '; Secure' : '');

  function tokenFor(req) {
    const prefix = `${cookieName()}=`;
    const token = (req.headers.cookie ?? '').split(';').map((part) => part.trim()).find((part) => part.startsWith(prefix))?.slice(prefix.length);
    return credentialPattern.test(token ?? '') ? token : null;
  }

  function accountFor(req) {
    const token = tokenFor(req);
    return token && db.prepare(`SELECT a.id, a.username FROM account_sessions s JOIN accounts a ON a.id = s.account_id
      WHERE s.token_hash = ? AND s.expires_at > ?`).get(hashSecret(token), now());
  }

  function requireAccount(req) {
    const account = accountFor(req);
    if (!account) fail(401, '请先登录');
    return account;
  }

  function startSession(req, res, account) {
    const token = randomSecret();
    const timestamp = now();
    db.prepare('DELETE FROM account_sessions WHERE expires_at <= ?').run(timestamp);
    const previous = tokenFor(req);
    if (previous) db.prepare('DELETE FROM account_sessions WHERE token_hash = ?').run(hashSecret(previous));
    db.prepare('INSERT INTO account_sessions VALUES (?, ?, ?)').run(hashSecret(token), account.id, timestamp + sessionLifetime);
    res.setHeader('Set-Cookie', cookie(token, sessionLifetime / 1_000));
    return { username: account.username };
  }

  function usernameFrom(value) {
    const username = typeof value === 'string' ? value.trim().toLowerCase() : '';
    if (!/^[a-z0-9][a-z0-9_.-]{2,31}$/u.test(username)) fail(400, '用户名须为 3–32 位字母、数字、点、下划线或短横线');
    return username;
  }

  function passwordFrom(value, registration) {
    if (typeof value !== 'string' || Buffer.byteLength(value) > 256 || (registration && [...value].length < 12)) {
      fail(400, '密码须至少 12 个字符，最多 256 字节');
    }
    return value;
  }

  function rateLimit(req) {
    const timestamp = now();
    for (const [key, entry] of attempts) if (entry.expires <= timestamp) attempts.delete(key);
    const address = req.socket.remoteAddress;
    const entry = attempts.get(address) ?? { expires: timestamp + 60_000, count: 0 };
    if (++entry.count > 30 || attempts.size >= 4096 && !attempts.has(address)) fail(429, '请求过多，请稍后重试');
    attempts.set(address, entry);
  }

  function relyingParty() {
    const origin = new URL(portalOrigin());
    if (origin.hostname === 'localhost') return 'localhost';
    if (origin.protocol !== 'https:' || isIP(origin.hostname.replace(/^\[|\]$/gu, '')) || !origin.hostname.includes('.')) return null;
    return origin.hostname;
  }

  function stageChallenge(kind, options, account) {
    const timestamp = now();
    for (const [id, pending] of challenges) if (pending.expires <= timestamp) challenges.delete(id);
    if (challenges.size >= 1024) fail(429, '请求过多，请稍后重试');
    const requestId = randomSecret();
    challenges.set(requestId, { kind, options, account, expires: timestamp + challengeLifetime });
    return { requestId, options };
  }

  function takeChallenge(requestId, kind, accountId) {
    const pending = challenges.get(requestId);
    if (!pending || pending.kind !== kind || pending.expires <= now() || (accountId && pending.account.id !== accountId)) {
      fail(400, 'Passkey 请求已失效，请重试');
    }
    challenges.delete(requestId);
    return pending;
  }

  function addAccount(account, password = null) {
    if (!db.prepare('INSERT INTO accounts VALUES (?, ?, ?, ?) ON CONFLICT(username) DO NOTHING')
      .run(account.id, account.username, password, now()).changes) fail(409, '用户名已注册');
  }

  function storePasskey(account, credential) {
    db.prepare('INSERT INTO account_passkeys VALUES (?, ?, ?, ?, ?)').run(
      credential.id, account.id, credential.publicKey, credential.counter, JSON.stringify(credential.transports ?? []),
    );
  }

  async function passkey(req, res, path, body) {
    const rpID = relyingParty();
    if (!rpID) fail(409, 'Passkey 需要通过受信任证书的 HTTPS 域名访问');
    const verification = { expectedOrigin: portalOrigin(), expectedRPID: rpID, requireUserVerification: true };
    const newAccount = path.includes('/register/');
    if (path.endsWith('/registration/options') || path.endsWith('/register/options')) {
      const account = newAccount ? { id: randomSecret(), username: usernameFrom(body.username) } : requireAccount(req);
      const existing = db.prepare('SELECT id, transports FROM account_passkeys WHERE account_id = ?').all(account.id);
      const options = await generateRegistrationOptions({
        rpName: 'OpenUUYC', rpID, userName: account.username, userID: Buffer.from(account.id, 'base64url'),
        attestationType: 'none', authenticatorSelection: { residentKey: 'required', userVerification: 'required' },
        excludeCredentials: existing.map(({ id, transports }) => ({ id, transports: JSON.parse(transports) })),
      });
      return stageChallenge(newAccount ? 'register' : 'add', options, account);
    }
    if (path.endsWith('/registration/verify') || path.endsWith('/register/verify')) {
      const accountId = newAccount ? null : requireAccount(req).id;
      const pending = takeChallenge(body.requestId, newAccount ? 'register' : 'add', accountId);
      const result = await verifyRegistrationResponse({ ...verification, response: body.credential,
        expectedChallenge: pending.options.challenge }).catch(() => null);
      if (!result?.verified) fail(400, 'Passkey 注册验证失败');
      db.exec('BEGIN IMMEDIATE');
      try {
        if (newAccount) addAccount(pending.account);
        if (db.prepare('SELECT 1 FROM account_passkeys WHERE id = ?').get(result.registrationInfo.credential.id)) {
          fail(409, '该 Passkey 已注册');
        }
        storePasskey(pending.account, result.registrationInfo.credential);
        db.exec('COMMIT');
      } catch (error) {
        db.exec('ROLLBACK');
        throw error;
      }
      return newAccount ? startSession(req, res, pending.account) : {};
    }
    if (path === '/v1/account/passkey/login/options') {
      const options = await generateAuthenticationOptions({ rpID, userVerification: 'required' });
      return stageChallenge('login', options);
    }
    if (path === '/v1/account/passkey/login/verify') {
      const pending = takeChallenge(body.requestId, 'login');
      if (typeof body.credential?.id !== 'string') fail(400, 'Passkey 登录验证失败');
      const key = db.prepare(`SELECT p.*, a.username FROM account_passkeys p JOIN accounts a ON a.id = p.account_id
        WHERE p.id = ?`).get(body.credential.id);
      if (!key || body.credential.response?.userHandle !== key.account_id) fail(401, 'Passkey 登录验证失败');
      const result = await verifyAuthenticationResponse({ ...verification, response: body.credential,
        expectedChallenge: pending.options.challenge, credential: { id: key.id, publicKey: key.public_key,
          counter: key.counter, transports: JSON.parse(key.transports) } }).catch(() => null);
      if (!result?.verified || !db.prepare('UPDATE account_passkeys SET counter = ? WHERE id = ? AND counter = ?')
        .run(result.authenticationInfo.newCounter, key.id, key.counter).changes) fail(401, 'Passkey 登录验证失败');
      return startSession(req, res, { id: key.account_id, username: key.username });
    }
    fail(404, 'not found');
  }

  async function request(req, res, path) {
    const asset = staticAssets.get(path);
    if (asset && ['GET', 'HEAD'].includes(req.method)) {
      const [mime, contents] = asset;
      res.writeHead(200, {
        'Content-Type': mime, 'Content-Length': contents.length, 'Cache-Control': 'no-store',
        'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        'X-Content-Type-Options': 'nosniff', 'Referrer-Policy': 'no-referrer',
      });
      res.end(req.method === 'HEAD' ? undefined : contents);
      return;
    }
    if (req.method === 'GET' && path === '/v1/account/status') {
      const account = accountFor(req);
      const devices = account ? db.prepare(`SELECT COALESCE(i.persistent_id, d.id) AS device_id, d.name, d.platform
        FROM account_devices ad JOIN devices d ON d.id = ad.device_id LEFT JOIN ios_device_ids i ON i.device_id = d.id
        WHERE ad.account_id = ? AND d.revoked_at IS NULL ORDER BY d.name, d.id`).all(account.id) : [];
      jsonResponse(res, 200, { username: account?.username ?? null, devices, passkey: Boolean(relyingParty()),
        ...(deviceLogin && { deviceLogin: true }) });
      return;
    }
    if (req.method !== 'POST') fail(404, 'not found');
    if (req.headers.origin !== portalOrigin()) fail(403, 'origin is not allowed');
    rateLimit(req);
    const body = await readJson(req);
    let result;
    if (deviceLogin && (path === '/v1/account/register' || path.startsWith('/v1/account/passkey/register/')
      || path === '/v1/account/device-code' || path === '/v1/account/device-transfer-code')) fail(404, 'not found');
    if (path === '/v1/account/qr') {
      if (!deviceLogin) fail(404, 'not found');
      result = deviceLogin.action(requireAccount(req).id, body.id, body.action);
    } else if (path === '/v1/account/register') {
      const account = { id: randomSecret(), username: usernameFrom(body.username) };
      addAccount(account, await passwordHash(passwordFrom(body.password, true)));
      result = startSession(req, res, account);
    } else if (path === '/v1/account/login') {
      const account = db.prepare('SELECT * FROM accounts WHERE username = ?').get(usernameFrom(body.username));
      const expected = account?.password_hash ?? `${'0'.repeat(32)}:${'0'.repeat(128)}`;
      const actual = await passwordHash(passwordFrom(body.password, false), expected.split(':', 1)[0]);
      if (!timingSafeEqual(Buffer.from(actual), Buffer.from(expected)) || !account?.password_hash) fail(401, '用户名或密码错误');
      result = startSession(req, res, account);
    } else if (path === '/v1/account/logout') {
      const token = tokenFor(req);
      if (token) db.prepare('DELETE FROM account_sessions WHERE token_hash = ?').run(hashSecret(token));
      res.setHeader('Set-Cookie', cookie('', 0));
      result = {};
    } else if (path === '/v1/account/device-transfer-code') {
      const account = requireAccount(req);
      if (typeof body.device_id !== 'string' || !/^[a-f0-9]{16}$/u.test(body.device_id)) fail(400, '设备 ID 格式无效');
      const target = db.prepare(`SELECT d.id FROM account_devices ad JOIN devices d ON d.id = ad.device_id
        JOIN address_members m ON m.device_id = d.id LEFT JOIN ios_device_ids i ON i.device_id = d.id
        WHERE ad.account_id = ? AND COALESCE(i.persistent_id, d.id) = ?
          AND d.role = 'controller' AND d.platform = 'ios' AND d.revoked_at IS NULL`).get(account.id, body.device_id);
      if (!target) fail(404, '没有找到此账号下的 iOS 设备');
      const code = randomBytes(16).toString('hex');
      const timestamp = now();
      db.exec('BEGIN IMMEDIATE');
      try {
        db.prepare('DELETE FROM account_device_transfers WHERE expires_at <= ? OR device_id = ?').run(timestamp, target.id);
        db.prepare('INSERT INTO account_device_transfers VALUES (?, ?, ?, ?)')
          .run(hashSecret(code), target.id, account.id, timestamp + challengeLifetime);
        db.exec('COMMIT');
      } catch (error) {
        db.exec('ROLLBACK');
        throw error;
      }
      result = { code, expires_in: challengeLifetime / 1_000 };
    } else if (path === '/v1/account/device-code') {
      const account = requireAccount(req);
      const code = randomBytes(16).toString('hex');
      const timestamp = now();
      db.prepare('DELETE FROM account_device_codes WHERE expires_at <= ? OR account_id = ?').run(timestamp, account.id);
      db.prepare('INSERT INTO account_device_codes VALUES (?, ?, ?)').run(hashSecret(code), account.id, timestamp + challengeLifetime);
      result = { code, expires_in: challengeLifetime / 1_000 };
    } else if (path.startsWith('/v1/account/passkey/')) {
      result = await passkey(req, res, path, body);
    } else {
      fail(404, 'not found');
    }
    jsonResponse(res, 200, result);
  }

  function deviceAccount(id) {
    return db.prepare(`SELECT a.username FROM account_devices ad JOIN accounts a ON a.id = ad.account_id WHERE ad.device_id = ?`).get(id);
  }

  function linkDevice(device, value) {
    if (typeof value !== 'string' || !/^[a-f0-9]{32}$/u.test(value)) fail(400, '设备关联码格式无效');
    db.exec('BEGIN IMMEDIATE');
    try {
      const grant = db.prepare('DELETE FROM account_device_codes WHERE code_hash = ? AND expires_at > ? RETURNING account_id')
        .get(hashSecret(value), now());
      if (!grant) fail(400, '设备关联码无效或已过期');
      const owner = db.prepare('SELECT account_id FROM account_devices WHERE device_id = ?').get(device.id);
      if (owner && owner.account_id !== grant.account_id) fail(409, '设备已关联其他账号，请先在 App 内解除关联');
      db.prepare('INSERT INTO account_devices VALUES (?, ?) ON CONFLICT(device_id) DO NOTHING').run(device.id, grant.account_id);
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
    announceDevice(device.id);
    return deviceAccount(device.id);
  }

  function unlinkDevice(device) {
    db.prepare('DELETE FROM account_devices WHERE device_id = ?').run(device.id);
    announceDevice(device.id);
  }

  function transferDevice(device, code) {
    if (typeof code !== 'string' || !/^[a-f0-9]{32}$/u.test(code)) fail(400, '设备迁移码格式无效');
    const timestamp = now();
    db.exec('BEGIN IMMEDIATE');
    try {
      const target = db.prepare(`SELECT d.id, d.token_hash, ad.account_id FROM account_device_transfers t
        JOIN account_devices ad ON t.device_id = ad.device_id AND t.account_id = ad.account_id
        JOIN devices d ON t.device_id = d.id JOIN address_members m ON m.device_id = d.id
        WHERE t.code_hash = ? AND t.expires_at > ? AND d.revoked_at IS NULL
          AND d.platform = 'ios' AND d.role = 'controller'`).get(hashSecret(code), timestamp);
      if (!target) fail(400, '设备迁移码无效或已过期');
      if (target.id === device.id) fail(409, '此安装已使用该设备身份');
      const source = db.prepare(`SELECT d.token_hash, ad.account_id FROM devices d
        LEFT JOIN account_devices ad ON d.id = ad.device_id WHERE d.id = ? AND d.revoked_at IS NULL`).get(device.id);
      if (!source || (source.account_id && source.account_id !== target.account_id)) fail(409, '当前安装已关联其他账号');
      db.prepare('DELETE FROM account_device_transfers WHERE code_hash = ?').run(hashSecret(code));
      db.prepare('DELETE FROM account_devices WHERE device_id = ?').run(device.id);
      // Keep the old credential on the revoked row so an old installation cannot enroll it again.
      const updateToken = db.prepare('UPDATE devices SET token_hash = ? WHERE id = ?');
      updateToken.run(hashSecret(randomSecret()), device.id);
      updateToken.run(source.token_hash, target.id);
      updateToken.run(target.token_hash, device.id);
      db.prepare('UPDATE devices SET revoked_at = ? WHERE id = ?').run(timestamp, device.id);
      db.prepare('INSERT INTO ios_device_ids VALUES (?, ?) ON CONFLICT(device_id) DO NOTHING').run(target.id, target.id);
      db.prepare('UPDATE room_tickets SET retired_at = ? WHERE device_id IN (?, ?) AND retired_at IS NULL')
        .run(timestamp, device.id, target.id);
      db.prepare(`UPDATE sessions SET ended_at = ?, end_reason = 'device_transferred'
        WHERE controller_id IN (?, ?) AND ended_at IS NULL`).run(timestamp, device.id, target.id);
      const { persistent_id } = db.prepare('SELECT persistent_id FROM ios_device_ids WHERE device_id = ?').get(target.id);
      db.exec('COMMIT');
      return { id: target.id, persistent_id, ...deviceAccount(target.id) };
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
  }

  return { request, portalOrigin, deviceAccount, linkDevice, unlinkDevice, transferDevice };
}
