// ANNOTATION requests (`docs/protocol/peer-messages.md`, 0x0601).
import { describe, expect, test } from 'vitest';
import { op } from '../lib/annotation';
import { hex } from '../lib/encoding';
import { Fields } from '../lib/pb';

const spaced = (bytes: Uint8Array) => hex(bytes).replace(/(..)(?!$)/g, '$1 ');

describe('annotation', () => {
  test('show is always written', () => {
    expect(spaced(op.show(true))).toBe('08 01');
    expect(spaced(op.show(false))).toBe('08 00');
  });

  test('strokes always carry screen, width and colour', () => {
    const stroke = new Fields(op.draw(7, 0, [{ x: 0.5, y: 0 }], 4, 0xffff4444)).message(2)!;
    expect(stroke.uint32(1)).toBe(7);
    expect(stroke.has(2)).toBe(true);
    expect(stroke.int32(2)).toBe(0);
    expect(stroke.float(4)).toBe(4);
    expect(stroke.fixed32(5)).toBe(0xffff4444);
    const p = stroke.messages(3)[0];
    expect([p.float(1), p.float(2)]).toEqual([0.5, 0]);
  });

  test('erase, clear and board', () => {
    const erase = new Fields(op.erase(9, 2)).message(4)!;
    expect([erase.uint32(1), erase.int32(2)]).toEqual([9, 2]);
    expect(spaced(op.clear(false))).toBe('28 00');
    expect(spaced(op.clear(true))).toBe('28 01');
    expect(spaced(op.board(0))).toBe('32 02 08 00');
    expect(spaced(op.board(1, 0xffffffff))).toBe('32 07 08 01 15 ff ff ff ff');
  });
});
