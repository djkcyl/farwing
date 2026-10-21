// File transfer with the host (`docs/protocol/web-control-brief.md` §4):
// FILE requests on the session channel, data as TRANSFER streams (kind 1)
// on the bulk channel paced by FLOW credits. Browsing, uploading files and
// folders into a host folder, and downloading files and folders.
import * as fpp from './fpp';
import { Fields, Writer } from './pb';
import { PeerRequestError, type PeerIo } from './peerio';
import { ZipWriter } from './zip';

export const EntryKind = { FILE: 1, DIR: 2, DISK: 3, LINK: 4 } as const;

export interface FileEntry {
  name: string;
  kind: number;
  size: bigint;
  mtimeNs: bigint;
  hidden: boolean;
  /// Absolute path; only in the root listing (disks, known folders).
  path: string;
  icon: string;
}

export interface Listing {
  path: string;
  entries: FileEntry[];
}

/// The host user's Downloads folder (`storage.rs`).
export const DEFAULT_DIR = ':/Default';

const REQUEST_TIMEOUT_MS = 60_000;
const DOWNLOAD_WINDOW = 16 * 1024 * 1024;
const READ_BLOCK = 2 * 1024 * 1024;
const WHOLE_TRANSFER = 0xffffffff;

const ERRORS: Record<number, string> = {
  1: '传输协议出错',
  2: '对方不允许文件传输',
  3: '文件或文件夹不存在',
  4: '目标已存在',
  5: '对方磁盘空间不足',
  6: '读写文件失败',
  7: '对方暂时无法处理',
  8: '文件在传输过程中被修改',
  9: '已取消',
};

export class FileError extends Error {
  code: number;
  constructor(code: number, message?: string) {
    super(message || ERRORS[code] || `文件传输失败（${code}）`);
    this.code = code;
  }
}

