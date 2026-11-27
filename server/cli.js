#!/usr/bin/env node
import { DEFAULT_DB_PATH, createPairingCode, listDevices, openDatabase, revokeDevice } from './db.js';
import { runWithSqliteFlag } from './sqlite-runtime.js';
import { approveEnrollmentRequest, listEnrollmentRequests } from './desktop.js';

function parseFlags(values) {
  const result = {};
  for (let index = 0; index < values.length; index += 1) {
    const flag = values[index];
    if (!flag.startsWith('--')) throw new Error(`unexpected argument: ${flag}`);
    const value = values[index + 1];
    if (!value || value.startsWith('--')) throw new Error(`${flag} needs a value`);
    result[flag.slice(2)] = value;
    index += 1;
  }
  return result;
}

function help() {
  process.stdout.write(`OpenUUYC self-host operator CLI

Commands:
  node server/cli.js pair-code --role host
  node server/cli.js pair-code --role controller --host DEVICE_ID
  node server/cli.js list
  node server/cli.js pending
  node server/cli.js approve DEVICE_ID
  node server/cli.js revoke DEVICE_ID

Options:
  --ttl-minutes MINUTES   Pair-code lifetime (default 10, maximum 1440)
  OPENUUYC_DB_PATH        SQLite database path (default server/data/devices.sqlite)

Pair a Windows host first, then create a controller code bound to that host id.
Pair codes are single-use and are printed only when created.
`);
}

function errorText(error) {
  return error instanceof Error ? error.message : String(error);
}

const [command, ...args] = process.argv.slice(2);
if (!command || command === 'help' || command === '--help' || command === '-h') {
  help();
} else if (!runWithSqliteFlag(import.meta.url)) {
  const dbPath = process.env.OPENUUYC_DB_PATH || DEFAULT_DB_PATH;
  let db;
  try {
    db = openDatabase(dbPath);
    if (command === 'pair-code') {
      const flags = parseFlags(args);
      const role = flags.role;
      const boundHostId = flags.host || null;
      const ttlMinutes = Number(flags['ttl-minutes'] ?? 10);
      if (!Number.isInteger(ttlMinutes) || ttlMinutes < 1 || ttlMinutes > 1440) {
        throw new Error('--ttl-minutes must be between 1 and 1440');
      }
      const pair = createPairingCode(db, { role, boundHostId, ttlMs: ttlMinutes * 60_000 });
      process.stdout.write(`Pairing code: ${pair.code}\nRole: ${role}\nExpires: ${new Date(pair.expiresAt).toISOString()}\n`);
    } else if (command === 'list') {
      if (args.length) throw new Error('list does not accept arguments');
      const devices = listDevices(db);
      if (!devices.length) {
        process.stdout.write('No paired devices.\n');
      } else {
        for (const device of devices) {
          const status = device.revokedAt === null ? 'active' : 'revoked';
          const bound = device.boundHostId ? ` host=${device.boundHostId}` : '';
          process.stdout.write(`${device.id}\t${device.role}\t${device.platform}\t${status}\t${device.name}${bound}\n`);
        }
      }
    } else if (command === 'pending') {
      if (args.length) throw new Error('pending does not accept arguments');
      const requests = listEnrollmentRequests(db);
      for (const request of requests) {
        const status = request.approvedAt === null ? 'pending' : 'approved, waiting for client';
        process.stdout.write(`${request.id}\t${request.platform}\t${status}\t${JSON.stringify(request.name)}\t${new Date(request.requestedAt).toISOString()}\n`);
      }
      if (!requests.length) process.stdout.write('No device enrollment requests.\n');
    } else if (command === 'approve') {
      if (args.length !== 1 || !/^[a-f0-9]{16}$/u.test(args[0])) throw new Error('approve requires one 16-character device id');
      if (!approveEnrollmentRequest(db, args[0])) throw new Error('pending device request was not found or expired');
      process.stdout.write(`Approved ${args[0]}. Reconnect the device to complete enrollment.\n`);
    } else if (command === 'revoke') {
      if (args.length !== 1 || !/^[a-f0-9]{16}$/u.test(args[0])) throw new Error('revoke requires one 16-character device id');
      if (!revokeDevice(db, args[0])) throw new Error('device id was not found');
      process.stdout.write(`Revoked ${args[0]}.\n`);
    } else {
      throw new Error(`unknown command: ${command}`);
    }
  } catch (error) {
    process.stderr.write(`${errorText(error)}\n`);
    process.exitCode = 1;
  } finally {
    db?.close();
  }
}
