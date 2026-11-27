import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { listDevices } from './db.js';
import { approveEnrollmentRequest, deleteEnrollmentEntry, listEnrollmentGrants,
  listEnrollmentRequests, preapproveEnrollment } from './desktop.js';

const host = '127.0.0.1';
const assets = new Map([
  ['/', ['text/html; charset=utf-8', './admin/index.html']],
  ['/admin.css', ['text/css; charset=utf-8', './admin/style.css']],
  ['/admin.js', ['text/javascript; charset=utf-8', './admin/app.js']],
].map(([path, [type, file]]) => [path, { type, body: readFileSync(new URL(file, import.meta.url)) }]));
const securityHeaders = {
  'Cache-Control': 'no-store',
  'Content-Security-Policy': "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
  'Referrer-Policy': 'no-referrer',
  'X-Content-Type-Options': 'nosniff',
  'Cross-Origin-Resource-Policy': 'same-origin',
  'X-Frame-Options': 'DENY',
};

export function createAdmin({ db, now, revoke, readJson, HttpError, approvalLifetime }) {
  const server = createServer(async (req, res) => {
    function reply(status, value) {
      res.writeHead(status, { ...securityHeaders, 'Content-Type': 'application/json; charset=utf-8' });
      res.end(JSON.stringify(value));
    }
    const authority = `${host}:${server.address().port}`;
    const origin = `http://${authority}`;
    const site = req.headers['sec-fetch-site'];
    if (req.socket.remoteAddress !== host || req.headers.host !== authority
      || Object.keys(req.headers).some((key) => key === 'forwarded' || key.startsWith('x-forwarded-'))
      || (site && site !== 'same-origin' && site !== 'none')
      || (req.headers.origin && req.headers.origin !== origin)) {
      reply(403, { error: '只能从部署端本机访问。' });
      return;
    }

    try {
      if (req.method === 'GET') {
        const asset = assets.get(req.url);
        if (asset) {
          res.writeHead(200, { ...securityHeaders, 'Content-Type': asset.type });
          res.end(asset.body);
          return;
        }
        if (req.url === '/api/devices') {
          const timestamp = now();
          reply(200, {
            requests: listEnrollmentRequests(db, timestamp, approvalLifetime),
            grants: listEnrollmentGrants(db, timestamp),
            devices: listDevices(db).filter((device) => device.revokedAt === null),
          });
          return;
        }
      } else if (req.method === 'POST') {
        if (req.headers.origin !== origin) {
          reply(403, { error: '请求来源无效。' });
          return;
        }
        const actions = {
          '/api/devices/approve': [approveEnrollmentRequest, '申请已不存在，请刷新列表。'],
          '/api/devices/preapprove': [preapproveEnrollment, '该 ID 已有设备记录。'],
          '/api/devices/delete': [deleteEnrollmentEntry, '申请或预准入已不存在。'],
          '/api/devices/revoke': [(_, id) => revoke(id), '设备已不存在。'],
        };
        if (Object.hasOwn(actions, req.url)) {
          const { id } = await readJson(req);
          if (typeof id !== 'string' || !/^[a-f0-9]{16}$/u.test(id)) {
            reply(400, { error: '请输入 16 位十六进制设备 ID。' });
            return;
          }
          const [action, message] = actions[req.url];
          if (!action(db, id, now(), approvalLifetime)) {
            reply(409, { error: message });
            return;
          }
          reply(200, { ok: true });
          return;
        }
      }
      reply(404, { error: '未找到页面。' });
    } catch (error) {
      if (res.destroyed) return;
      const status = error instanceof HttpError ? error.status : 500;
      reply(status, { error: status === 500 ? '管理请求失败。' : error.message });
    }
  });
  server.requestTimeout = 10_000;

  return {
    async listen(port) {
      if (!Number.isInteger(port) || port < 0 || port > 65535) throw new Error('Invalid ADMIN_PORT');
      await new Promise((resolve, reject) => {
        server.once('error', reject);
        server.listen(port, host, () => {
          server.off('error', reject);
          resolve();
        });
      });
      return `http://${host}:${server.address().port}`;
    },
    close: () => new Promise((resolve) => server.close(resolve)),
  };
}
