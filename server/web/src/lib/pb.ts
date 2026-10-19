// A small protobuf (proto3) codec for the peer protocol's messages
// (`docs/protocol/web-viewer-brief.md` §4.3, `web-control-brief.md` §0.3).
// Only the wire types the peer protocol uses: varint (also zigzag), 64-bit,
// length-delimited and 32-bit.

export class Writer {
  private buf = new Uint8Array(64);
  private len = 0;

  private reserve(n: number) {
    if (this.len + n <= this.buf.length) return;
    const next = new Uint8Array(Math.max(this.buf.length * 2, this.len + n));
    next.set(this.buf.subarray(0, this.len));
    this.buf = next;
  }

  private byte(b: number) {
    this.reserve(1);
    this.buf[this.len++] = b;
  }

  private append(bytes: Uint8Array) {
    this.reserve(bytes.length);
    this.buf.set(bytes, this.len);
    this.len += bytes.length;
  }

  private varint(value: bigint) {
    let v = BigInt.asUintN(64, value);
    while (v >= 0x80n) {
      this.byte(Number(v & 0x7fn) | 0x80);
      v >>= 7n;
    }
    this.byte(Number(v));
  }

  private tag(field: number, wire: number) {
    this.varint(BigInt((field << 3) | wire));
  }

  /// int32/int64/uint32/uint64/bool/enum; zero is not written (proto3).
  int(field: number, value: number | bigint | boolean) {
    const v = typeof value === 'boolean' ? (value ? 1n : 0n) : BigInt(value);
    if (v === 0n) return this;
    this.tag(field, 0);
    this.varint(v);
    return this;
  }

  /// Like `int`, but written even when zero (proto3 `optional`).
  intAlways(field: number, value: number | bigint) {
    this.tag(field, 0);
    this.varint(BigInt(value));
    return this;
  }

  /// sint32 / sint64 (zigzag).
  sint(field: number, value: number) {
    if (value === 0) return this;
    const v = BigInt(Math.trunc(value));
    this.tag(field, 0);
    this.varint(v >= 0n ? v << 1n : (-v << 1n) - 1n);
    return this;
  }

