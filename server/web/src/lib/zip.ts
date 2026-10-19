// A stored (uncompressed) ZIP archive built in memory: how browsers without
// the File System Access API receive a downloaded folder. Up to 65 535
// entries and 4 GiB (no ZIP64); names in UTF-8.

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

/// CRC-32 of `data`, continuing from `crc` (0 to start).
export function crc32(data: Uint8Array, crc = 0): number {
  let c = (crc ^ 0xffffffff) >>> 0;
  for (let i = 0; i < data.length; i++) c = CRC_TABLE[(c ^ data[i]) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function dosTime(date: Date): [number, number] {
  const year = Math.min(Math.max(date.getFullYear(), 1980), 2107);
  return [
    (date.getHours() << 11) | (date.getMinutes() << 5) | (date.getSeconds() >> 1),
    ((year - 1980) << 9) | ((date.getMonth() + 1) << 5) | date.getDate(),
  ];
}

interface Open {
  name: Uint8Array;
  dir: boolean;
  time: number;
  date: number;
  crc: number;
  size: number;
  chunks: Uint8Array[];
}

export class ZipWriter {
  private parts: Uint8Array[] = [];
  private central: Uint8Array[] = [];
  private offset = 0;
  private count = 0;
  private current?: Open;

  /// Start an entry (closing the previous one); directories end with '/'
  /// in the archive.
  begin(path: string, dir: boolean, modified: Date) {
    this.end();
    if (this.count >= 0xffff) throw new Error('压缩包里的项目太多');
    const [time, date] = dosTime(modified);
    const name = new TextEncoder().encode(dir && !path.endsWith('/') ? `${path}/` : path);
    this.current = { name, dir, time, date, crc: 0, size: 0, chunks: [] };
  }

  write(data: Uint8Array) {
    const c = this.current;
    if (!c || c.dir) throw new Error('no open file entry');
    c.crc = crc32(data, c.crc);
    c.size += data.length;
    c.chunks.push(data);
  }

  /// Close the entry: its local header, data and central record.
  end() {
    const c = this.current;
    if (!c) return;
    this.current = undefined;
    if (this.offset + 30 + c.name.length + c.size > 0xffffffff) throw new Error('压缩包超过 4 GB');
    const local = new Uint8Array(30 + c.name.length);
    const l = new DataView(local.buffer);
    l.setUint32(0, 0x04034b50, true);
    l.setUint16(4, 20, true);
    l.setUint16(6, 0x0800, true);
    l.setUint16(8, 0, true);
    l.setUint16(10, c.time, true);
    l.setUint16(12, c.date, true);
    l.setUint32(14, c.crc, true);
    l.setUint32(18, c.size, true);
    l.setUint32(22, c.size, true);
    l.setUint16(26, c.name.length, true);
    l.setUint16(28, 0, true);
    local.set(c.name, 30);
    const record = new Uint8Array(46 + c.name.length);
    const r = new DataView(record.buffer);
    r.setUint32(0, 0x02014b50, true);
    r.setUint16(4, 20, true);
    r.setUint16(6, 20, true);
    r.setUint16(8, 0x0800, true);
    r.setUint16(10, 0, true);
    r.setUint16(12, c.time, true);
    r.setUint16(14, c.date, true);
    r.setUint32(16, c.crc, true);
    r.setUint32(20, c.size, true);
    r.setUint32(24, c.size, true);
    r.setUint16(28, c.name.length, true);
    // Extra, comment, disk and internal attributes stay 0.
    r.setUint32(38, c.dir ? 0x10 : 0, true);
    r.setUint32(42, this.offset, true);
    record.set(c.name, 46);
    this.parts.push(local, ...c.chunks);
    this.central.push(record);
    this.offset += local.length + c.size;
    this.count += 1;
  }

  /// The archive: entries, central directory and its end record.
  finish(): Blob {
    this.end();
    const size = this.central.reduce((n, r) => n + r.length, 0);
    const tail = new Uint8Array(22);
    const t = new DataView(tail.buffer);
    t.setUint32(0, 0x06054b50, true);
    t.setUint16(8, this.count, true);
    t.setUint16(10, this.count, true);
    t.setUint32(12, size, true);
    t.setUint32(16, this.offset, true);
    return new Blob([...this.parts, ...this.central, tail] as BlobPart[], { type: 'application/zip' });
  }
}
