/**
 * E6.INTERNET.2: the game's periodic diagnostics reach the durable profile log
 * through the game window's console, so the renderer gets no new API. Only
 * lines the game marks with DIAG_PREFIX are kept; each is stripped of control
 * characters, truncated, and rate limited, so a misbehaving page cannot flood
 * the log. Diagnostics carry timings and counters only, never credentials.
 */
export const DIAG_PREFIX = '[tk-diag] ';
export const DIAG_MAX_CHARS = 1000;
/** Token bucket: a burst of DIAG_BURST lines, refilled at DIAG_RATE_PER_S. */
export const DIAG_BURST = 8;
export const DIAG_RATE_PER_S = 1;

/** The log line for a console message, or null if it is not a game diagnostic. */
export function diagLine(message: unknown): string | null {
  if (typeof message !== 'string' || !message.startsWith(DIAG_PREFIX)) return null;
  // eslint-disable-next-line no-control-regex
  const clean = message.replace(/[\x00-\x1f\x7f]/g, ' ');
  return clean.length > DIAG_MAX_CHARS ? `${clean.slice(0, DIAG_MAX_CHARS)}…` : clean;
}

/** Bounded acceptance of diagnostic lines; `dropped` counts refused ones. */
export class DiagLimiter {
  private tokens = DIAG_BURST;
  private last: number;
  dropped = 0;
  constructor(private readonly now: () => number = () => Date.now()) {
    this.last = this.now();
  }
  take(): boolean {
    const t = this.now();
    this.tokens = Math.min(DIAG_BURST, this.tokens + ((t - this.last) / 1000) * DIAG_RATE_PER_S);
    this.last = t;
    if (this.tokens >= 1) {
      this.tokens -= 1;
      return true;
    }
    this.dropped++;
    return false;
  }
}
