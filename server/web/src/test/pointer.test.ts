// TOUCH and PEN bodies (`docs/protocol/touch-pen-brief.md` §5.9), the same
// bytes the Rust encoder is tested against.
import { describe, expect, test } from 'vitest';
import { hex } from '../lib/encoding';
import { PenPhase, penMessage, penTilt, TouchPhase, touchMessage } from '../lib/input';

const spaced = (bytes: Uint8Array) => hex(bytes).replace(/(..)(?!$)/g, '$1 ');

describe('touch', () => {
  test('contacts', () => {
    expect(spaced(touchMessage(0, [{ id: 1, phase: TouchPhase.DOWN, x: 0.5, y: 0.25 }]))).toBe(
      '08 00 12 0e 08 01 10 01 1d 00 00 00 3f 25 00 00 80 3e',
    );
    expect(
      spaced(
        touchMessage(0, [
          { id: 1, phase: TouchPhase.MOVE, x: 0.75, y: 0.25 },
          { id: 2, phase: TouchPhase.DOWN, x: 0.25, y: 0.5 },
        ]),
      ),
    ).toBe('08 00 12 0e 08 01 10 02 1d 00 00 40 3f 25 00 00 80 3e 12 0e 08 02 10 01 1d 00 00 80 3e 25 00 00 00 3f');
    expect(spaced(touchMessage(0, []))).toBe('08 00');
    expect(
      spaced(
        touchMessage(
          1,
          [
            {
              id: 3,
              phase: TouchPhase.DOWN,
              x: 0.5,
              y: 0.5,
              pressure: 0.5,
              width: 0.01,
              height: 0.02,
              orientation: 90,
            },
          ],
          1_000_000,
        ),
      ),
    ).toBe(
      '08 01 12 1f 08 03 10 01 1d 00 00 00 3f 25 00 00 00 3f 2d 00 00 00 3f 35 0a d7 23 3c 3d 0a d7 a3 3c 40 5a 18 c0 84 3d',
    );
  });
});

describe('pen', () => {
  test('samples', () => {
    expect(spaced(penMessage(0, [{ phase: PenPhase.HOVER, x: 0.5, y: 0.5, tiltX: 30, tiltY: -15 }]))).toBe(
      '08 00 12 10 08 01 15 00 00 00 3f 1d 00 00 00 3f 28 3c 30 1d',
    );
    expect(
      spaced(
        penMessage(0, [
          { phase: PenPhase.MOVE, x: 0.5, y: 0.5, pressure: 0.5, ageUs: 4167 },
          { phase: PenPhase.MOVE, x: 0.75, y: 0.5, pressure: 0.75 },
        ]),
      ),
    ).toBe(
      '08 00 12 14 08 03 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 48 c7 20 12 11 08 03 15 00 00 40 3f 1d 00 00 00 3f 25 00 00 40 3f',
    );
    expect(spaced(penMessage(0, [{ phase: PenPhase.DOWN, x: 0.5, y: 0.5, pressure: 0.5, buttons: 2 }]))).toBe(
      '08 00 12 13 08 02 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 40 02',
    );
    expect(spaced(penMessage(0, [{ phase: PenPhase.LEAVE, x: 0, y: 0 }]))).toBe('08 00 12 02 08 05');
  });

  test('tilt from altitude and azimuth', () => {
    expect(penTilt({ tiltX: 12, tiltY: -3 })).toEqual([12, -3]);
    expect(penTilt({ altitudeAngle: Math.PI / 2, azimuthAngle: 0 })).toEqual([0, 0]);
    // 45° altitude towards +x.
    expect(penTilt({ altitudeAngle: Math.PI / 4, azimuthAngle: 0 })).toEqual([45, 0]);
  });
});
