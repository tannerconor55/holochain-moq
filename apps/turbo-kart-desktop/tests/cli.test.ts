// E6.INTERNET: the opt-in --game-relay-url launch option and what main passes
// to the sidecar. Absent, everything must be exactly the v0.1 LAN behaviour.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { sidecarNetArgs, validateArgs, validateGameRelayUrl } from '../src/main/cli';

test('no --game-relay-url is LAN mode: no extra sidecar arguments', () => {
  const o = validateArgs({ profile: 'demo' });
  assert.equal(o.gameRelayUrl, undefined);
  assert.equal(sidecarNetArgs(o), undefined);
});

test('a clean https relay selects Internet mode for the sidecar', () => {
  const url = 'https://dev-test-bootstrap2.holochain.org/';
  const o = validateArgs({ profile: 'inet', gameRelayUrl: url });
  assert.equal(o.gameRelayUrl, url);
  assert.deepEqual(sidecarNetArgs(o), ['--relay-url', url]);
  assert.equal(validateGameRelayUrl('https://relay.example:8443/relay'), 'https://relay.example:8443/relay');
});

test('insecure or malformed relay URLs are refused before anything starts', () => {
  for (const bad of [
    'http://relay.example/',
    'ws://relay.example/',
    'ftp://relay.example/',
    'relay.example',
    'https://',
    'https:///path',
    'https://user:pw@relay.example/',
    'https://relay.example/?token=1',
    'https://relay.example/#x',
    'https://relay .example/',
    'https://relay.example/\n',
    '',
    `https://${'r'.repeat(256)}.example/`,
  ]) {
    assert.throws(() => validateGameRelayUrl(bad), /--game-relay-url/, JSON.stringify(bad));
    assert.throws(() => validateArgs({ gameRelayUrl: bad }), /--game-relay-url/, JSON.stringify(bad));
  }
  assert.throws(() => validateGameRelayUrl(42), /must be a string/);
});
