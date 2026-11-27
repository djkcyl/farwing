import { createServer } from './index.js';

const routes = [
  /^\/healthz$/,
  /^\/v1\/(?:desktop\/(?:register|room|controllable|devices|disconnect|profile|groups|join|video-preferences)|host\/(?:room|controllable)|devices|sessions|session)$/,
  /^\/api\/v1\/(?:tool\/configures_with_version|device\/(?:ios\/init|windows\/init|controllable|list|groups\/of\/my|preferences)|room\/(?:app_flag|join\/refetch|create)|transport\/rpc\/response|user\/info)$/,
  /^\/socket\.io\/$/,
];
const events = new Set([
  'room_info', 'control', 'be-controlled', 'soac', 'forward_setting', 'refresh_reconnect_key',
  'leave', 'left', 'released', 'clear_out', 'publisher_disconnect', 'session_ended',
]);
const signalTypes = new Set(['offer', 'answer', 'candidate', 'restart_ice', 'signal_app_data', 'device_capability']);

function routeLabel(url) {
  const pathname = URL.parse(url, 'http://localhost')?.pathname ?? '';
  if (/^\/api\/v1\/room\/join\/by_device\/[a-f0-9]{16}$/u.test(pathname)) {
    return '/api/v1/room/join/by_device/{id}';
  }
  if (/^\/api\/v1\/device\/[a-f0-9]{16}\/detail$/u.test(pathname)) {
    return '/api/v1/device/{id}/detail';
  }
  return routes.some((pattern) => pattern.test(pathname)) ? pathname : '(other)';
}

function log(kind, details) {
  process.stdout.write(JSON.stringify({ time: new Date().toISOString(), kind, ...details }) + '\n');
}

function signalSummary(event, payload) {
  const summary = { event: events.has(event) ? event : '(other)' };
  const type = payload?.data?.type;
  if (signalTypes.has(type)) summary.type = type;
  if (Buffer.isBuffer(payload?.app_data)) {
    summary.appDataBytes = payload.app_data.length;
    summary.appDataEnginePrefix = payload.app_data[0] === 0x04;
    summary.appDataStartsWithProtobuf = payload.app_data[summary.appDataEnginePrefix ? 1 : 0] === 0x08
      || payload.app_data[summary.appDataEnginePrefix ? 1 : 0] === 0x10;
  }
  if (event === 'control') {
    const id = payload?.app_control_id;
    summary.controlIdIsUuid = typeof id === 'string' && /^[a-f0-9-]{36}$/iu.test(id);
    summary.controlIdHasUppercase = typeof id === 'string' && /[A-F]/u.test(id);
    summary.streamerDataType = typeof payload?.streamer_data;
    if (typeof payload?.streamer_data === 'string') summary.streamerDataLength = payload.streamer_data.length;
  }
  return summary;
}

for (const key of ['HOST', 'PORT', 'PUBLIC_ORIGIN', 'DESKTOP_CLIENT_NETWORKS', 'OPENUUYC_DB_PATH', 'TLS_CERT_FILE', 'TLS_KEY_FILE']) {
  if (!process.env[key]) throw new Error(`Capture requires ${key}`);
}

const app = createServer({ adminPort: Number(process.env.ADMIN_PORT ?? 8789) });
app.server.prependListener('request', (request, response) => {
  response.once('finish', () => log('http', {
    method: request.method, route: routeLabel(request.url), status: response.statusCode,
  }));
});
app.server.prependListener('upgrade', (request) => log('upgrade', { route: routeLabel(request.url) }));
app.io.engine.on('connection_error', (error) => log('engine_error', { code: error.code }));
app.io.on('connection', (socket) => {
  const peer = socket.data.nativeSession ? 'controller' : socket.data.roomAuth.presenceOnly ? 'ios_presence' : 'host';
  log('socket_connected', { peer });
  socket.onAny((event, payload) => log('socket_in', { peer, ...signalSummary(event, payload) }));
  socket.onAnyOutgoing((event, payload) => log('socket_out', { peer, ...signalSummary(event, payload) }));
  socket.on('disconnect', () => log('socket_disconnected', { peer }));
});
await app.listen();
log('listening', { origin: app.publicOrigin() });
log('admin_listening', { origin: app.adminOrigin() });
for (const signal of ['SIGINT', 'SIGTERM']) process.once(signal, () => app.close());
