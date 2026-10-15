// Protocol v1 with the kart-net sidecar (sidecar/README.md): length-prefixed
// JSON frames. Shared by the host (main) and its tests; no Electron imports.

export const PROTOCOL_VERSION = 1;
/** Largest frame either side accepts. */
export const MAX_FRAME_BYTES = 64 * 1024;
/** Origin of the dedicated app interfaces (only the sidecar uses it). */
export const SIDECAR_ORIGIN = 'turbo-kart-sidecar';

/** The commands main forwards; the sidecar has the same allowlist. */
export const COMMANDS = [
  'net_init',
  'lobby_create',
  'lobby_list',
  'lobby_get',
  'lobby_join',
  'lobby_leave',
  'race_start',
  'race_get',
  'session_start',
  'session_status',
  'session_stop',
  'session_ready',
] as const;
export type CommandName = (typeof COMMANDS)[number];

/**
 * kart-net's codes (unchanged), the sidecar's, and main's own:
 * `sidecar_unavailable` (not running / crashed / restarting), `forbidden`
 * (IPC from anything but the game window), `not_propagated` (a lobby write
 * Holochain could not commit yet because another agent's record has not
 * reached this conductor; nothing was written), `unsupported_platform` (this
 * build cannot take part in online races, E7.WINDOWS.1; nothing was sent).
 */
export type ErrorCode =
  | 'invalid_input'
  | 'not_found'
  | 'agent_mismatch'
  | 'control_plane'
  | 'not_host'
  | 'race_not_ready'
  | 'not_initialised'
  | 'unknown_command'
  | 'bad_request'
  | 'busy'
  | 'timeout'
  | 'internal'
  | 'sidecar_unavailable'
  | 'not_propagated'
  | 'forbidden'
  | 'unsupported_platform';

export interface KartNetError {
  code: ErrorCode;
  message: string;
}

export function kartNetError(code: ErrorCode, message: string): KartNetError {
  return { code, message };
}

export function isKartNetError(e: unknown): e is KartNetError {
  return (
    typeof e === 'object' &&
    e !== null &&
    typeof (e as KartNetError).code === 'string' &&
    typeof (e as KartNetError).message === 'string'
  );
}

/** Encodes one message; refuses anything over the frame limit. */
export function encodeFrame(msg: unknown): Buffer {
  const body = Buffer.from(JSON.stringify(msg), 'utf-8');
  if (body.length > MAX_FRAME_BYTES) throw new Error('frame too large');
  const head = Buffer.alloc(4);
  head.writeUInt32BE(body.length, 0);
  return Buffer.concat([head, body]);
}

/**
 * Incremental frame decoder. A declared length over the limit is a protocol
 * error (the stream cannot be resynchronised); the caller closes the socket.
 */
export class FrameDecoder {
  private buf: Buffer = Buffer.alloc(0);

  /** Returns the complete messages so far; throws on a protocol error. */
  push(chunk: Buffer): unknown[] {
    this.buf = this.buf.length ? Buffer.concat([this.buf, chunk]) : chunk;
    const out: unknown[] = [];
    while (this.buf.length >= 4) {
      const len = this.buf.readUInt32BE(0);
      if (len > MAX_FRAME_BYTES) throw new Error(`frame of ${len} bytes exceeds ${MAX_FRAME_BYTES}`);
      if (this.buf.length < 4 + len) break;
      const body = this.buf.subarray(4, 4 + len);
      this.buf = this.buf.subarray(4 + len);
      out.push(JSON.parse(body.toString('utf-8')));
    }
    return out;
  }
}
