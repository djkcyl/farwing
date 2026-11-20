import { gunzipSync } from 'node:zlib';

export const MAX_SDP_BYTES = 1024 * 1024;
export const MAX_CANDIDATE_BYTES = 8192;

function varint(value) {
  let number = BigInt(value);
  const bytes = [];
  while (number > 0x7fn) {
    bytes.push(Number((number & 0x7fn) | 0x80n));
    number >>= 7n;
  }
  bytes.push(Number(number));
  return Buffer.from(bytes);
}

function fieldVarint(tag, value) {
  const encoded = typeof value === 'bigint' ? value : BigInt(value);
  // int32 negative values sign-extend to the protobuf's ten-byte varint form.
  return Buffer.concat([varint(BigInt(tag) << 3n), varint(encoded < 0 ? BigInt.asUintN(64, encoded) : encoded)]);
}

function fieldBytes(tag, value) {
  const bytes = Buffer.isBuffer(value) ? value : Buffer.from(value);
  return Buffer.concat([varint((BigInt(tag) << 3n) | 2n), varint(bytes.length), bytes]);
}

function message(...fields) {
  return Buffer.concat(fields);
}

/**
 * Construct the small ConnectOptions message consumed by the Windows host.
 * These values mirror src/session/negotiation.rs: 30 fps, the compatibility
 * quality tier, the local 1920x1080 display, and one H.264 4:2:0 decoder.
 */
export function createConnectOptions(controllerId, platform) {
  const clientType = platform === 'ios' ? 1 : 3;
  const localResolution = message(fieldVarint(1, 1920), fieldVarint(2, 1080));
  const capture = message(
    fieldVarint(1, 1), // 30 fps
    fieldVarint(2, 2), // confirmed compatibility quality tier
    fieldVarint(3, 1), // capture cursor
    fieldVarint(4, 1), // follow the remote resolution
    fieldBytes(5, localResolution),
    fieldVarint(7, 1), // H.264 4:2:0
    fieldVarint(11, 30),
  );
  const decoder = message(
    fieldVarint(1, 30), // 30 fps decoder capacity
    fieldVarint(2, 1), // UU VideoCodec::H264
    fieldVarint(3, 1920),
    fieldVarint(4, 1080),
    fieldVarint(5, 1), // 4:2:0
  );
  const readOnlyFeatureFlag = message(fieldVarint(1, 6)); // CaptureSetting only
  return message(
    fieldVarint(1, 1), // viewing
    fieldVarint(2, -1), // screen_id, int32 -1
    fieldBytes(3, capture),
    fieldBytes(4, decoder),
    fieldVarint(8, clientType),
    fieldBytes(9, controllerId),
    fieldVarint(10, 1), // ordinary connection
    fieldBytes(11, readOnlyFeatureFlag),
    fieldBytes(12, '4.48.1.5606'),
  );
}

export function createStreamerData(iceId) {
  return JSON.stringify({
    device_capability: {
      openuuyc_sdr_10bit: false,
      openuuyc_av1_visible_size: false,
      ice_id: iceId,
      display_info: [],
      video_codec_capability: [{
        video_codec: 1,
        width: 1920,
        height: 1080,
        chroma_sampling: 1,
        bit_depth: 8,
        codec_impl: 37,
      }],
    },
  });
}

export function decompressHostSdp(value) {
  if (typeof value === 'string' && value.length > 0) {
    if (Buffer.byteLength(value, 'utf8') > MAX_SDP_BYTES) throw new Error('SDP exceeds the size limit');
    return value;
  }
  if (!Buffer.isBuffer(value) || value.length === 0) throw new Error('host answer omitted SDP');
  const bytes = value[0] === 0x04 ? value.subarray(1) : value;
  if (bytes.length > MAX_SDP_BYTES) throw new Error('compressed SDP exceeds the size limit');
  const sdp = gunzipSync(bytes, { maxOutputLength: MAX_SDP_BYTES });
  if (sdp.length === 0) throw new Error('host answer omitted SDP');
  return sdp.toString('utf8');
}

export function validateSdp(sdp) {
  if (typeof sdp !== 'string' || sdp.length === 0 || Buffer.byteLength(sdp, 'utf8') > MAX_SDP_BYTES) {
    throw new Error('SDP must be a non-empty string under 1 MiB');
  }
  return sdp;
}

export function validateCandidate(value) {
  if (!value || typeof value !== 'object' || Array.isArray(value)) throw new Error('candidate must be an object');
  if (typeof value.candidate !== 'string' || Buffer.byteLength(value.candidate, 'utf8') > MAX_CANDIDATE_BYTES) {
    throw new Error('candidate has an invalid candidate string');
  }
  const candidate = { candidate: value.candidate };
  if (value.sdpMid === null || typeof value.sdpMid === 'string') {
    if (typeof value.sdpMid === 'string' && value.sdpMid.length > 256) throw new Error('candidate sdpMid is too long');
    candidate.sdpMid = value.sdpMid;
  } else if (value.sdpMid !== undefined) {
    throw new Error('candidate sdpMid is invalid');
  }
  if (value.sdpMLineIndex === null || Number.isInteger(value.sdpMLineIndex)) {
    if (Number.isInteger(value.sdpMLineIndex) && (value.sdpMLineIndex < 0 || value.sdpMLineIndex > 255)) {
      throw new Error('candidate sdpMLineIndex is invalid');
    }
    candidate.sdpMLineIndex = value.sdpMLineIndex;
  } else if (value.sdpMLineIndex !== undefined) {
    throw new Error('candidate sdpMLineIndex is invalid');
  }
  if (value.usernameFragment === null || typeof value.usernameFragment === 'string') {
    if (typeof value.usernameFragment === 'string' && value.usernameFragment.length > 256) throw new Error('candidate usernameFragment is too long');
    candidate.usernameFragment = value.usernameFragment;
  } else if (value.usernameFragment !== undefined) {
    throw new Error('candidate usernameFragment is invalid');
  }
  return candidate;
}

export function isRecord(value) {
  return value !== null && typeof value === 'object' && !Array.isArray(value);
}
