// Peer protocol bytes (`docs/protocol/web-viewer-brief.md` §4) and device
// identity derivation.
import { describe, expect, test } from 'vitest';
import { deviceIdOf, hex, unb64 } from '../lib/encoding';
import * as fpp from '../lib/fpp';
import { Writer } from '../lib/pb';
import { fingerprints, withoutAv1Level } from '../lib/sdp';

describe('peer protocol', () => {
  test('frame header', () => {
    const frame = fpp.encode(fpp.Type.HELLO, new Uint8Array([1, 2]), fpp.Flag.RESPONSE, 7);
    expect(hex(frame)).toBe('4657020100010000000701' + '02');
    const parsed = fpp.decode(frame.buffer as ArrayBuffer)!;
    expect(parsed).toMatchObject({ flags: 1, type: 1, requestId: 7 });
    expect(fpp.decode(new Uint8Array([0x46, 0x57, 1, 0, 0, 1, 0, 0, 0, 0]).buffer)).toBeUndefined();
  });

  test('minimal HELLO as in the brief', () => {
    const w = new Writer().int(1, 2).int(2, 2).int(3, 1).string(4, 'web');
    w.message(5, new Writer().string(1, 'video.h264').int(2, 1));
    w.int(6, 262144).string(7, 'web');
    expect(hex(w.finish())).toBe(
      '0802' + '1002' + '1801' + '2203776562' + '2a0e0a0a766964656f2e683236341001' + '30808010' + '3a03776562',
    );
  });

  test('our HELLO round-trips and is compatible', () => {
    const hello = fpp.parseHello(fpp.helloBody(262144));
    expect(hello.protocolVersion).toBe(2);
    expect(hello.minProtocolVersion).toBe(2);
    expect(hello.role).toBe(1);
    expect(hello.features['video.h264']).toBe(1);
    expect(hello.features['display.config']).toBeUndefined();
    expect(fpp.compatible(hello)).toBe(true);
  });

  test('switching screens (SESSION capture)', () => {
    const frame = fpp.encode(fpp.Type.SESSION, fpp.captureControl(1, true));
    expect(hex(frame)).toBe('46570200020100000000' + '2a0408011001');
    expect(hex(fpp.captureControl(-1, true))).toBe('2a0d08ffffffffffffffffff011001');
  });

  test('video acknowledgement (VideoFeedback with SSRC and RTP timestamp)', () => {
    // SessionBody tag 8; fixed32 ssrc (1) and rtp_timestamp (7), even when 0.
    expect(hex(fpp.videoAcknowledgement(0x11223344, 0))).toBe('420a' + '0d44332211' + '3d00000000');
  });

  test('parses screens', () => {
    const screen = new Writer()
      .int(1, 1)
      .int(2, 60)
      .message(4, new Writer().int(3, 2560).int(4, 1440))
      .int(7, true)
      .string(10, 'DELL')
      .int(12, -1);
    const body = new Writer().message(1, new Writer().message(1, screen).int(2, 1)).finish();
    const m = fpp.parseSession(body);
    expect(m.kind).toBe('screens');
    if (m.kind !== 'screens') return;
    expect(m.value.current).toBe(1);
    expect(m.value.screens[0]).toMatchObject({ id: 1, fps: 60, primary: true, name: 'DELL', track: -1 });
    expect(m.value.screens[0].current).toMatchObject({ width: 2560, height: 1440 });
  });
});

describe('identity', () => {
  test('device ID from a public key (Rust local_device_id)', async () => {
    expect(await deviceIdOf(unb64('AeuHcosWEpLtuiUrkPZuzu8IakftDesJV9cOKodJl4w'))).toBe('n4ruegpwsfx4ltg3');
  });

  test('DTLS fingerprints: all lines, lower case, sorted, unique', () => {
    const sdp =
      'v=0\r\na=fingerprint:sha-256 AB:CD\r\nm=audio 9\r\na=fingerprint:sha-256 ab:cd\r\na=fingerprint:sha-256 01:02 \r\n';
    expect(fingerprints(sdp)).toEqual(['sha-256 01:02', 'sha-256 ab:cd']);
  });

  test('AV1 offers leave level-idx out for the host', () => {
    const sdp =
      'm=video 9 UDP/TLS/RTP/SAVPF 45 46 96\r\na=rtpmap:45 AV1/90000\r\na=fmtp:45 level-idx=5;profile=0;tier=0\r\n' +
      'a=rtpmap:46 AV1/90000\r\na=fmtp:46 level-idx=5\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 level-idx=5\r\n';
    expect(withoutAv1Level(sdp)).toBe(
      'm=video 9 UDP/TLS/RTP/SAVPF 45 46 96\r\na=rtpmap:45 AV1/90000\r\na=fmtp:45 profile=0;tier=0\r\n' +
        'a=rtpmap:46 AV1/90000\r\na=rtpmap:96 H264/90000\r\na=fmtp:96 level-idx=5\r\n',
    );
  });
});
