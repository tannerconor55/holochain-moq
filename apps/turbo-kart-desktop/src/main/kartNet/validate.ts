// Argument validation in Electron main for the renderer's kart-net calls.
// Main checks shape and size (closed objects, exact types, byte bounds equal
// to the DNA's own maxima); what a value means (a known character, track,
// class, player count) stays kart-net's and the DNA's to judge, so their
// `invalid_input` codes are unchanged. No Electron imports (unit-tested in Node).

import { CommandName, KartNetError, kartNetError } from './protocol';

// crates/turbo-kart/src/lib.rs
const MAX_LOBBY_NAME_BYTES = 128;
const MAX_TRACK_ID_LEN = 32;
const MAX_CLASS_LEN = 8;
const MAX_CHARACTER_ID_LEN = 16;
const MAX_INVITE_CODE_LEN = 64;
const MAX_RELAY_URLS = 4;
const MAX_RELAY_URL_LEN = 256;
export const MAX_LIST_LIMIT = 50;
export const DEFAULT_LIST_LIMIT = 20;
/** Lobby ids are base64url action hashes (53 chars); allow some slack, no other characters. */
const LOBBY_ID = /^[A-Za-z0-9_-]{1,64}$/;

export type Validated = { ok: true; args: Record<string, unknown> } | { ok: false; error: KartNetError };

const bad = (message: string): Validated => ({ ok: false, error: kartNetError('bad_request', message) });

function isPlainObject(v: unknown): v is Record<string, unknown> {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) return false;
  const proto = Object.getPrototypeOf(v);
  return proto === Object.prototype || proto === null;
}

function str(v: unknown, field: string, maxBytes: number, minBytes = 1): string | KartNetError {
  if (typeof v !== 'string') return kartNetError('bad_request', `${field} must be a string`);
  const n = Buffer.byteLength(v, 'utf-8');
  if (n < minBytes || n > maxBytes) {
    return kartNetError('bad_request', `${field} must be ${minBytes}..${maxBytes} bytes`);
  }
  return v;
}

function u8(v: unknown, field: string): number | KartNetError {
  if (typeof v !== 'number' || !Number.isInteger(v) || v < 0 || v > 255) {
    return kartNetError('bad_request', `${field} must be an integer 0..255`);
  }
  return v;
}

const isErr = (v: unknown): v is KartNetError =>
  typeof v === 'object' && v !== null && 'code' in v && 'message' in v;

/** Checks `obj` has only `allowed` keys and all of `required`. */
function closed(obj: unknown, what: string, required: string[], optional: string[]): KartNetError | Record<string, unknown> {
  if (!isPlainObject(obj)) return kartNetError('bad_request', `${what} must be an object`);
  const allowed = new Set([...required, ...optional]);
  for (const k of Object.keys(obj)) {
    if (!allowed.has(k)) return kartNetError('bad_request', `${what}: unknown field ${JSON.stringify(k.slice(0, 32))}`);
  }
  for (const k of required) {
    if (!(k in obj)) return kartNetError('bad_request', `${what}: missing ${k}`);
  }
  return obj;
}

function lobbyId(v: unknown): string | KartNetError {
  if (typeof v !== 'string' || !LOBBY_ID.test(v)) {
    return kartNetError('bad_request', 'lobby_id must be 1..64 characters of [A-Za-z0-9_-]');
  }
  return v;
}

function inviteCode(v: unknown): string | null | KartNetError {
  if (v === undefined || v === null) return null;
  return str(v, 'invite_code', MAX_INVITE_CODE_LEN);
}

/** Turns the renderer's arguments into the sidecar's `args`, or refuses them. */
export function validate(command: CommandName, input: unknown[]): Validated {
  if (input.length > 1) return bad(`${command} takes at most one argument`);
  const [a] = input;
  switch (command) {
    case 'session_status':
    case 'session_stop':
    case 'session_ready':
    case 'net_init':
      if (a !== undefined) return bad(`${command} takes no arguments`);
      return { ok: true, args: {} };
    case 'lobby_list': {
      const limit = a === undefined ? DEFAULT_LIST_LIMIT : a;
      if (typeof limit !== 'number' || !Number.isInteger(limit) || limit < 1 || limit > MAX_LIST_LIMIT) {
        return bad(`limit must be an integer 1..${MAX_LIST_LIMIT}`);
      }
      return { ok: true, args: { limit } };
    }
    case 'session_start':
    case 'race_start':
    case 'race_get':
    case 'lobby_get':
    case 'lobby_leave': {
      const id = lobbyId(a);
      return isErr(id) ? { ok: false, error: id } : { ok: true, args: { lobby_id: id } };
    }
    case 'lobby_create': {
      const o = closed(a, 'request', ['name', 'capacity', 'track_id', 'class', 'laps', 'character_id'], ['invite_code', 'relay_urls']);
      if (isErr(o)) return { ok: false, error: o };
      const fields = {
        name: str(o.name, 'name', MAX_LOBBY_NAME_BYTES),
        capacity: u8(o.capacity, 'capacity'),
        track_id: str(o.track_id, 'track_id', MAX_TRACK_ID_LEN),
        class: str(o.class, 'class', MAX_CLASS_LEN),
        laps: u8(o.laps, 'laps'),
        character_id: str(o.character_id, 'character_id', MAX_CHARACTER_ID_LEN),
        invite_code: inviteCode(o.invite_code),
      };
      for (const v of Object.values(fields)) if (isErr(v)) return { ok: false, error: v };
      const relays = o.relay_urls === undefined ? [] : o.relay_urls;
      if (!Array.isArray(relays) || relays.length > MAX_RELAY_URLS) {
        return bad(`relay_urls must be an array of at most ${MAX_RELAY_URLS}`);
      }
      const relay_urls: string[] = [];
      for (const r of relays) {
        const s = str(r, 'relay_urls[]', MAX_RELAY_URL_LEN);
        if (isErr(s)) return { ok: false, error: s };
        relay_urls.push(s);
      }
      return { ok: true, args: { ...fields, relay_urls } };
    }
    case 'lobby_join': {
      const o = closed(a, 'request', ['lobby_id', 'character_id'], ['invite_code']);
      if (isErr(o)) return { ok: false, error: o };
      const fields = {
        lobby_id: lobbyId(o.lobby_id),
        character_id: str(o.character_id, 'character_id', MAX_CHARACTER_ID_LEN),
        invite_code: inviteCode(o.invite_code),
      };
      for (const v of Object.values(fields)) if (isErr(v)) return { ok: false, error: v };
      return { ok: true, args: fields };
    }
  }
}
