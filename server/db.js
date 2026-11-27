import { randomBytes, createHash } from 'node:crypto';
import { chmodSync, mkdirSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { DatabaseSync, SQLITE_UNAVAILABLE } from './sqlite-runtime.js';

export const DEFAULT_DB_PATH = resolve(import.meta.dirname, 'data/devices.sqlite');

export function hashSecret(secret) {
  return createHash('sha256').update(secret, 'utf8').digest('hex');
}

export function randomSecret(bytes = 32) {
  return randomBytes(bytes).toString('base64url');
}

export function randomDeviceId() {
  return randomBytes(8).toString('hex');
}

export function openDatabase(dbPath = DEFAULT_DB_PATH) {
  if (!DatabaseSync) throw new Error(SQLITE_UNAVAILABLE);
  mkdirSync(dirname(resolve(dbPath)), { recursive: true, mode: 0o700 });
  const db = new DatabaseSync(resolve(dbPath));
  chmodSync(resolve(dbPath), 0o600);
  db.exec('PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA busy_timeout = 5000;');
  db.exec(`
    CREATE TABLE IF NOT EXISTS devices (
      id TEXT PRIMARY KEY,
      role TEXT NOT NULL CHECK (role IN ('host', 'controller')),
      platform TEXT NOT NULL CHECK (platform IN ('windows', 'ios', 'web')),
      name TEXT NOT NULL,
      token_hash TEXT NOT NULL UNIQUE,
      bound_host_id TEXT REFERENCES devices(id),
      controllable INTEGER NOT NULL DEFAULT 0 CHECK (controllable IN (0, 1)),
      paired_at INTEGER NOT NULL,
      revoked_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS devices_host_idx ON devices(bound_host_id);
    CREATE TABLE IF NOT EXISTS address_members (
      device_id TEXT PRIMARY KEY REFERENCES devices(id)
    );
    CREATE TABLE IF NOT EXISTS address_enrollment_requests (
      device_id TEXT PRIMARY KEY,
      token_hash TEXT NOT NULL UNIQUE,
      platform TEXT NOT NULL CHECK (platform IN ('windows', 'ios', 'web')),
      name TEXT NOT NULL,
      requested_at INTEGER NOT NULL,
      approved_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS address_enrollment_grants (
      device_id TEXT PRIMARY KEY,
      expires_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS desktop_devices (
      device_id TEXT PRIMARY KEY REFERENCES address_members(device_id),
      version_name TEXT NOT NULL DEFAULT '',
      connect_id TEXT UNIQUE,
      support_wol INTEGER NOT NULL DEFAULT 0 CHECK (support_wol IN (0, 1)),
      lan_info TEXT,
      wallpaper_id TEXT
    );
    CREATE TABLE IF NOT EXISTS desktop_wallpapers (
      id TEXT PRIMARY KEY,
      device_id TEXT NOT NULL REFERENCES desktop_devices(device_id),
      mime TEXT NOT NULL,
      content BLOB NOT NULL,
      expires_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS desktop_wallpapers_device_idx ON desktop_wallpapers(device_id);
    CREATE TABLE IF NOT EXISTS desktop_hardware (
      device_id TEXT PRIMARY KEY REFERENCES address_members(device_id),
      details TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS ios_video_preferences (
      device_id TEXT PRIMARY KEY REFERENCES address_members(device_id),
      preferences TEXT NOT NULL
    );
    CREATE TABLE IF NOT EXISTS desktop_saved_devices (
      controller_id TEXT NOT NULL REFERENCES devices(id),
      host_id TEXT NOT NULL REFERENCES devices(id),
      remark TEXT NOT NULL DEFAULT '',
      last_connected_at INTEGER,
      favorited_at INTEGER,
      PRIMARY KEY (controller_id, host_id)
    );
    CREATE TABLE IF NOT EXISTS pairing_codes (
      code_hash TEXT PRIMARY KEY,
      role TEXT NOT NULL CHECK (role IN ('host', 'controller')),
      bound_host_id TEXT REFERENCES devices(id),
      expires_at INTEGER NOT NULL,
      created_at INTEGER NOT NULL,
      consumed_at INTEGER
    );
    CREATE TABLE IF NOT EXISTS room_tickets (
      id TEXT PRIMARY KEY,
      device_id TEXT NOT NULL REFERENCES devices(id),
      token_hash TEXT NOT NULL UNIQUE,
      reconnect_key_hash TEXT,
      expires_at INTEGER NOT NULL,
      created_at INTEGER NOT NULL,
      retired_at INTEGER
    );
    CREATE INDEX IF NOT EXISTS room_tickets_device_idx ON room_tickets(device_id, retired_at);
    CREATE TABLE IF NOT EXISTS sessions (
      id TEXT PRIMARY KEY,
      host_id TEXT NOT NULL REFERENCES devices(id),
      controller_id TEXT NOT NULL REFERENCES devices(id),
      ticket_hash TEXT NOT NULL UNIQUE,
      expires_at INTEGER NOT NULL,
      created_at INTEGER NOT NULL,
      used_at INTEGER,
      ended_at INTEGER,
      end_reason TEXT
    );
    CREATE INDEX IF NOT EXISTS sessions_host_idx ON sessions(host_id, ended_at);
    CREATE INDEX IF NOT EXISTS sessions_controller_idx ON sessions(controller_id, ended_at);
    CREATE TABLE IF NOT EXISTS accounts (
      id TEXT PRIMARY KEY,
      username TEXT NOT NULL UNIQUE,
      password_hash TEXT,
      created_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS account_passkeys (
      id TEXT PRIMARY KEY,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
      public_key BLOB NOT NULL,
      counter INTEGER NOT NULL,
      transports TEXT NOT NULL
    );
    CREATE INDEX IF NOT EXISTS account_passkeys_owner ON account_passkeys(account_id);
    CREATE TABLE IF NOT EXISTS account_sessions (
      token_hash TEXT PRIMARY KEY,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
      expires_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS account_sessions_expiry ON account_sessions(expires_at);
    CREATE TABLE IF NOT EXISTS account_device_codes (
      code_hash TEXT PRIMARY KEY,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
      expires_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS account_device_codes_expiry ON account_device_codes(expires_at);
    CREATE TABLE IF NOT EXISTS account_devices (
      device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE
    );
    CREATE INDEX IF NOT EXISTS account_devices_owner ON account_devices(account_id);
    CREATE TABLE IF NOT EXISTS account_device_tokens (
      device_id TEXT PRIMARY KEY REFERENCES account_devices(device_id) ON DELETE CASCADE,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
      token_hash TEXT NOT NULL UNIQUE,
      installation_hash TEXT NOT NULL,
      expires_at INTEGER NOT NULL
    );
    CREATE TABLE IF NOT EXISTS account_device_transfers (
      code_hash TEXT PRIMARY KEY,
      device_id TEXT NOT NULL REFERENCES account_devices(device_id) ON DELETE CASCADE,
      account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
      expires_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS account_device_transfers_expiry ON account_device_transfers(expires_at);
    CREATE TABLE IF NOT EXISTS ios_device_ids (
      device_id TEXT PRIMARY KEY REFERENCES devices(id) ON DELETE CASCADE,
      persistent_id TEXT NOT NULL UNIQUE
    );
  `);
  return db;
}

export function createPairingCode(db, { role, boundHostId = null, ttlMs = 10 * 60_000 }) {
  if (!['host', 'controller'].includes(role)) throw new Error('role must be host or controller');
  if (role === 'host' && boundHostId !== null) throw new Error('host pairing codes cannot bind to another device');
  if (role === 'controller' && !boundHostId) throw new Error('controller pairing codes require a host device id');
  const now = Date.now();
  if (role === 'controller') {
    const host = db.prepare("SELECT id FROM devices WHERE id = ? AND role = 'host' AND revoked_at IS NULL").get(boundHostId);
    if (!host) throw new Error('controller pairing requires an enrolled, active host');
  }
  const code = randomSecret(24);
  const expiresAt = now + ttlMs;
  db.prepare(`INSERT INTO pairing_codes (code_hash, role, bound_host_id, expires_at, created_at)
    VALUES (?, ?, ?, ?, ?)`).run(hashSecret(code), role, boundHostId, expiresAt, now);
  return { code, expiresAt };
}

export function listDevices(db) {
  return db.prepare(`SELECT id, role, platform, name, bound_host_id AS boundHostId,
      controllable, paired_at AS pairedAt, revoked_at AS revokedAt
    FROM devices ORDER BY paired_at, id`).all();
}

export function revokeDevice(db, deviceId) {
  const now = Date.now();
  db.exec('BEGIN IMMEDIATE');
  try {
    const result = db.prepare('UPDATE devices SET revoked_at = COALESCE(revoked_at, ?), controllable = 0 WHERE id = ?').run(now, deviceId);
    db.prepare('DELETE FROM account_device_tokens WHERE device_id = ?').run(deviceId);
    db.prepare('UPDATE room_tickets SET retired_at = COALESCE(retired_at, ?) WHERE device_id = ?').run(now, deviceId);
    db.prepare(`UPDATE sessions SET ended_at = COALESCE(ended_at, ?), end_reason = COALESCE(end_reason, 'device_revoked')
      WHERE ended_at IS NULL AND (host_id = ? OR controller_id = ?)`).run(now, deviceId, deviceId);
    db.exec('COMMIT');
    return result.changes === 1;
  } catch (error) {
    db.exec('ROLLBACK');
    throw error;
  }
}
