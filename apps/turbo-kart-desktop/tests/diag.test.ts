// E6.INTERNET.2: the game's `[tk-diag]` console lines reach the durable log
// only filtered, truncated and rate limited (src/main/kartNet/diag.ts).
import test from 'node:test';
import assert from 'node:assert/strict';
import { DiagLimiter, diagLine, DIAG_BURST, DIAG_MAX_CHARS, DIAG_PREFIX } from '../src/main/kartNet/diag';

test('only game diagnostics are kept, without control characters, truncated', () => {
  assert.equal(diagLine('hello'), null);
  assert.equal(diagLine(42), null);
  assert.equal(diagLine(`x${DIAG_PREFIX}y`), null, 'the prefix must lead');
  assert.equal(diagLine(`${DIAG_PREFIX}a\nb\x1b[31mc`), `${DIAG_PREFIX}a b [31mc`);
  const long = diagLine(DIAG_PREFIX + 'z'.repeat(5000))!;
  assert.equal(long.length, DIAG_MAX_CHARS + 1);
  assert.ok(long.endsWith('…'));
});

test('a burst is accepted, a flood is dropped and counted, the rate refills', () => {
  let now = 0;
  const l = new DiagLimiter(() => now);
  for (let i = 0; i < DIAG_BURST; i++) assert.equal(l.take(), true);
  assert.equal(l.take(), false);
  assert.equal(l.dropped, 1);
  now += 1000;
  assert.equal(l.take(), true, 'one line per second after the burst');
  assert.equal(l.take(), false);
});