/// A name the host accepts as one path segment.
export function safeName(name: string): string {
  // eslint-disable-next-line no-control-regex
  let n = name.replace(/[\u0000-\u001f:<>"|?*\\/]/g, '_').replace(/[. ]+$/, '');
  if (!n || n === '.' || n === '..') n = '_';
  if (/^(con|prn|aux|nul|com[0-9]|lpt[0-9])(\..*)?$/i.test(n)) n = `_${n}`;
  return n.length > 200 ? n.slice(0, 200) : n;
}

/// Unique (case-insensitive) names for one upload.
export function uniqueNames(names: string[]): string[] {
  const seen = new Set<string>();
  return names.map((raw) => {
    const name = safeName(raw);
    const dot = name.lastIndexOf('.');
    const [stem, ext] = dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ''];
    let candidate = name;
    for (let n = 2; seen.has(candidate.toLowerCase()); n++) candidate = `${stem} (${n})${ext}`;
    seen.add(candidate.toLowerCase());
    return candidate;
  });
}

/// A '/'-separated relative path the host accepts, segment by segment.
export function safePath(path: string): string {
  return path
    .split(/[\\/]+/)
    .filter((part) => part && part !== '.')
    .map(safeName)
    .join('/');
}

/// Safe and unique (case-insensitive) paths for one upload: a later
/// duplicate gets "name (2).ext".
export function uniquePaths(paths: string[]): string[] {
  const seen = new Set<string>();
  return paths.map((raw) => {
    const path = safePath(raw) || '_';
    const cut = path.lastIndexOf('/');
    const [parent, name] = cut >= 0 ? [path.slice(0, cut + 1), path.slice(cut + 1)] : ['', path];
    const dot = name.lastIndexOf('.');
    const [stem, ext] = dot > 0 ? [name.slice(0, dot), name.slice(dot)] : [name, ''];
    let candidate = path;
    for (let n = 2; seen.has(candidate.toLowerCase()); n++) candidate = `${parent}${stem} (${n})${ext}`;
    seen.add(candidate.toLowerCase());
    return candidate;
  });
}

/// The full path of an entry inside a listing.
export function childPath(listing: string, entry: FileEntry): string {
  if (entry.path) return entry.path;
  return listing.endsWith('\\') || listing.endsWith('/') ? listing + entry.name : `${listing}\\${entry.name}`;
}

// Messages ------------------------------------------------------------------------

export function listMessage(path: string): Uint8Array {
  return new Writer().message(1, new Writer().string(1, path)).finish();
}

interface Item {
  path: string;
  size: bigint;
  mtimeNs: bigint;
  dir?: boolean;
}

/// `folder`: the top-level folder created in `dir` (renamed "folder (n)" if
/// it exists); item paths are relative to it.
export function uploadMessage(transfer: number, dir: string, items: Item[], key: string, folder = ''): Uint8Array {
  const inner = new Writer().int(1, transfer).string(2, dir).string(3, folder);
  for (const item of items) {
    inner.message(4, new Writer().string(1, item.path).int(2, item.size).int(3, item.mtimeNs).int(4, !!item.dir));
  }
  // conflict 2: rename like "name (2).ext".
  inner.int(5, 2).string(6, key);
  return new Writer().message(6, inner).finish();
}

export function uploadStartMessage(transfer: number, offsets: bigint[]): Uint8Array {
  return new Writer().message(7, new Writer().int(1, transfer).packed(2, offsets)).finish();
}

export function downloadMessage(transfer: number, paths: string[]): Uint8Array {
  const inner = new Writer().int(1, transfer);
  for (const p of paths) inner.string(2, p);
  return new Writer().message(8, inner).finish();
}

export function downloadStartMessage(transfer: number, items: number): Uint8Array {
  const inner = new Writer().int(1, transfer);
  for (let i = 0; i < items; i++) inner.message(2, new Writer());
  return new Writer().message(9, inner).finish();
}

export function cancelMessage(transfer: number, key: string, discard: boolean): Uint8Array {
  return new Writer().message(10, new Writer().int(1, transfer).string(2, key).int(3, discard)).finish();
}

/// The reply body, or the `Failed` it carries as an error.
function reply(body: Uint8Array, tag: number): Fields {
  const f = new Fields(body);
  const failed = f.message(26);
  if (failed) throw new FileError(failed.int32(1), failed.string(2));
  const m = f.message(tag);
  if (!m) throw new FileError(1, '对方的回复格式不正确');
  return m;
}

// Transfers -----------------------------------------------------------------------

export interface TransferInfo {
  id: number;
  direction: 'up' | 'down';
  name: string;
  total: number;
  done: number;
  state: 'running' | 'done' | 'failed' | 'cancelled';
  error?: string;
}

export interface DownloadSink {
  write(data: Uint8Array): Promise<void>;
  close(): Promise<void>;
  abort(): Promise<void>;
}

/// One entry of a download manifest: a file or a directory, '/'-separated
/// and relative to the downloaded folder.
export interface ManifestItem {
  path: string;
  size: number;
  mtimeNs: bigint;
  dir: boolean;
}

/// Where a download goes, entry by entry in manifest order.
export interface DownloadTarget {
  /// Start an entry: create the directory or open the file.
  entry(item: ManifestItem): Promise<void>;
  write(data: Uint8Array): Promise<void>;
  /// The current entry is complete.
  finish(): Promise<void>;
  close(): Promise<void>;
  abort(): Promise<void>;
}

/// One thing to upload: a file, or a directory (so empty ones arrive too),
/// at a '/'-separated path relative to the receiving folder.
export interface UploadEntry {
  path: string;
  file?: File;
}

interface UploadState {
  info: TransferInfo;
  key: string;
  credit: number;
  pending: Set<number>;
  wake?: () => void;
  settle?: (error?: Error) => void;
  failed?: Error;
  cancelled: boolean;
}

interface DownloadState {
  info: TransferInfo;
  target: DownloadTarget;
  items: ManifestItem[];
  /// The entry being written, -1 before the first.
  current: number;
  delivered: number;
  granted: number;
  chain: Promise<void>;
  settle?: (error?: Error) => void;
  failed?: Error;
  finished: boolean;
  cancelled: boolean;
}

export interface FileEvents {
  transfer?(info: TransferInfo): void;
  log?(line: string): void;
}

export class FileTransfer {
  private io: PeerIo;
  private events: FileEvents;
  private nextId = 1;
  private uploads = new Map<number, UploadState>();
  private downloads = new Map<number, DownloadState>();

  constructor(io: PeerIo, events: FileEvents) {
    this.io = io;
    this.events = events;
  }

  private id(): number {
    const id = this.nextId;
    this.nextId = (this.nextId % 0xfffffffe) + 1;
    return id;
  }

  private report(info: TransferInfo) {
    this.events.transfer?.({ ...info });
  }

  private async call(body: Uint8Array): Promise<Uint8Array> {
    try {
      return await this.io.request(fpp.Type.FILE, body, REQUEST_TIMEOUT_MS);
    } catch (error) {
      if (error instanceof PeerRequestError) {
        throw new FileError(error.code === 'forbidden' ? 2 : 1, error.code === 'timeout' ? '对方没有响应' : undefined);
      }
      throw error;
    }
  }

  async list(path: string): Promise<Listing> {
    const m = reply(await this.call(listMessage(path)), 20);
    return {
      path: m.string(1),
      entries: m.messages(2).map((e) => ({
        name: e.string(1),
        kind: e.int32(2),
        size: e.big(3),
        mtimeNs: e.big(4),
        hidden: e.bool(5),
        path: e.string(6),
        icon: e.string(7),
      })),
    };
  }

  /// Upload loose files into `dir`; names that exist are renamed.
  uploadFiles(files: File[], dir: string): Promise<void> {
    return this.upload(
      files.map((file) => ({ path: file.name, file })),
      dir,
    );
  }

  /// Upload entries into `dir`, under a new top-level `folder` when given.
  async upload(entries: UploadEntry[], dir: string, folder = ''): Promise<void> {
    if (!entries.length && !folder) return;
    const id = this.id();
    const paths = uniquePaths(entries.map((e) => e.path));
    const top = folder ? safeName(folder) : '';
    const files = entries.filter((e) => e.file).length;
    const total = entries.reduce((n, e) => n + (e.file?.size ?? 0), 0);
    const state: UploadState = {
      info: {
        id,
        direction: 'up',
        name: top || (files === 1 ? paths[0] : `${paths[0]} 等 ${files} 个文件`),
        total,
        done: 0,
        state: 'running',
      },
      key: crypto.randomUUID(),
      credit: fpp.INITIAL_CREDIT,
      pending: new Set(),
      cancelled: false,
    };
    this.uploads.set(id, state);
    this.report(state.info);
    try {
      const items = entries.map((e, i) => ({
        path: paths[i],
        size: BigInt(e.file?.size ?? 0),
        mtimeNs: BigInt(e.file?.lastModified ?? 0) * 1_000_000n,
        dir: !e.file,
      }));
      const plan = reply(await this.call(uploadMessage(id, dir, items, state.key, top)), 23);
      const skip = plan.messages(2).map((p) => p.bool(3));
      for (let i = 0; i < entries.length; i++) if (!skip[i]) state.pending.add(i);
      const done = new Promise<void>((resolve, reject) => {
        state.settle = (error) => (error ? reject(error) : resolve());
      });
      // Settled early when a step before the wait fails.
      done.catch(() => undefined);
      reply(
        await this.call(
          uploadStartMessage(
            id,
            entries.map(() => 0n),
          ),
        ),
        21,
      );
      let sent = 0;
      for (let i = 0; i < entries.length; i++) {
        const file = entries[i].file;
        if (skip[i]) {
          state.info.done += file?.size ?? 0;
          continue;
        }
        const size = file?.size ?? 0;
        let offset = 0;
        // Read ahead in blocks: one read per TRANSFER frame is slow.
        let block = new Uint8Array();
        let blockAt = 0;
        // Directories and empty files: one empty last chunk.
        do {
          if (state.cancelled || state.failed) break;
          const end = Math.min(size, offset + fpp.TRANSFER_CHUNK);
          if (file && end > blockAt + block.length) {
            blockAt = offset;
            block = new Uint8Array(await file.slice(offset, Math.min(size, offset + READ_BLOCK)).arrayBuffer());
          }
          const data = block.subarray(offset - blockAt, end - blockAt);
          while (sent + data.length > state.credit && !state.cancelled && !state.failed) {
            await new Promise<void>((resolve) => {
              state.wake = resolve;
              window.setTimeout(resolve, 5000);
            });
          }
          if (state.cancelled || state.failed) break;
          await this.io.bulk(
            fpp.Type.TRANSFER,
            fpp.chunk({
              stream: id,
              kind: fpp.TransferKind.FILE,
              offset: BigInt(offset),
              data,
              last: end >= size,
              entry: i,
            }),
          );
          sent += data.length;
          offset = end;
          state.info.done += data.length;
          this.report(state.info);
        } while (offset < size);
      }
      if (state.failed) throw state.failed;
      if (state.cancelled) throw new FileError(9);
      if (state.pending.size) await done;
      state.info.state = 'done';
      this.report(state.info);
    } catch (error) {
      state.info.state = state.cancelled ? 'cancelled' : 'failed';
      state.info.error = (error as Error).message;
      this.report(state.info);
      throw error;
    } finally {
      this.uploads.delete(id);
    }
  }

  /// Download a file or a folder of the host into `target`.
  async download(path: string, name: string, target: DownloadTarget): Promise<void> {
    const id = this.id();
    const state: DownloadState = {
      info: { id, direction: 'down', name, total: 0, done: 0, state: 'running' },
      target,
      items: [],
      current: -1,
      delivered: 0,
      granted: fpp.INITIAL_CREDIT,
      chain: Promise.resolve(),
      finished: false,
      cancelled: false,
    };
    // Data may arrive before the replies: register first.
    this.downloads.set(id, state);
    this.report(state.info);
    try {
      const done = new Promise<void>((resolve, reject) => {
        state.settle = (error) => (error ? reject(error) : resolve());
      });
      // Settled early when a step before the wait fails.
      done.catch(() => undefined);
      const manifest = reply(await this.call(downloadMessage(id, [path])), 24);
      state.items = manifest.messages(3).map((m) => ({
        path: m.string(1),
        size: Number(m.big(2)),
        mtimeNs: m.big(3),
        dir: m.bool(4),
      }));
      state.info.total = state.items.reduce((n, i) => n + i.size, 0);
      this.report(state.info);
      reply(await this.call(downloadStartMessage(id, state.items.length)), 25);
      if (state.items.length) await done;
      await state.chain;
      if (state.failed) throw state.failed;
      await target.close();
      state.info.state = 'done';
      state.info.done = state.info.total;
      this.report(state.info);
    } catch (error) {
      await target.abort().catch(() => undefined);
      state.info.state = state.cancelled ? 'cancelled' : 'failed';
      state.info.error = (error as Error).message;
      this.report(state.info);
      throw error;
    } finally {
      this.downloads.delete(id);
    }
  }

  async cancel(id: number) {
    const up = this.uploads.get(id);
    if (up) {
      up.cancelled = true;
      up.wake?.();
      up.settle?.(new FileError(9));
      await this.io.request(fpp.Type.FILE, cancelMessage(id, up.key, true)).catch(() => undefined);
      return;
    }
    const down = this.downloads.get(id);
    if (down) {
      down.cancelled = true;
      down.settle?.(new FileError(9));
      await this.io.request(fpp.Type.FILE, cancelMessage(id, '', true)).catch(() => undefined);
    }
  }

  /// FILE pushes: `ItemDone`.
  handle(frame: fpp.Frame) {
    const done = new Fields(frame.body).message(30);
    if (!done) {
      if (frame.requestId !== 0) this.io.replyError(fpp.Type.FILE, frame.requestId, 'bad_request');
      return;
    }
    const id = done.uint32(1);
    const entry = done.uint32(2);
    const code = done.int32(3);
    const error = code ? new FileError(code, done.string(4) || undefined) : undefined;
    const up = this.uploads.get(id);
    if (up) {
      if (error) {
        up.failed = error;
        up.wake?.();
        up.settle?.(error);
      } else {
        up.pending.delete(entry);
        if (!up.pending.size) up.settle?.();
      }
      return;
    }
    const down = this.downloads.get(id);
    if (down && (error || entry === WHOLE_TRANSFER)) {
      down.failed = error ?? new FileError(1);
      down.settle?.(down.failed);
    }
  }

  onChunk(c: fpp.Chunk) {
    const down = this.downloads.get(c.stream);
    if (!down || down.failed || down.cancelled || down.finished) return;
    const data = c.data.slice();
    down.chain = down.chain.then(async () => {
      if (down.failed || down.cancelled) return;
      const item = down.items[c.entry];
      try {
        if (!item || c.entry < down.current) throw new FileError(1, '文件条目顺序不符');
        if (c.entry !== down.current) {
          down.current = c.entry;
          await down.target.entry(item);
        }
        if (data.length) await down.target.write(data);
        if (c.last) await down.target.finish();
      } catch (error) {
        down.failed = error instanceof FileError ? error : new FileError(6, `保存失败：${(error as Error).message}`);
        down.settle?.(down.failed);
        void this.cancel(c.stream);
        return;
      }
      down.delivered += data.length;
      down.info.done = down.delivered;
      this.report(down.info);
      if (down.delivered + DOWNLOAD_WINDOW - down.granted >= DOWNLOAD_WINDOW / 4) {
        down.granted = down.delivered + DOWNLOAD_WINDOW;
        void this.io.bulk(
          fpp.Type.FLOW,
          fpp.flow({ stream: c.stream, credit: BigInt(down.granted), kind: fpp.TransferKind.FILE }),
        );
      }
      if (c.last && c.entry + 1 >= down.items.length) {
        down.finished = true;
        down.settle?.();
      }
    });
  }

  onFlow(f: fpp.FlowCredit) {
    const up = this.uploads.get(f.stream);
    if (!up) return;
    up.credit = Math.max(up.credit, Number(f.credit));
    up.wake?.();
  }

  /// The connection is gone: fail everything in flight.
  reset() {
    const error = new FileError(1, '连接已断开');
    for (const up of this.uploads.values()) {
      up.failed = error;
      up.wake?.();
      up.settle?.(error);
    }
    for (const down of this.downloads.values()) {
      down.failed = error;
      down.settle?.(error);
    }
  }
}

// Saving downloads -------------------------------------------------------------------

interface Writable {
  write(data: BufferSource): Promise<void>;
  close(): Promise<void>;
  abort(): Promise<void>;
}

interface FileHandle {
  createWritable(): Promise<Writable>;
}

interface DirectoryHandle {
  name: string;
  getDirectoryHandle(name: string, options?: { create?: boolean }): Promise<DirectoryHandle>;
  getFileHandle(name: string, options?: { create?: boolean }): Promise<FileHandle>;
}

interface FilePickers {
  showSaveFilePicker?(options: { suggestedName?: string }): Promise<FileHandle>;
  showDirectoryPicker?(options: { mode?: 'read' | 'readwrite'; id?: string }): Promise<DirectoryHandle>;
}

const pickers = () => window as unknown as FilePickers;

/// Memory downloads are limited to this (other browsers than Chromium).
const MEMORY_LIMIT = 1024 * 1024 * 1024;

function browserDownload(blob: Blob, name: string) {
  const url = URL.createObjectURL(blob);
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  window.setTimeout(() => URL.revokeObjectURL(url), 60_000);
}

/// A single-file download as a target.
export function fileTarget(sink: DownloadSink): DownloadTarget {
  return {
    entry: async () => undefined,
    write: (data) => sink.write(data),
    finish: async () => undefined,
    close: () => sink.close(),
    abort: () => sink.abort(),
  };
}

/// Where a file download goes: a file the user picks (Chromium, streamed to
/// disk), else memory and a browser download at the end. Call from the
/// click handler: the picker needs the user's gesture.
export async function saveTarget(name: string, size: number): Promise<DownloadSink | undefined> {
  const picker = pickers().showSaveFilePicker;
  if (picker) {
    try {
      const handle = await picker.call(window, { suggestedName: name });
      const writable = await handle.createWritable();
      return {
        write: (data) => writable.write(data as Uint8Array<ArrayBuffer>),
        close: () => writable.close(),
        abort: () => writable.abort(),
      };
    } catch (error) {
      if ((error as DOMException).name === 'AbortError') return undefined;
      // Not allowed here: fall back to memory.
    }
  }
  if (size > MEMORY_LIMIT) throw new FileError(6, '这个浏览器只能下载 1 GB 以内的文件');
  const parts: Uint8Array[] = [];
  return {
    write: async (data) => {
      parts.push(data);
    },
    close: async () => browserDownload(new Blob(parts as BlobPart[]), name),
    abort: async () => {
      parts.length = 0;
    },
  };
}

/// A folder written into a local directory (File System Access API).
export function directoryTarget(root: DirectoryHandle): DownloadTarget {
  let writable: Writable | undefined;
  const dirOf = async (segments: string[]) => {
    let dir = root;
    for (const part of segments) dir = await dir.getDirectoryHandle(safeName(part), { create: true });
    return dir;
  };
  return {
    entry: async (item) => {
      const segments = item.path.split('/').filter(Boolean);
      if (item.dir) {
        await dirOf(segments);
        return;
      }
      const dir = await dirOf(segments.slice(0, -1));
      const file = await dir.getFileHandle(safeName(segments[segments.length - 1] ?? '_'), { create: true });
      writable = await file.createWritable();
    },
    write: async (data) => {
      await writable?.write(data as Uint8Array<ArrayBuffer>);
    },
    finish: async () => {
      await writable?.close();
      writable = undefined;
    },
    close: async () => undefined,
    abort: async () => {
      await writable?.abort().catch(() => undefined);
      writable = undefined;
    },
  };
}

/// A folder as a ZIP built in memory, downloaded at the end.
export function zipTarget(folder: string): DownloadTarget {
  const zip = new ZipWriter();
  let size = 0;
  return {
    entry: async (item) => {
      zip.end();
      zip.begin(`${folder}/${item.path}`, item.dir, new Date(Number(item.mtimeNs / 1_000_000n) || Date.now()));
    },
    write: async (data) => {
      size += data.length;
      if (size > MEMORY_LIMIT) throw new Error('这个浏览器只能下载 1 GB 以内的文件夹');
      zip.write(data);
    },
    finish: async () => zip.end(),
    close: async () => browserDownload(zip.finish(), `${folder}.zip`),
    abort: async () => undefined,
  };
}

/// Where a folder download goes: a new folder inside a local directory the
/// user picks (Chromium), else a ZIP of at most 1 GB. Call from the click
/// handler.
export async function saveFolderTarget(folder: string, size: number): Promise<DownloadTarget | undefined> {
  const picker = pickers().showDirectoryPicker;
  if (picker) {
    try {
      const parent = await picker.call(window, { mode: 'readwrite', id: 'farwing-downloads' });
      // A free name: "folder", "folder (2)", …
      let name = safeName(folder);
      for (let n = 2; ; n++) {
        try {
          await parent.getDirectoryHandle(name);
          name = `${safeName(folder)} (${n})`;
        } catch {
          break;
        }
      }
      return directoryTarget(await parent.getDirectoryHandle(name, { create: true }));
    } catch (error) {
      if ((error as DOMException).name === 'AbortError') return undefined;
      // Not allowed here: fall back to a ZIP.
    }
  }
  if (size > MEMORY_LIMIT) throw new FileError(6, '这个浏览器只能下载 1 GB 以内的文件夹');
  return zipTarget(safeName(folder));
}

// Picking and dropping local folders ------------------------------------------------

/// Entries of an `<input webkitdirectory>` selection, grouped by their
/// top-level folder.
export function folderSelection(files: FileList | File[]): { folder: string; entries: UploadEntry[] }[] {
  const groups = new Map<string, UploadEntry[]>();
  for (const file of files) {
    const rel = (file as File & { webkitRelativePath?: string }).webkitRelativePath || file.name;
    const [top, ...rest] = rel.split('/');
    const list = groups.get(top) ?? [];
    list.push({ path: rest.join('/') || file.name, file });
    groups.set(top, list);
  }
  return [...groups].map(([folder, entries]) => ({ folder, entries }));
}

interface FsEntry {
  isFile: boolean;
  isDirectory: boolean;
  name: string;
  file?(ok: (file: File) => void, fail: (error: unknown) => void): void;
  createReader?(): { readEntries(ok: (entries: FsEntry[]) => void, fail: (error: unknown) => void): void };
}

async function walk(dir: FsEntry, prefix: string, out: UploadEntry[]) {
  const reader = dir.createReader!();
  for (;;) {
    const batch = await new Promise<FsEntry[]>((ok, fail) => reader.readEntries(ok, fail));
    if (!batch.length) break;
    for (const entry of batch) {
      const path = prefix ? `${prefix}/${entry.name}` : entry.name;
      if (entry.isDirectory) {
        out.push({ path });
        await walk(entry, path, out);
      } else if (entry.isFile) {
        out.push({ path, file: await new Promise<File>((ok, fail) => entry.file!(ok, fail)) });
      }
    }
  }
}

/// What was dropped: loose files, and folders with their whole trees.
export async function droppedEntries(
  data: DataTransfer,
): Promise<{ files: File[]; folders: { folder: string; entries: UploadEntry[] }[] }> {
  const files: File[] = [];
  const folders: { folder: string; entries: UploadEntry[] }[] = [];
  // Entries must be taken synchronously, before any await.
  const entries = [...data.items].map((item) => (item.webkitGetAsEntry?.() ?? null) as unknown as FsEntry | null);
  const plain = [...data.files];
  for (let i = 0; i < entries.length; i++) {
    const entry = entries[i];
    if (entry?.isDirectory) {
      const list: UploadEntry[] = [];
      await walk(entry, '', list);
      folders.push({ folder: entry.name, entries: list });
    } else if (entry?.isFile) {
      files.push(await new Promise<File>((ok, fail) => entry.file!(ok, fail)));
    } else if (!entry && plain[i]) {
      files.push(plain[i]);
    }
  }
  return { files, folders };
}
