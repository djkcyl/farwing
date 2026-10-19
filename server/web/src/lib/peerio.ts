// What the clipboard and file modules need from the peer connection: session
// channel requests, replies and pushes, and paced sends on the bulk channel.

export interface PeerIo {
  /// A request on the session channel; resolves with the response body,
  /// rejects with `PeerRequestError` on an error response or timeout.
  request(type: number, body: Uint8Array, timeoutMs?: number): Promise<Uint8Array>;
  /// A push (request ID 0) on the session channel.
  push(type: number, body: Uint8Array): void;
  /// Answer a request of the host.
  reply(type: number, requestId: number, body: Uint8Array): void;
  replyError(type: number, requestId: number, code: string, message?: string): void;
  /// Send on the bulk channel once it has room.
  bulk(type: number, body: Uint8Array): Promise<void>;
}

export class PeerRequestError extends Error {
  code: string;
  constructor(code: string, message: string) {
    super(message || code);
    this.code = code;
  }
}
