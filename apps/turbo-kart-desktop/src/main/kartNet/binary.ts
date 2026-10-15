// E6.2's fixed diagnostic wire; no JSON or caller-selected socket/track.
export const HEADER = 56;
export const MAX_BODY = HEADER + 4096;
export const MAX_PENDING = 16;
export const MAX_BUFFER = 64 * 1024;
/** E6.3: the player's own WP3 kart state (`Latest` track `state`). */
export const KART_LANE = 4;
export const KART_STATE_LEN = 42;
/** E6.4: host-owned AI kart state (`member` = AI slot). */
export const AI_LANE = 5;
/** E6.5a: authoritative progress snapshot (host renderer send; host → peer delivery). */
export const PROGRESS_LANE = 6;
export const PROGRESS_BODY_LEN = 172;
/** E6.5a: lap / finish / status events; delivered only (Rust derives them). */
export const PROGRESS_EVENT_LANE = 7;

export function body(value: unknown): Buffer {
  if (!(value instanceof ArrayBuffer) && !ArrayBuffer.isView(value)) throw new Error('binary buffer required');
  const b = value instanceof ArrayBuffer ? Buffer.from(value) : Buffer.from(value.buffer, value.byteOffset, value.byteLength);
  if (b.length < HEADER || b.length > MAX_BODY || b.toString('ascii', 0, 4) !== 'TKR2') throw new Error('binary frame size/magic');
  if (!/^[a-fA-F0-9]{32}$/.test(b.toString('ascii', 24, 56)) || b[5] < 1 || b[5] > PROGRESS_EVENT_LANE || b[6] > 7) throw new Error('fixed lane/race/member refused');
  // E6.3 kart state: exactly one WP3 KartState per send (Rust decodes it again).
  if (b[4] === 1 && (b[5] === KART_LANE || b[5] === AI_LANE) && b.length - HEADER !== KART_STATE_LEN) throw new Error('kart state must be one WP3 KartState');
  // E6.5a: one progress body per send (Rust validates it); events are never sent.
  if (b[4] === 1 && b[5] === PROGRESS_LANE && b.length - HEADER !== PROGRESS_BODY_LEN) throw new Error('progress must be one progress body');
  if (b[4] === 1 && b[5] === PROGRESS_EVENT_LANE) throw new Error('progress events are never sent');
  // An AI send names its slot in `member`; Rust checks the sender is the host.
  if (b[4] === 1 && ((b[6] !== 0 && b[5] !== AI_LANE) || b[7] !== 0 || b.readBigUInt64BE(16) !== 0n || b.length - HEADER > (b[5] === 2 ? 4096 : 1024))) throw new Error('binary send shape/size');
  if (b[4]===5 && (b.length!==HEADER || b[6]!==0 || b[7]!==0 || b.readBigUInt64BE(16)!==0n)) throw new Error('invalid disconnect probe');
  return b;
}
export function frame(b: Buffer): Buffer { const n=Buffer.alloc(4);n.writeUInt32BE(b.length);return Buffer.concat([n,b]); }
export class BinaryDecoder {
  private buffer: Buffer = Buffer.alloc(0);
  push(chunk: Buffer): Buffer[] {
    if (chunk.length + this.buffer.length > MAX_BUFFER) throw new Error('binary receive window exceeded');
    this.buffer = Buffer.concat([this.buffer,chunk]);
    const out:Buffer[]=[];
    while (this.buffer.length >= 4) {
      const n=this.buffer.readUInt32BE(0);
      if (n < HEADER || n > MAX_BODY) throw new Error('binary length refused');
      if (this.buffer.length < n+4) break;
      out.push(body(this.buffer.subarray(4,n+4)));this.buffer=this.buffer.subarray(n+4);
    }
    return out;
  }
}
