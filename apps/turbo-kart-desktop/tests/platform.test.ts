// E7.WINDOWS.1: a desktop without the realtime channel refuses, in main, every
// command that would make it a race participant; reads stay available.
//   node --import tsx --test tests/*.test.ts          (yarn test:unit)
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { COMMANDS } from '../src/main/kartNet/protocol';
import { NO_REALTIME_MESSAGE, REALTIME_AVAILABLE, unsupportedCommand } from '../src/main/kartNet/platform';

test('without realtime: lobby create/join and race start/entry are refused with an accurate message', () => {
  const refused = COMMANDS.filter((c) => unsupportedCommand(c, false));
  assert.deepEqual(refused, ['lobby_create', 'lobby_join', 'race_start', 'session_start', 'session_ready']);
  const e = unsupportedCommand('lobby_join', false)!;
  assert.equal(e.code, 'unsupported_platform');
  assert.equal(e.message, NO_REALTIME_MESSAGE);
  assert.match(e.message, /Single Player works/);
  // Reading, leaving and stopping stay possible (a player is never stuck in a lobby).
  for (const c of ['net_init', 'lobby_list', 'lobby_get', 'lobby_leave', 'race_get', 'session_status', 'session_stop'] as const) {
    assert.equal(unsupportedCommand(c, false), null, c);
  }
});

test('with realtime (Linux, macOS): nothing is refused, so Unix behaviour is unchanged', () => {
  for (const c of COMMANDS) assert.equal(unsupportedCommand(c, true), null, c);
  assert.equal(REALTIME_AVAILABLE, process.platform !== 'win32');
});
