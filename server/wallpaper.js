import { createHash, randomUUID } from 'node:crypto';
import { hashSecret, randomSecret } from './db.js';
import { addressHost } from './desktop.js';

const PATH = '/v1/wallpapers/';
const TTL = 120;
const LIMIT = 8 * 1024 * 1024;
const PNG_HEADER = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

export function createWallpapers({ db, now, fail, origin, announceDevice }) {
  const grants = new Map();
  const grantByDevice = new Map();

  function url(id) {
    return `${origin()}${PATH}${id}`;
  }

  function dropGrant(hash) {
    const grant = grants.get(hash);
    if (!grant) return;
    grants.delete(hash);
    grantByDevice.delete(grant.deviceId);
  }

  function grant(device) {
    dropGrant(grantByDevice.get(device.id));
    const token = randomSecret();
    const hash = hashSecret(token);
    grants.set(hash, { deviceId: device.id, expiresAt: now() + TTL * 1_000 });
    grantByDevice.set(device.id, hash);
    return { token, req_url: url('upload'), ttl: TTL, max_size: LIMIT };
  }

  async function upload(req) {
    const token = req.headers.authorization;
    const hash = typeof token === 'string' && hashSecret(token);
    const grant = grants.get(hash);
    if (!grant || grant.expiresAt <= now() || !addressHost(db, grant.deviceId)) fail(401, 'wallpaper upload grant is invalid');
    const chunks = [];
    let size = 0;
    if (Number(req.headers['content-length']) >= LIMIT) fail(413, 'wallpaper exceeds the upload limit');
    for await (const chunk of req) {
      size += chunk.length;
      if (size >= LIMIT) fail(413, 'wallpaper exceeds the upload limit');
      chunks.push(chunk);
    }
    if (grants.get(hash) !== grant || grant.expiresAt <= now() || !addressHost(db, grant.deviceId)) fail(401, 'wallpaper upload grant is invalid');
    const content = Buffer.concat(chunks, size);
    let mime;
    if (content.subarray(0, PNG_HEADER.length).equals(PNG_HEADER)) mime = 'image/png';
    else if (size >= 5 && content[0] === 0xff && content[1] === 0xd8 && content[2] === 0xff
      && content[size - 2] === 0xff && content[size - 1] === 0xd9) mime = 'image/jpeg';
    else fail(415, 'wallpaper must be a PNG or JPEG image');

    const id = randomUUID();
    db.prepare('INSERT OR IGNORE INTO desktop_devices (device_id) VALUES (?)').run(grant.deviceId);
    db.prepare('DELETE FROM desktop_wallpapers WHERE device_id = ? AND expires_at IS NOT NULL').run(grant.deviceId);
    db.prepare('INSERT INTO desktop_wallpapers (id, device_id, mime, content, expires_at) VALUES (?, ?, ?, ?, ?)')
      .run(id, grant.deviceId, mime, content, now() + TTL * 1_000);
    dropGrant(hash);
    return { url: url(id), fsize: size, md5: createHash('md5').update(content).digest('hex'), mime };
  }

  function bind(device, address) {
    const id = typeof address === 'string' && address.slice(url('').length);
    if (!id || address !== url(id) || !/^[a-f0-9-]{36}$/u.test(id)) fail(400, 'invalid wallpaper address');
    db.exec('BEGIN IMMEDIATE');
    try {
      const updated = db.prepare(`UPDATE desktop_wallpapers SET expires_at = NULL WHERE id = ? AND device_id = ?
        AND (expires_at IS NULL OR expires_at > ?)`).run(id, device.id, now());
      if (!updated.changes) fail(404, 'wallpaper upload was not found');
      db.prepare('UPDATE desktop_devices SET wallpaper_id = ? WHERE device_id = ?').run(id, device.id);
      db.prepare('DELETE FROM desktop_wallpapers WHERE device_id = ? AND id != ?').run(device.id, id);
      db.exec('COMMIT');
    } catch (error) {
      db.exec('ROLLBACK');
      throw error;
    }
    announceDevice(device.id);
  }

  function download(req, res, id) {
    if ((req.method !== 'GET' && req.method !== 'HEAD') || !/^[a-f0-9-]{36}$/u.test(id)) fail(404, 'wallpaper was not found');
    const row = db.prepare(`SELECT w.mime, w.content FROM desktop_wallpapers w JOIN devices d ON d.id = w.device_id
      WHERE w.id = ? AND (w.expires_at IS NULL OR w.expires_at > ?) AND d.revoked_at IS NULL`).get(id, now());
    if (!row) fail(404, 'wallpaper was not found');
    res.writeHead(200, {
      'Content-Type': row.mime,
      'Content-Length': row.content.length,
      'X-Content-Type-Options': 'nosniff',
      'Cache-Control': 'private, max-age=300',
    });
    res.end(req.method === 'HEAD' ? undefined : row.content);
  }

  function prune() {
    const timestamp = now();
    for (const [hash, value] of grants) {
      if (value.expiresAt <= timestamp) dropGrant(hash);
    }
  }

  function removeDevice(id) {
    dropGrant(grantByDevice.get(id));
    db.prepare('DELETE FROM desktop_wallpapers WHERE device_id = ?').run(id);
  }

  return { grant, bind, upload, download, prune, removeDevice, url };
}
