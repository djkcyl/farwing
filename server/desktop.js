import { BlockList, isIP } from 'node:net';
import { hashSecret } from './db.js';

const DEVICE_ID = /^[a-f0-9]{16}$/u;
export const ADDRESS_TOKEN = /^[a-f0-9]{64}$/u;

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

export function enrollAddressDevice(db, token, name, platform, timestamp) {
  const hash = hashSecret(token);
  const id = hash.slice(0, 16);
  db.exec('BEGIN IMMEDIATE');
  try {
    const existing = db.prepare(`SELECT d.token_hash AS tokenHash, d.platform, d.revoked_at AS revokedAt,
        a.device_id IS NOT NULL AS addressMember
      FROM devices d LEFT JOIN address_members a ON a.device_id = d.id WHERE d.id = ?`).get(id);
    if (existing) {
      if (!existing.addressMember || existing.tokenHash !== hash || existing.platform !== platform
        || existing.revokedAt !== null) {
        db.exec('ROLLBACK');
        return null;
      }
    } else {
      db.prepare(`INSERT INTO devices (id, role, platform, name, token_hash, paired_at)
          VALUES (?, ?, ?, ?, ?, ?)`).run(id, platform === 'windows' ? 'host' : 'controller', platform, name, hash, timestamp);
      db.prepare('INSERT INTO address_members (device_id) VALUES (?)').run(id);
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
  return db.prepare(`SELECT d.id, d.name, d.controllable FROM address_members a
    JOIN devices d ON a.device_id = d.id WHERE d.id = ? AND d.role = 'host' AND d.revoked_at IS NULL`).get(id);
}
