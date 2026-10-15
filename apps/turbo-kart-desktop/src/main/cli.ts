// No network seed option (WP10 E2): a seed would change the DNA hashes away
// from the production networks.

export interface CliOpts {
  profile?: string;
  holochainPath?: string;
  lairPath?: string;
  holochainRustLog?: string;
  holochainWasmLog?: string;
  lairRustLog?: string;
  bootstrapUrl?: string;
  relayUrl?: string;
  gameRelayUrl?: string;
  printHolochainLogs?: boolean;
}

export interface RunOptions {
  profile: string | undefined;
  bootstrapUrl: URL | undefined;
  relayUrl: URL | undefined;
  /** E6.INTERNET: the game mesh's https relay; undefined = LAN mode (v0.1). */
  gameRelayUrl: string | undefined;
  holochainPath: string | undefined;
  lairPath: string | undefined;
  holochainRustLog: string | undefined;
  holochainWasmLog: string | undefined;
  lairRustLog: string | undefined;
  printHolochainLogs: boolean;
}

/** Same limit as the presence/lobby relay URL (`MAX_RELAY_URL_LEN`). */
export const MAX_GAME_RELAY_URL_LEN = 256;

/**
 * E6.INTERNET: `https://host[:port][/path]` only - no plaintext relay, no
 * credentials, query or fragment. The sidecar validates it again.
 */
export function validateGameRelayUrl(value: unknown): string {
  const fail = (why: string): never => {
    throw new Error(`The --game-relay-url argument ${why}.`);
  };
  if (typeof value !== 'string') return fail('must be a string');
  if (value.length === 0 || value.length > MAX_GAME_RELAY_URL_LEN) {
    return fail(`must be 1..${MAX_GAME_RELAY_URL_LEN} characters`);
  }
  if (!/^[\x21-\x7e]+$/.test(value)) return fail('must be printable ASCII without spaces');
  if (!/^https:\/\/[^/]/.test(value)) return fail('must be an https://<host> URL (no plaintext relay)');
  if (value.includes('?') || value.includes('#') || value.includes('@')) {
    return fail('must not contain credentials, a query or a fragment');
  }
  let url: URL;
  try {
    url = new URL(value);
  } catch {
    return fail('is not a valid URL');
  }
  if (url.protocol !== 'https:' || !url.hostname) return fail('must be an https://<host> URL');
  return value;
}

/** Extra sidecar arguments: none in LAN mode (exactly v0.1), else the relay. */
export function sidecarNetArgs(o: Pick<RunOptions, 'gameRelayUrl'>): string[] | undefined {
  return o.gameRelayUrl === undefined ? undefined : ['--relay-url', o.gameRelayUrl];
}

export function validateArgs(args: CliOpts): RunOptions {
  // validate --profile argument
  const allowedProfilePattern = /^[0-9a-zA-Z-]+$/;
  if (args.profile && !allowedProfilePattern.test(args.profile)) {
    throw new Error(
      `The --profile argument may only contain digits (0-9), letters (a-z,A-Z) and dashes (-) but got '${args.profile}'`
    );
  }
  if (args.bootstrapUrl && typeof args.bootstrapUrl !== 'string') {
    throw new Error('The --bootstrap-url argument must be of type string.');
  }
  if (args.relayUrl && typeof args.relayUrl !== 'string') {
    throw new Error('The --relay-url argument must be of type string.');
  }
  const gameRelayUrl =
    args.gameRelayUrl === undefined ? undefined : validateGameRelayUrl(args.gameRelayUrl);
  if (args.holochainPath && typeof args.holochainPath !== 'string') {
    throw new Error('The --holochain-path argument must be of type string.');
  }
  if (args.lairPath && typeof args.lairPath !== 'string') {
    throw new Error('The --lair-path argument must be of type string.');
  }
  if (args.holochainRustLog && typeof args.holochainRustLog !== 'string') {
    throw new Error('The --holochain-rust-log argument must be of type string.');
  }
  if (args.holochainWasmLog && typeof args.holochainWasmLog !== 'string') {
    throw new Error('The --holochain-wasm-log argument must be of type string.');
  }
  if (args.lairRustLog && typeof args.lairRustLog !== 'string') {
    throw new Error('The --lair-rust-log argument must be of type string.');
  }

  const profile = args.profile ? args.profile : undefined;

  const bootstrapUrl = args.bootstrapUrl ? new URL(args.bootstrapUrl) : undefined;
  const relayUrl = args.relayUrl ? new URL(args.relayUrl) : undefined;

  return {
    profile,
    bootstrapUrl,
    relayUrl,
    gameRelayUrl,
    holochainPath: args.holochainPath ? args.holochainPath : undefined,
    lairPath: args.lairPath ? args.lairPath : undefined,
    holochainRustLog: args.holochainRustLog ? args.holochainRustLog : undefined,
    holochainWasmLog: args.holochainWasmLog ? args.holochainWasmLog : undefined,
    lairRustLog: args.lairRustLog ? args.lairRustLog : undefined,
    printHolochainLogs: !!args.printHolochainLogs,
  };
}
