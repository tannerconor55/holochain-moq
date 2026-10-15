// Electron main's argument validation for the renderer's kart-net calls.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { validate } from '../src/main/kartNet/validate';

const create = {
  name: 'Cup',
  capacity: 4,
  track_id: 'palm-cove',
  class: '100cc',
  laps: 3,
  character_id: 'rex',
};

const code = (r: ReturnType<typeof validate>) => (r.ok ? 'ok' : r.error.code);

test('well-formed calls pass and are normalised', () => {
  assert.deepEqual(validate('net_init', []), { ok: true, args: {} });
  assert.deepEqual(validate('lobby_list', []), { ok: true, args: { limit: 20 } });
  assert.deepEqual(validate('lobby_list', [50]), { ok: true, args: { limit: 50 } });
  const c = validate('lobby_create', [create]);
  assert.ok(c.ok);
  assert.deepEqual(c.args, { ...create, invite_code: null, relay_urls: [] });
  const j = validate('lobby_join', [{ lobby_id: 'uhCkkAbc_-1', character_id: 'zippy', invite_code: 'secret1' }]);
  assert.ok(j.ok);
  assert.deepEqual(validate('lobby_get', ['uhCkkX']), { ok: true, args: { lobby_id: 'uhCkkX' } });
  // Meaning is the DNA's: an unknown character passes main and is refused by kart-net.
  assert.equal(code(validate('lobby_create', [{ ...create, character_id: 'wario' }])), 'ok');
});

test('wrong shapes, types and sizes are refused as bad_request', () => {
  const bad: [Parameters<typeof validate>[0], unknown[]][] = [
    ['net_init', [{ admin_port: 1 }]],
    ['net_init', [1, 2]],
    ['lobby_list', [0]],
    ['lobby_list', [51]],
    ['lobby_list', [2.5]],
    ['lobby_list', ['10']],
    ['lobby_get', ['']],
    ['lobby_get', ['../../etc/passwd']],
    ['lobby_get', ['a'.repeat(65)]],
    ['lobby_get', [{ lobby_id: 'x' }]],
    ['lobby_leave', [null]],
    ['lobby_create', [null]],
    ['lobby_create', [[create]]],
    ['lobby_create', [{ ...create, extra: 1 }]],
    ['lobby_create', [{ ...create, __proto__: { polluted: 1 } }]],
    ['lobby_create', [{ ...create, capacity: 256 }]],
    ['lobby_create', [{ ...create, capacity: -1 }]],
    ['lobby_create', [{ ...create, laps: '3' }]],
    ['lobby_create', [{ ...create, name: '' }]],
    ['lobby_create', [{ ...create, name: 'x'.repeat(129) }]],
    ['lobby_create', [{ ...create, name: '🏁'.repeat(33) }]],
    ['lobby_create', [{ ...create, invite_code: 'x'.repeat(65) }]],
    ['lobby_create', [{ ...create, relay_urls: ['a', 'b', 'c', 'd', 'e'] }]],
    ['lobby_create', [{ ...create, relay_urls: [1] }]],
    ['lobby_create', [{ name: 'only' }]],
    ['lobby_join', [{ lobby_id: 'x' }]],
    ['lobby_join', [{ lobby_id: 'x', character_id: 'rex', role: 'admin' }]],
  ];
  for (const [cmd, args] of bad) {
    assert.equal(code(validate(cmd, args)), 'bad_request', `${cmd} ${JSON.stringify(args)}`);
  }
  // A class instance is not a plain object.
  class Req {
    name = 'x';
  }
  assert.equal(code(validate('lobby_create', [new Req()])), 'bad_request');
});


test('race operations accept only a lobby ID, never caller-selected admission or endpoint', () => {
  for (const command of ['race_start', 'race_get'] as const) {
    assert.deepEqual(validate(command, ['uhCkkX']), { ok: true, args: { lobby_id: 'uhCkkX' } });
    for (const input of [[], [{ lobby_id: 'x', participants: [] }], ['x', 'extra'], ['../file']]) {
      assert.equal(code(validate(command, input)), 'bad_request');
    }
  }
});

test('session controls cannot select a roster, endpoint, peer address or track', () => {
  for (const command of ['session_start'] as const) {
    assert.equal(validate(command, ['uhCkk-valid']).ok, true);
    assert.equal(validate(command, [{ lobby_id: 'uhCkk-valid', roster: [] }]).ok, false);
    assert.equal(validate(command, ['uhCkk-valid', { endpoint: '127.0.0.1:1' }]).ok, false);
  }
  for (const command of ['session_status', 'session_stop'] as const) {
    assert.equal(validate(command, []).ok, true);
    assert.equal(validate(command, [{ track: 'arbitrary' }]).ok, false);
  }
});