  float(field: number, value: number) {
    if (value === 0) return this;
    this.tag(field, 5);
    const view = new DataView(new ArrayBuffer(4));
    view.setFloat32(0, value, true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  double(field: number, value: number) {
    if (value === 0) return this;
    this.tag(field, 1);
    const view = new DataView(new ArrayBuffer(8));
    view.setFloat64(0, value, true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  fixed64(field: number, value: bigint) {
    if (value === 0n) return this;
    this.tag(field, 1);
    const view = new DataView(new ArrayBuffer(8));
    view.setBigUint64(0, BigInt.asUintN(64, value), true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  /// Like `fixed32`, but written even when zero (proto3 `optional`).
  fixed32Always(field: number, value: number) {
    this.tag(field, 5);
    const view = new DataView(new ArrayBuffer(4));
    view.setUint32(0, value >>> 0, true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  /// Like `float`, but written even when zero (proto3 `optional`).
  floatAlways(field: number, value: number) {
    this.tag(field, 5);
    const view = new DataView(new ArrayBuffer(4));
    view.setFloat32(0, value, true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  fixed32(field: number, value: number) {
    if (value === 0) return this;
    this.tag(field, 5);
    const view = new DataView(new ArrayBuffer(4));
    view.setUint32(0, value >>> 0, true);
    this.append(new Uint8Array(view.buffer));
    return this;
  }

  raw(field: number, value: Uint8Array, always = false) {
    if (value.length === 0 && !always) return this;
    this.tag(field, 2);
    this.varint(BigInt(value.length));
    this.append(value);
    return this;
  }

  string(field: number, value: string) {
    return this.raw(field, new TextEncoder().encode(value));
  }

  /// An embedded message; written even when empty, like prost's `Some(..)`.
  message(field: number, value: Writer) {
    return this.raw(field, value.finish(), true);
  }

  /// Packed repeated int32/uint64.
  packed(field: number, values: (number | bigint)[]) {
    if (values.length === 0) return this;
    const inner = new Writer();
    for (const v of values) inner.varint(BigInt(v));
    return this.raw(field, inner.finish());
  }

  finish(): Uint8Array {
    return this.buf.slice(0, this.len);
  }
}

export type Value =
  | { wire: 0; value: bigint }
  | { wire: 1; value: DataView }
  | { wire: 2; value: Uint8Array }
  | { wire: 5; value: DataView };

/// Decoded fields: field number → values in order of appearance.
export class Fields {
  private map = new Map<number, Value[]>();

  constructor(bytes: Uint8Array) {
    let at = 0;
    const varint = (): bigint => {
      let result = 0n;
      let shift = 0n;
      for (;;) {
        if (at >= bytes.length) throw new Error('truncated varint');
        const b = bytes[at++];
        result |= BigInt(b & 0x7f) << shift;
        if ((b & 0x80) === 0) return result;
        shift += 7n;
        if (shift > 63n) throw new Error('varint too long');
      }
    };
    while (at < bytes.length) {
      const key = Number(varint());
      const field = key >>> 3;
      const wire = key & 7;
      let value: Value;
      switch (wire) {
        case 0:
          value = { wire, value: varint() };
          break;
        case 1:
          if (at + 8 > bytes.length) throw new Error('truncated fixed64');
          value = { wire, value: new DataView(bytes.buffer, bytes.byteOffset + at, 8) };
          at += 8;
          break;
        case 2: {
          const length = Number(varint());
          if (at + length > bytes.length) throw new Error('truncated bytes');
          value = { wire, value: bytes.subarray(at, at + length) };
          at += length;
          break;
        }
        case 5:
          if (at + 4 > bytes.length) throw new Error('truncated fixed32');
          value = { wire, value: new DataView(bytes.buffer, bytes.byteOffset + at, 4) };
          at += 4;
          break;
        default:
          throw new Error(`unsupported wire type ${wire}`);
      }
      const list = this.map.get(field);
      if (list) list.push(value);
      else this.map.set(field, [value]);
    }
  }

  has(field: number): boolean {
    return this.map.has(field);
  }

  private last(field: number): Value | undefined {
    const list = this.map.get(field);
    return list?.[list.length - 1];
  }

  /// int32 (sign-extended from 64 bits).
  int32(field: number): number {
    const v = this.last(field);
    return v?.wire === 0 ? Number(BigInt.asIntN(32, v.value)) : 0;
  }

  uint32(field: number): number {
    const v = this.last(field);
    return v?.wire === 0 ? Number(BigInt.asUintN(32, v.value)) : 0;
  }

  int64(field: number): number {
    const v = this.last(field);
    return v?.wire === 0 ? Number(BigInt.asIntN(64, v.value)) : 0;
  }

  /// sint32 (zigzag).
  sint32(field: number): number {
    const v = this.last(field);
    if (v?.wire !== 0) return 0;
    const n = BigInt.asUintN(64, v.value);
    return Number(n & 1n ? -((n + 1n) >> 1n) : n >> 1n);
  }

  /// uint64 as a bigint (sizes and offsets).
  big(field: number): bigint {
    const v = this.last(field);
    return v?.wire === 0 ? BigInt.asUintN(64, v.value) : 0n;
  }

  /// Repeated uint64, packed or not.
  bigs(field: number): bigint[] {
    const out: bigint[] = [];
    for (const v of this.map.get(field) ?? []) {
      if (v.wire === 0) out.push(BigInt.asUintN(64, v.value));
      else if (v.wire === 2) {
        let at = 0;
        while (at < v.value.length) {
          let result = 0n;
          let shift = 0n;
          for (;;) {
            if (at >= v.value.length) throw new Error('truncated packed varint');
            const b = v.value[at++];
            result |= BigInt(b & 0x7f) << shift;
            if ((b & 0x80) === 0) break;
            shift += 7n;
          }
          out.push(result);
        }
      }
    }
    return out;
  }

  /// uint64 as a decimal string (participant IDs exceed 2^53).
  uint64(field: number): string {
    const v = this.last(field);
    return v?.wire === 0 ? BigInt.asUintN(64, v.value).toString() : '0';
  }

  bool(field: number): boolean {
    const v = this.last(field);
    return v?.wire === 0 ? v.value !== 0n : false;
  }

  float(field: number): number {
    const v = this.last(field);
    return v?.wire === 5 ? v.value.getFloat32(0, true) : 0;
  }

  double(field: number): number {
    const v = this.last(field);
    return v?.wire === 1 ? v.value.getFloat64(0, true) : 0;
  }

  fixed32(field: number): number {
    const v = this.last(field);
    return v?.wire === 5 ? v.value.getUint32(0, true) : 0;
  }

  fixed64(field: number): bigint {
    const v = this.last(field);
    return v?.wire === 1 ? v.value.getBigUint64(0, true) : 0n;
  }

  bytes(field: number): Uint8Array {
    const v = this.last(field);
    return v?.wire === 2 ? v.value : new Uint8Array();
  }

  string(field: number): string {
    return new TextDecoder().decode(this.bytes(field));
  }

  message(field: number): Fields | undefined {
    const v = this.last(field);
    return v?.wire === 2 ? new Fields(v.value) : undefined;
  }

  messages(field: number): Fields[] {
    return (this.map.get(field) ?? []).flatMap((v) => (v.wire === 2 ? [new Fields(v.value)] : []));
  }

  strings(field: number): string[] {
    return (this.map.get(field) ?? []).flatMap((v) => (v.wire === 2 ? [new TextDecoder().decode(v.value)] : []));
  }
}
