// Wire bytes of the control messages, from `docs/protocol/web-control-brief.md`
// (§1.1, §2.9, §3.7, §4.7, §5.2).
import { describe, expect, test } from 'vitest';
import { dataMessage, decodeText, encodeText, fetchMessage, offerMessage, pullMessage } from '../lib/clipboard';
import { hex } from '../lib/encoding';
import {
  downloadMessage,
  downloadStartMessage,
  listMessage,
  safeName,
  uniqueNames,
  uploadMessage,
  uploadStartMessage,
} from '../lib/files';
import * as fpp from '../lib/fpp';
import {
  heartbeat,
  KEY_USAGE,
  keyMessage,
  lockStateMessage,
  mouseAbsolute,
  mouseButton,
  mouseRelative,
  mouseWheel,
  systemCommand,
} from '../lib/input';
import { Fields } from '../lib/pb';

const spaced = (bytes: Uint8Array) => hex(bytes).replace(/(..)(?!$)/g, '$1 ');

describe('input', () => {
  test('keys', () => {
    expect(KEY_USAGE.KeyA).toBe(0x070004);
    expect(KEY_USAGE.KeyZ).toBe(0x07001d);
    expect(KEY_USAGE.Digit0).toBe(0x070027);
    expect(KEY_USAGE.F13).toBe(0x070068);
    expect(KEY_USAGE.MetaLeft).toBe(0x0700e3);
    expect(KEY_USAGE.AudioVolumeMute).toBe(0x0c00e2);
    expect(spaced(keyMessage(0x070004, true))).toBe('0a 06 08 84 80 1c 10 01');
    expect(spaced(keyMessage(0x070004, true, true))).toBe('0a 08 08 84 80 1c 10 01 18 01');
    expect(spaced(keyMessage(0x070004, false))).toBe('0a 04 08 84 80 1c');
    // Releases never carry repeat.
    expect(spaced(keyMessage(0x070004, false, true))).toBe('0a 04 08 84 80 1c');
    expect(spaced(keyMessage(0x0700e3, true))).toBe('0a 06 08 e3 81 1c 10 01');
    expect(spaced(keyMessage(0x0c00e2, true))).toBe('0a 06 08 e2 81 30 10 01');
    expect(spaced(lockStateMessage(true, true, false, 3))).toBe('12 06 08 01 10 01 20 03');
  });

  test('mouse', () => {
    expect(spaced(mouseAbsolute(0, 0.5, 0.25))).toBe(
      '1a 14 08 00 11 00 00 00 00 00 00 e0 3f 19 00 00 00 00 00 00 d0 3f',
    );
    expect(spaced(mouseRelative(-5, 7))).toBe('22 04 08 09 10 0e');
    expect(spaced(mouseButton(1, true))).toBe('2a 04 08 01 10 01');
    expect(spaced(mouseButton(1, false))).toBe('2a 02 08 01');
    expect(spaced(mouseButton(2, true))).toBe('2a 04 08 02 10 01');
    expect(spaced(mouseButton(16, true))).toBe('2a 04 08 10 10 01');
    expect(spaced(mouseWheel(0, -120))).toBe('32 03 10 ef 01');
    expect(spaced(mouseWheel(0, 120))).toBe('32 03 10 f0 01');
    expect(spaced(systemCommand(1))).toBe('38 01');
    expect(spaced(heartbeat())).toBe('42 00');
    // Zigzag reads back.
    expect(new Fields(new Fields(mouseRelative(-5, 7)).bytes(4)).sint32(1)).toBe(-5);
  });
});

