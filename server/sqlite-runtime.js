import { isMainThread, parentPort, Worker, workerData } from 'node:worker_threads';

const SQLITE_FLAG = '--experimental-sqlite';
const canEnableSqlite = process.allowedNodeEnvironmentFlags.has(SQLITE_FLAG);
export const DatabaseSync = process.getBuiltinModule?.('node:sqlite')?.DatabaseSync;
export const SQLITE_UNAVAILABLE = canEnableSqlite
  ? `SQLite is unavailable. Start Node.js with ${SQLITE_FLAG}.`
  : 'SQLite is unavailable. Use Node.js 22.13 or later with node:sqlite enabled.';

export function runWithSqliteFlag(entryUrl) {
  if (DatabaseSync || !isMainThread || !canEnableSqlite) return false;

  const worker = new Worker(new URL(entryUrl), {
    argv: process.argv.slice(2),
    execArgv: [SQLITE_FLAG],
    workerData: { sqliteStartup: true },
  });
  const shutdown = () => worker.postMessage('shutdown');
  process.once('SIGINT', shutdown);
  process.once('SIGTERM', shutdown);
  worker.once('exit', (code) => {
    process.off('SIGINT', shutdown);
    process.off('SIGTERM', shutdown);
    process.exitCode = code;
  });
  return true;
}

export function onShutdown(shutdown) {
  if (parentPort && workerData?.sqliteStartup) {
    parentPort.once('message', shutdown).unref();
  } else {
    process.once('SIGINT', shutdown);
    process.once('SIGTERM', shutdown);
  }
}
