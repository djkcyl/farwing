import { randomUUID } from 'node:crypto';
import { isIPv4 } from 'node:net';
import { addressHost } from './desktop.js';

const WOL_INFO = '/api/v1/device/wake_on_lan/info';
const WOL_SETTING = '/api/v1/device/set_support_wol';
const RPC_RESPONSE = '/api/v1/transport/rpc/response';
const RESPONSE_TIMEOUT = 16_000;

function ipv4(value) {
  if (typeof value !== 'string' || !isIPv4(value)) return null;
  return value.split('.').reduce((address, octet) => address * 256 + Number(octet), 0);
}

function subnet(address, mask) {
  return (address & mask) >>> 0;
}

function lanInfo(body, peer, fail) {
  const mac = typeof body.mac === 'string' && body.mac.length <= 17 && body.mac.replace(/[:-]/gu, '').toLowerCase();
  const address = ipv4(body.inner_ip);
  const mask = ipv4(body.subnet_mask);
  const prefix = mask === null ? 0 : Math.clz32((~mask) >>> 0);
  if (!mac || !/^[a-f0-9]{12}$/u.test(mac) || /^0+$/u.test(mac) || (Number.parseInt(mac.slice(0, 2), 16) & 1)
    || address === null || prefix < 1 || prefix > 30 || mask !== ((0xffffffff << (32 - prefix)) >>> 0)) {
    fail(400, 'invalid wake-on-LAN network');
  }
  const first = address >>> 24;
  const part = (address & ~mask) >>> 0;
  if (first === 0 || first === 127 || first >= 224 || (address >>> 16) === 0xa9fe || !part || part === ((~mask) >>> 0)) {
    fail(400, 'invalid wake-on-LAN host address');
  }
  return { mac, inner_ip: body.inner_ip, subnet_mask: body.subnet_mask, peer };
}

function localPeer(peer, target) {
  const parsed = ipv4(peer?.replace(/^::ffff:/u, ''));
  const mask = ipv4(target.subnet_mask);
  return parsed !== null && subnet(parsed, mask) === subnet(ipv4(target.inner_ip), mask);
}

export function createPower({ db, fail, roomId, pushTo, announceDevice, connectedPeers }) {
  const requests = new Map();

  function writeLanInfo(device, body, peer) {
    const info = lanInfo(body, peer, fail);
    db.prepare(`INSERT INTO desktop_devices (device_id, lan_info) VALUES (?, ?)
      ON CONFLICT(device_id) DO UPDATE SET lan_info = excluded.lan_info`).run(device.id, JSON.stringify(info));
    return { support_wol: Boolean(addressHost(db, device.id)?.supportWol) };
  }

  function setWake(device, enabled) {
    if (typeof enabled !== 'boolean') fail(400, 'support_wol must be a boolean');
    if (enabled && (!addressHost(db, device.id)?.lanInfo || !device.controllable || !roomId(device.id))) {
      fail(409, 'enable controlled access and register the wired network first');
    }
    db.prepare(`INSERT INTO desktop_devices (device_id, support_wol) VALUES (?, ?)
      ON CONFLICT(device_id) DO UPDATE SET support_wol = excluded.support_wol`).run(device.id, enabled ? 1 : 0);
    announceDevice(device.id);
  }

  function wake(device, target) {
    if (roomId(target.id) || !target.supportWol || !target.lanInfo) fail(409, 'host is not available for wake-on-LAN');
    const { peer, ...lan } = JSON.parse(target.lanInfo);
    const address = ipv4(lan.inner_ip);
    const mask = ipv4(lan.subnet_mask);
    const network = subnet(address, mask);
    let sender = false;
    let count = 0;
    for (const [id, currentPeer] of connectedPeers()) {
      const helper = addressHost(db, id);
      if (!helper?.controllable) continue;
      let sameNetwork = localPeer(currentPeer, lan) && localPeer(peer, lan);
      if (!sameNetwork && currentPeer === peer && helper.lanInfo) {
        const info = JSON.parse(helper.lanInfo);
        sameNetwork = info.peer === peer && info.subnet_mask === lan.subnet_mask
          && subnet(ipv4(info.inner_ip), mask) === network;
      }
      if (sameNetwork && pushTo(id, 'wake_on_lan', { target_device: lan })) {
        if (id === device.id) sender = true;
        else count += 1;
      }
    }
    if (!sender && !count) fail(409, 'no online device can send the wake-on-LAN packet');
    return { current_device_in_same_network: sender, assist_count: count, router_wol: false };
  }

  async function run(device, target, action, res) {
    const hostRoom = roomId(target.id);
    if (!hostRoom) fail(409, 'host is offline', 2006);
    if ([...requests.values()].some((request) => request.hostId === target.id)) fail(409, 'host has a pending power request');
    const callId = randomUUID();
    const result = await new Promise((resolve) => {
      const finish = (code) => {
        clearTimeout(timer);
        res.off('close', cancelled);
        requests.delete(callId);
        resolve(code);
      };
      const cancelled = () => finish(null);
      const timer = setTimeout(cancelled, RESPONSE_TIMEOUT);
      timer.unref?.();
      res.once('close', cancelled);
      requests.set(callId, { hostId: target.id, controllerId: device.id, hostRoom, finish });
      const event = action === 'shutdown' ? 'transport_rpc' : 'device_reboot';
      if (!pushTo(target.id, event, { p: action, call_id: callId, need_response: true })) cancelled();
    });
    if (result === null) fail(504, 'host did not acknowledge the power request');
    if (result !== 0) fail(403, 'host denied the power request');
    return {};
  }

  async function request(device, path, body, req, res, send) {
    if (req.method !== 'POST') return false;
    if (path === WOL_INFO) {
      send(writeLanInfo(device, body, req.socket.remoteAddress));
    } else if (path === WOL_SETTING) {
      setWake(device, body.support_wol);
      send({});
    } else if (path === RPC_RESPONSE) {
      const pending = requests.get(body.call_id);
      if (!pending || pending.hostId !== device.id || pending.hostRoom !== roomId(device.id)) fail(404, 'power request expired');
      if (!Number.isSafeInteger(body.code)) fail(400, 'invalid power response');
      pending.finish(body.code);
      send({});
    } else {
      const direct = /^\/api\/v1\/device\/([a-f0-9]{16})\/(reboot|wake_on_lan\/req)$/u.exec(path);
      const shutdown = /^\/api\/v1\/transport\/([a-f0-9]{16})\/rpc\/request$/u.exec(path);
      if (!direct && !shutdown) return false;
      const id = (direct ?? shutdown)[1];
      const target = addressHost(db, id);
      if (id === device.id) fail(400, 'cannot send a remote power command to this device');
      if (!target) fail(404, 'device was not found');
      if (!target.controllable) fail(409, 'host is not accepting power requests');
      if (shutdown && body.p !== 'shutdown') fail(400, 'unsupported power procedure');
      send(direct?.[2] === 'wake_on_lan/req' ? wake(device, target) : await run(device, target, shutdown ? 'shutdown' : 'reboot', res));
    }
    return true;
  }

  function cancelForDevice(id) {
    for (const pending of requests.values()) {
      if (pending.hostId === id || pending.controllerId === id) pending.finish(null);
    }
  }

  function close() {
    for (const pending of requests.values()) pending.finish(null);
  }

  return { request, cancelForDevice, close };
}