describe('session', () => {
  test('control turn and microphone', () => {
    expect(spaced(fpp.encode(fpp.Type.SESSION, fpp.turnRequest(true), 0, 1))).toBe(
      '46 57 02 00 02 01 00 00 00 01 8a 01 02 08 01',
    );
    expect(spaced(fpp.turnRequest(true, true))).toBe('8a 01 04 08 01 10 01');
    expect(spaced(fpp.turnRequest(false))).toBe('8a 01 00');
    const held = fpp.parseSession(Uint8Array.from([0xba, 0x01, 0x02, 0x08, 0x01]));
    expect(held).toEqual({ kind: 'turn_result', held: true, refusal: 0, holder: '' });
    expect(spaced(fpp.microphonePolicy(true))).toBe('82 01 02 08 01');
    expect(spaced(fpp.microphonePolicy(false))).toBe('82 01 00');
    expect(fpp.parseSession(Uint8Array.from([0xb2, 0x01, 0x00]))).toEqual({ kind: 'microphone_result', code: 0 });
    const denied = fpp.parseSession(Uint8Array.from([0xb2, 0x01, 0x05, 0x08, 0x86, 0xe0, 0x80, 0x70]));
    expect(denied).toEqual({ kind: 'microphone_result', code: 0x0e003006 });
    expect(fpp.parseSession(Uint8Array.from([0x1a, 0x02, 0x08, 0x01]))).toEqual({
      kind: 'microphone_state',
      state: 1,
    });
  });

  test('fragments join per request', () => {
    const body = Uint8Array.from({ length: 150 }, (_, i) => i);
    const frames = fpp.encodeFragments(fpp.Type.FILE, body, 64, fpp.Flag.RESPONSE, 7);
    expect(frames.length).toBe(3);
    const join = new fpp.Reassembly(1000);
    const out = frames.map((f) => join.push(fpp.decode(f.slice().buffer)!));
    expect(out[0]).toBeUndefined();
    expect(out[1]).toBeUndefined();
    expect(out[2]?.body).toEqual(body);
    expect(out[2]?.flags).toBe(fpp.Flag.RESPONSE);
    expect(() => new fpp.Reassembly(100).push(fpp.decode(frames[0].slice().buffer)!)).not.toThrow();
  });
});

describe('clipboard', () => {
  test('messages', () => {
    expect(spaced(offerMessage(1, [13]))).toBe('0a 06 08 01 12 02 08 0d');
    expect(spaced(fetchMessage(1, 13, 5))).toBe('12 08 08 01 12 02 08 0d 18 05');
    expect(spaced(dataMessage(12, encodeText('hello')))).toBe('1a 10 08 0c 12 0c 68 00 65 00 6c 00 6c 00 6f 00 00 00');
    expect(spaced(pullMessage())).toBe('4a 00');
    expect(spaced(fpp.flow({ stream: 1, credit: 5n * 1024n * 1024n, kind: 2 }))).toBe(
      '08 01 10 01 18 80 80 c0 02 20 02',
    );
  });

  test('text', () => {
    expect(spaced(encodeText('a\nb'))).toBe('61 00 0d 00 0a 00 62 00 00 00');
    expect(decodeText(Uint8Array.from([0x61, 0, 0x0d, 0, 0x0a, 0, 0x62, 0, 0, 0, 0x7a, 0]))).toBe('a\nb');
    expect(decodeText(encodeText('中文 😀'))).toBe('中文 😀');
  });
});

describe('files', () => {
  test('messages', () => {
    expect(spaced(listMessage(''))).toBe('0a 00');
    expect(spaced(uploadMessage(1, ':/Default', [{ path: 'a.txt', size: 5n, mtimeNs: 0n }], 'k1'))).toBe(
      '32 1e 08 01 12 09 3a 2f 44 65 66 61 75 6c 74 22 09 0a 05 61 2e 74 78 74 10 05 28 02 32 02 6b 31',
    );
    expect(spaced(uploadStartMessage(1, [0n]))).toBe('3a 05 08 01 12 01 00');
    expect(spaced(downloadMessage(2, ['C:\\a.txt']))).toBe('42 0c 08 02 12 08 43 3a 5c 61 2e 74 78 74');
    expect(spaced(downloadStartMessage(2, 1))).toBe('4a 04 08 02 12 00');
    expect(
      spaced(
        fpp.chunk({ stream: 1, kind: 1, offset: 0n, data: new TextEncoder().encode('hello'), last: true, entry: 0 }),
      ),
    ).toBe('08 01 10 01 22 05 68 65 6c 6c 6f 28 01');
    expect(spaced(fpp.flow({ stream: 2, credit: 5n * 1024n * 1024n, kind: 1 }))).toBe(
      '08 01 10 02 18 80 80 c0 02 20 01',
    );
    // Accepted{[0]} reads back as packed uint64.
    expect(new Fields(Uint8Array.from([0x0a, 0x01, 0x00])).bigs(1)).toEqual([0n]);
  });

  test('names', () => {
    expect(safeName('a:b?.txt')).toBe('a_b_.txt');
    expect(safeName('report. ')).toBe('report');
    expect(safeName('CON.txt')).toBe('_CON.txt');
    expect(uniqueNames(['a.txt', 'A.TXT', 'b'])).toEqual(['a.txt', 'A (2).TXT', 'b']);
  });
});
