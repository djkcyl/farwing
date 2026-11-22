import assert from 'node:assert/strict';
import { execFile, spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, rm, writeFile } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
import { createInterface } from 'node:readline';
import test from 'node:test';
import { promisify } from 'node:util';

test('the server and CLI share a database when SQLite requires a startup flag', async (t) => {
  const directory = await mkdtemp(join(tmpdir(), 'farwing-startup-'));
  const envFile = join(directory, 'server.env');
  const dbPath = join(directory, 'devices.sqlite');
  const env = { ...process.env };
  for (const key of ['HOST', 'PORT', 'OPENUUYC_DB_PATH', 'PUBLIC_ORIGIN', 'TLS_CERT_FILE', 'TLS_KEY_FILE',
    'DESKTOP_CLIENT_NETWORKS', 'SELF_HOST_ICE_SERVERS', 'NODE_OPTIONS']) {
    delete env[key];
  }
  await writeFile(envFile, `HOST=127.0.0.1\nPORT=0\nOPENUUYC_DB_PATH="${dbPath}"\n`);

  const nodeArgs = [`--env-file=${envFile}`];
  if (process.allowedNodeEnvironmentFlags.has('--experimental-sqlite')) {
    nodeArgs.unshift('--no-experimental-sqlite');
  }
  const options = { cwd: resolve(import.meta.dirname, '../..'), env };
  const server = spawn(process.execPath, [...nodeArgs, 'server/index.js'], {
    ...options,
    stdio: ['ignore', 'pipe', 'inherit'],
  });
  const serverClosed = new Promise((resolveClose) => server.once('close', resolveClose));
  t.after(async () => {
    server.kill();
    await serverClosed;
    await rm(directory, { recursive: true, force: true });
  });

  const output = createInterface({ input: server.stdout });
  const [line] = await once(output, 'line', { signal: AbortSignal.timeout(5000) });
  output.close();
  const origin = /listening at (http:\/\/127\.0\.0\.1:\d+)/u.exec(line)?.[1];
  assert.ok(origin, line);
  const health = await fetch(`${origin}/healthz`, { signal: t.signal });
  assert.equal(health.status, 200);
  assert.deepEqual(await health.json(), { status: 'ok' });

  const { stdout } = await promisify(execFile)(process.execPath,
    [...nodeArgs, 'server/cli.js', 'pair-code', '--role', 'host'], { ...options, signal: t.signal });
  const code = /^Pairing code: (\S+)$/mu.exec(stdout)?.[1];
  assert.ok(code, 'the CLI should issue a pairing code');
  const pairing = await fetch(`${origin}/v1/pair`, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ code, role: 'host', platform: 'windows', name: 'Startup check' }),
    signal: t.signal,
  });
  assert.equal(pairing.status, 201);
  await pairing.arrayBuffer();
});
