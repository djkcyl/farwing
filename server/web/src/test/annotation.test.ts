// ANNOTATION packets (`docs/protocol/peer-messages.md`, 0x0601).
import { describe, expect, test } from 'vitest';
import { boardCommand, clearCommand, eraseCommand, packet, strokeCommand, toggleCommand } from '../lib/annotation';
import { hex } from '../lib/encoding';
import { Fields } from '../lib/pb';

const spaced = (bytes: Uint8Array) => hex(bytes).replace(/(..)(?!$)/g, '$1 ');

describe('annotation', () => {
  test('packet header: fixed64 token, request, command', () => {
    expect(spaced(packet(0x0102030405060708n, 1, toggleCommand(true)))).toBe(
      '09 08 07 06 05 04 03 02 01 10 01 22 06 0a 04 1a 02 08 01',
    );
    expect(spaced(toggleCommand(false).finish())).toBe('0a 02 1a 00');
  });

  test('strokes always carry screen, width and colour', () => {
    const body = strokeCommand(7, 0, [{ x: 0.5, y: 0 }], 4, 0xffff4444).finish();
    const stroke = new Fields(body).message(1)!.message(1)!;
    expect(stroke.uint32(1)).toBe(7);
    expect(stroke.has(3)).toBe(true);
    expect(stroke.int32(3)).toBe(0);
    expect(stroke.float(4)).toBe(4);
    expect(stroke.uint32(5)).toBe(0xffff4444);
    const p = stroke.messages(2)[0];
    expect([p.float(1), p.float(2)]).toEqual([0.5, 0]);
  });

  test('erase, clear and board', () => {
    const erase = new Fields(eraseCommand(9, 2).finish()).message(1)!.message(2)!;
    expect([erase.int32(1), erase.uint32(2), erase.int32(3)]).toEqual([2, 9, 2]);
    expect(spaced(clearCommand(false).finish())).toBe('28 00');
    expect(spaced(clearCommand(true).finish())).toBe('28 01');
    expect(spaced(boardCommand(0).finish())).toBe('22 00');
    expect(spaced(boardCommand(1, 0xffffffff).finish())).toBe('22 07 08 01 15 ff ff ff ff');
  });
});
