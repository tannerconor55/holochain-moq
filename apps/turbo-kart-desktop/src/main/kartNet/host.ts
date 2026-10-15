// SidecarHost: Electron main's side of the kart-net sidecar (one per app run).
// Spawns it, authenticates it over a private socket, provisions its conductor
// credentials, forwards allowlisted requests with ids and timeouts, and
// restarts it after a crash. No Electron imports (unit-tested in plain Node).

import { ChildProcess, spawn } from 'child_process';
import { EventEmitter } from 'events';
import crypto from 'crypto';
import fs from 'fs';
import net from 'net';
import { Duplex } from 'stream';
import { BinaryDecoder, body, frame, MAX_PENDING, MAX_BODY } from './binary';
import path from 'path';
import {
  CommandName,
  encodeFrame,
  FrameDecoder,
  isKartNetError,
  KartNetError,
  kartNetError,
  PROTOCOL_VERSION,
} from './protocol';

/** What main sends as `provision`, and how to withdraw what it granted. */
export interface Provisioned {
  message: Record<string, unknown>;
  revoke: () => Promise<void>;
}

export interface SidecarHostOptions {
  /** Fixed by main (const.ts); never from the renderer. */
  binary: string;
  /** Arguments before `--endpoint` (tests only: a script for `node`). */
  binaryArgs?: string[];
  /** Directory that holds the per-run private socket directory (0700). */
  runtimeDir: string;
  /** Grants credentials for the sidecar's signing key (`uhCAk…`). */
  provision: (signingKey: string) => Promise<Provisioned>;
  log?: (line: string) => void;
  requestTimeoutMs?: number;
  startupTimeoutMs?: number;
  maxPending?: number;
  /** Restarts allowed per minute after crashes. */
  maxRestarts?: number;
  /** Backoff schedule for `isDepMissingCommit` retries (tests shorten it). */
  depMissingRetryMs?: number[];
  /** Backoff schedule for `startSession`'s status-lag retries (tests shorten it). */
  statusLagRetryMs?: number[];
  binaryEnabled?: boolean;
  /**
   * The local channel to the sidecar. `unix` (default except on Windows): a
   * Unix socket in a private 0700 directory, removed after one connection.
   * `pipe` (default on Windows, E7.WINDOWS.1): a named pipe with a fresh
   * random name; the first connection that proves the session key is used,
   * any other is closed (see `acceptProven`).
   */
  channel?: 'unix' | 'pipe';
  /** Where pipe names live (tests only: a directory on a non-Windows host). */
  pipeNamespace?: string;
}

type State = 'idle' | 'starting' | 'ready' | 'failed' | 'stopping' | 'stopped';

interface Pending {
  resolve: (v: unknown) => void;
  reject: (e: KartNetError) => void;
  timer: NodeJS.Timeout;
}

const SIGNING_KEY = /^uhCAk[A-Za-z0-9_-]{48}$/;
/** Control-plane writes: logged on start and end (E6.5a.1 recovery forensics). */
const LOGGED_WRITES = new Set<CommandName>(['net_init', 'lobby_create', 'lobby_join', 'lobby_leave', 'race_start']);
/** Error messages are logged truncated; they never contain arguments or credentials. */
const MAX_LOGGED_MESSAGE = 400;
/** Lobby writes whose commit can depend on other agents' records (E6.5a.1b). */
const DEPENDENT_WRITES = new Set<CommandName>(['lobby_join', 'lobby_leave', 'race_start']);
/** Backoff before each retry of a commit Holochain refused as not yet possible. */
export const DEP_MISSING_RETRY_MS = [250, 500, 1000, 2000, 4000];
/** Backoff before each retry of a `session_start` whose lobby status has not propagated yet. */
export const STATUS_LAG_RETRY_MS = [500, 1000, 2000, 4000];
/** `session_admission`'s refusal when the lobby's current status is not Starting/Racing for the locked race. */
const NOT_CURRENT = 'race is finished, abandoned or no longer current';

/**
 * Holochain refused the commit itself, before anything was written, because a
 * record its validation needs has not reached this conductor's DHT yet
 * (`DepMissingFromDht`). Holochain states the commit "could not be completed
 * but may be retried": retrying is not a replay of a write that may have
 * happened. Any other failure (timeouts included) is never retried here.
 */
export function isDepMissingCommit(e: unknown): boolean {
  return (
    isKartNetError(e) &&
    e.code === 'control_plane' &&
    e.message.includes('The commit could not be completed but may be retried') &&
    e.message.includes('DepMissingFromDht')
  );
}

export class SidecarHost extends EventEmitter {
  private readonly o: Required<Omit<SidecarHostOptions, 'binaryArgs' | 'log' | 'pipeNamespace'>> &
    Pick<SidecarHostOptions, 'pipeNamespace'> &
    Pick<SidecarHostOptions, 'binaryArgs' | 'log'>;
  private state: State = 'idle';
  private child?: ChildProcess;
  private socket?: net.Socket;
  private startup?: Promise<void>;
  private revoke?: () => Promise<void>;
  private pending = new Map<number, Pending>();
  private nextId = 1;
  private restarts: number[] = [];
  private restartTimer?: NodeJS.Timeout;
  /** The child whose loss is being handled (socket close and exit both report it). */
  private recovering?: ChildProcess;
  /** Releases requests waiting for a scheduled restart. */
  private releaseRestart?: () => void;
  /** `net_init` succeeded once: a restarted sidecar is re-initialised. */
  private initialised = false;
  private binary?: Duplex;
  private binaryPending = new Map<number, number>();
  private binaryId = 1;
  /** Sidecar generation: 1 for the first process, +1 per replacement (diagnostics only). */
  private generation = 0;

  sendBinary(value: unknown): void {
    const b=Buffer.from(body(value));
    if ((b[4] !== 1 && b[4] !== 5) || !this.binary || this.state !== 'ready') throw new Error('binary session unavailable');
    if (this.binaryPending.size >= MAX_PENDING || this.binary.writableLength > MAX_PENDING*MAX_BODY) throw new Error('binary credit exhausted');
    const id=this.binaryId++;
    if (id > 0xffffffff) throw new Error('binary request IDs exhausted');
    this.binaryPending.set(id,b.readUInt32BE(8));b.writeUInt32BE(id,8);
    this.binary.write(frame(b));
  }
  retireBinaryRequests(): void { this.binaryPending.clear(); }
  pauseBinary(): void { this.binary?.pause(); }
  resumeBinary(): void { this.binary?.resume(); }


  constructor(options: SidecarHostOptions) {
    super();
    this.o = {
      requestTimeoutMs: 30_000,
      startupTimeoutMs: 20_000,
      maxPending: 32,
      maxRestarts: 5,
      depMissingRetryMs: DEP_MISSING_RETRY_MS,
      statusLagRetryMs: STATUS_LAG_RETRY_MS,
      binaryEnabled: false,
      channel: process.platform === 'win32' ? 'pipe' : 'unix',
      ...options,
    };
  }

  get status(): State {
    return this.state;
  }

  /** The running sidecar's pid (tests and diagnostics). */
  get pid(): number | undefined {
    return this.child?.pid;
  }

  private log(line: string) {
    this.o.log?.(`[kart-net] ${line}`);
  }

  /** Idempotent: one sidecar at a time. */
  start(): Promise<void> {
    if (this.state === 'ready') return Promise.resolve();
    if (this.state === 'starting' && this.startup) return this.startup;
    if (this.state === 'stopping' || this.state === 'stopped') {
      return Promise.reject(kartNetError('sidecar_unavailable', 'kart-net is shutting down'));
    }
    this.state = 'starting';
    this.startup = this.launch().then(
      () => {
        this.state = 'ready';
        this.emit('status', 'ready');
      },
      async (e) => {
        this.log(`startup failed: ${describe(e)}`);
        this.teardown();
        await this.revokeGrant();
        if (this.state === 'starting') this.state = 'failed';
        this.emit('status', 'failed');
        throw kartNetError('sidecar_unavailable', `kart-net did not start: ${describe(e)}`);
      }
    );
    return this.startup;
  }

  /**
   * One allowlisted command (arguments already validated by the caller).
   * Failures, and every control-plane write, are logged with the sidecar
   * generation and pid that served them; arguments are never logged.
   */
  async request(command: CommandName, args: Record<string, unknown>): Promise<unknown> {
    const t0 = Date.now();
    const write = LOGGED_WRITES.has(command);
    if (write) this.log(`request ${command} start (state ${this.state}, gen ${this.generation}, pid ${this.child?.pid ?? '-'})`);
    for (let attempt = 0; ; attempt++) {
      try {
        const value = await this.send(command, args);
        if (write) this.log(`request ${command} ok in ${Date.now() - t0} ms${attempt ? ` after ${attempt} retries` : ''} (gen ${this.generation}, pid ${this.child?.pid ?? '-'})`);
        return value;
      } catch (e) {
        const err = isKartNetError(e) ? e : kartNetError('internal', describe(e));
        this.log(`request ${command} failed in ${Date.now() - t0} ms (state ${this.state}, gen ${this.generation}, pid ${this.child?.pid ?? '-'}): ${err.code}: ${String(err.message).slice(0, MAX_LOGGED_MESSAGE)}`);
        if (!DEPENDENT_WRITES.has(command) || !isDepMissingCommit(err)) throw e;
        const delay = this.o.depMissingRetryMs[attempt];
        if (delay === undefined || this.state === 'stopping' || this.state === 'stopped') {
          // Nothing was written; the record has just not propagated yet. Not a lost connection.
          throw kartNetError('not_propagated', `${command} could not be committed yet: lobby data has not reached this desktop (nothing was written); check again`);
        }
        this.log(`request ${command}: commit not possible yet (DepMissingFromDht, nothing written); retry ${attempt + 1}/${this.o.depMissingRetryMs.length} in ${delay} ms`);
        await new Promise((r) => setTimeout(r, delay));
      }
    }
  }

  private async send(command: CommandName, args: Record<string, unknown>): Promise<unknown> {
    // A failed start is retried by the next call, within the restart budget.
    if (this.state === 'failed' && this.takeRestart()) {
      await this.start().catch(() => undefined);
    }
    if (this.state === 'starting' && this.startup) {
      await this.startup.catch(() => undefined);
    }
    if (this.state !== 'ready' || !this.socket) {
      throw kartNetError('sidecar_unavailable', `kart-net is not running (${this.state})`);
    }
    if (this.pending.size >= this.o.maxPending) {
      throw kartNetError('busy', `more than ${this.o.maxPending} kart-net requests pending`);
    }
    const id = this.nextId++;
    const result = await new Promise<unknown>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(kartNetError('timeout', `${command} did not answer within ${this.o.requestTimeoutMs} ms`));
      }, this.o.requestTimeoutMs);
      this.pending.set(id, { resolve, reject, timer });
      try {
        this.socket!.write(encodeFrame({ type: 'request', id, command, args }));
      } catch (e) {
        clearTimeout(timer);
        this.pending.delete(id);
        reject(kartNetError('bad_request', describe(e)));
      }
    });
    if (command === 'net_init') this.initialised = true;
    return result;
  }

  /**
   * E6.5a.1-R2: `session_start` with one precisely classified retry case.
   *
   * `session_start` only reads Holochain before it creates the session in one
   * step without awaiting (sidecar `slot.rs`), and returns the existing session
   * for the same lobby, so repeating it never duplicates a session. Its
   * admission check refuses with NOT_CURRENT both when the race is really over
   * and when the lobby's Starting status has not reached this desktop yet
   * (kart-net reports a not-yet-visible status as `open`). Only the latter is
   * retried: a locked race record for this lobby exists AND the status still
   * reads `open`. Both reads are read-only. Everything else is returned as is.
   */
  async startSession(args: { lobby_id: string }): Promise<unknown> {
    for (let attempt = 0; ; attempt++) {
      try {
        return await this.request('session_start', args);
      } catch (e) {
        if (!isKartNetError(e) || e.code !== 'race_not_ready' || !e.message.includes(NOT_CURRENT)) throw e;
        const [lobby, race] = await Promise.all([
          this.request('lobby_get', { lobby_id: args.lobby_id }).catch(() => null) as Promise<{ status?: string; race_id?: string | null } | null>,
          this.request('race_get', { lobby_id: args.lobby_id }).catch(() => null) as Promise<{ race?: { setup?: { lobby?: string; race_id?: string } } | null } | null>,
        ]);
        const locked = race?.race?.setup;
        const lagging = !!locked?.race_id && locked.lobby === args.lobby_id && lobby?.status === 'open';
        this.log(`session_start refused (${NOT_CURRENT}); lobby status ${lobby?.status ?? 'unread'}, locked race ${locked?.race_id ?? 'none'}: ${lagging ? 'status not propagated yet' : 'not retried'}`);
        if (!lagging) throw e;
        const delay = this.o.statusLagRetryMs[attempt];
        if (delay === undefined || this.state === 'stopping' || this.state === 'stopped') {
          throw kartNetError('not_propagated', 'the race has started, but its lobby status has not reached this desktop yet (no session was created); try entering again');
        }
        this.log(`session_start: retry ${attempt + 1}/${this.o.statusLagRetryMs.length} in ${delay} ms`);
        await new Promise((r) => setTimeout(r, delay));
      }
    }
  }

  /** Normal shutdown: `shutdown`, then SIGKILL after `graceMs`. */
  async stop(graceMs = 2000): Promise<void> {
    if (this.state === 'stopped') return;
    this.state = 'stopping';
    clearTimeout(this.restartTimer);
    this.releaseRestart?.();
    this.failPending('sidecar_unavailable', 'kart-net is shutting down');
    const child = this.child;
    if (child && child.exitCode === null && child.signalCode === null) {
      const exited = new Promise<void>((r) => child.once('exit', () => r()));
      try {
        this.socket?.write(encodeFrame({ type: 'shutdown' }));
      } catch {
        /* socket already gone */
      }
      const timer = setTimeout(() => child.kill('SIGKILL'), graceMs);
      await exited;
      clearTimeout(timer);
    }
    this.teardown();
    await this.revokeGrant();
    this.state = 'stopped';
    this.emit('status', 'stopped');
  }

  // ------------------------------------------------------------------ internals

  private async launch(): Promise<void> {
    const usePipe = this.o.channel === 'pipe';
    const dir = usePipe ? undefined : privateDir(this.o.runtimeDir);
    const endpoint = dir ? path.join(dir, 'kart-net.sock') : windowsPipeName(this.o.pipeNamespace);
    const server = net.createServer();
    const cleanupEndpoint = () => {
      server.close();
      if (dir) fs.rmSync(dir, { recursive: true, force: true });
    };
    try {
      await new Promise<void>((resolve, reject) => {
        server.once('error', reject);
        server.listen(endpoint, () => resolve());
      });
      if (!usePipe) fs.chmodSync(endpoint, 0o600);
      const key = crypto.randomBytes(32);

      const child = spawn(this.o.binary, [...(this.o.binaryArgs ?? []), '--endpoint', endpoint], {
        env: {},
        stdio: this.o.binaryEnabled ? ['pipe', 'ignore', 'pipe', 'pipe'] : ['pipe', 'ignore', 'pipe'],
      });
      this.child = child;
      if (this.o.binaryEnabled) {
        const pipe=child.stdio[3] as Duplex;
        this.binary=pipe;
        const decoder=new BinaryDecoder();
        pipe.on('data',(chunk:Buffer)=> {
          try {
            for (const b of decoder.push(chunk)) {
              if (b[4] === 2) {
                const id=b.readUInt32BE(8);const original=this.binaryPending.get(id);
                if (original === undefined) { if(id < this.binaryId) continue;throw new Error('unexpected binary acknowledgment'); }
                this.binaryPending.delete(id);b.writeUInt32BE(original,8);
              } else if (b[4] !== 3) throw new Error('unexpected binary message');
              this.emit('binary',b);
            }
          } catch { pipe.destroy();child.kill('SIGKILL'); }
        });
        pipe.on('error',()=>undefined);
        pipe.once('close',()=> { if(this.binary===pipe) {this.binary=undefined;this.binaryPending.clear();this.emit('binary-closed');} });
      }
      child.stdin!.end(`${key.toString('hex')}\n`);
      child.stderr!.setEncoding('utf-8');
      child.stderr!.on('data', (d: string) => d.split('\n').filter(Boolean).forEach((l) => this.log(l)));
      const exited = new Promise<never>((_, reject) => {
        child.once('exit', (code, sig) => reject(new Error(`sidecar exited (${code ?? sig}) during startup`)));
        // e.g. the binary is missing: no 'exit' follows.
        child.once('error', (e) => reject(new Error(`cannot start the sidecar: ${e.message}`)));
      });
      exited.catch(() => undefined);
      child.once('exit', (code, signal) => this.onExit(child, code, signal));

      const deadline = timeout(this.o.startupTimeoutMs, 'sidecar startup');
      const socket = await Promise.race([
        usePipe
          ? acceptProven(server, key, Math.min(PIPE_HELLO_MS, this.o.startupTimeoutMs), (l) => this.log(l))
          : new Promise<net.Socket>((resolve) => server.once('connection', resolve)),
        exited,
        deadline,
      ]);
      // One connection, then nothing else can connect: close and remove the endpoint.
      cleanupEndpoint();

      const decoder = new FrameDecoder();
      const inbox: unknown[] = [];
      let wake: (() => void) | undefined;
      socket.on('data', (chunk: Buffer) => {
        try {
          for (const m of decoder.push(chunk)) {
            if (this.socket === socket) this.onMessage(m);
            else {
              inbox.push(m);
              wake?.();
            }
          }
        } catch (e) {
          this.log(`protocol error from sidecar: ${describe(e)}; closing`);
          socket.destroy();
        }
      });
      socket.on('error', () => undefined);
      // `acceptProven` hands over a paused socket with the hello put back.
      socket.resume();
      const next = async (): Promise<Record<string, unknown>> => {
        while (inbox.length === 0) {
          await Promise.race([new Promise<void>((r) => (wake = r)), exited, deadline]);
        }
        const m = inbox.shift();
        if (typeof m !== 'object' || m === null) throw new Error('sidecar sent a non-object');
        return m as Record<string, unknown>;
      };

      const hello = await next();
      const theirs = typeof hello.session_key === 'string' ? Buffer.from(hello.session_key, 'hex') : Buffer.alloc(0);
      if (
        hello.type !== 'hello' ||
        hello.protocol !== PROTOCOL_VERSION ||
        theirs.length !== key.length ||
        !crypto.timingSafeEqual(theirs, key) ||
        typeof hello.signing_key !== 'string' ||
        !SIGNING_KEY.test(hello.signing_key)
      ) {
        throw new Error('the sidecar failed the hello check');
      }

      const granted = await this.o.provision(hello.signing_key);
      this.revoke = granted.revoke;
      socket.write(encodeFrame(this.o.binaryEnabled ? {...granted.message,binary_enabled:true} : granted.message));
      const ready = await next();
      if (ready.type !== 'ready') {
        const err = isKartNetError(ready.error) ? `${ready.error.code}: ${ready.error.message}` : String(ready.type);
        throw new Error(`sidecar did not become ready (${err})`);
      }
      this.socket = socket;
      for (const m of inbox.splice(0)) this.onMessage(m);
      socket.on('close', () => {
        if (this.socket === socket) this.recover(child, 'connection closed');
      });

      // A restarted sidecar resumes the player's existing session.
      if (this.initialised) {
        await new Promise<void>((resolve, reject) => {
          const id = this.nextId++;
          const timer = setTimeout(() => reject(new Error('re-init timed out')), this.o.requestTimeoutMs);
          this.pending.set(id, { resolve: () => resolve(), reject: (e) => reject(new Error(e.message)), timer });
          socket.write(encodeFrame({ type: 'request', id, command: 'net_init', args: {} }));
        });
      }
      this.generation++;
      this.log(`sidecar ready (pid ${child.pid}, gen ${this.generation}${this.initialised ? ', net_init resumed' : ''})`);
    } catch (e) {
      cleanupEndpoint();
      throw e;
    }
  }

  private onMessage(m: unknown) {
    if (typeof m !== 'object' || m === null) return;
    const msg = m as Record<string, unknown>;
    if (msg.type === 'response') {
      const id = typeof msg.id === 'number' ? msg.id : undefined;
      const p = id === undefined ? undefined : this.pending.get(id);
      if (!p) {
        if (isKartNetError(msg.error)) this.log(`sidecar error without a pending request: ${msg.error.code}`);
        return;
      }
      this.pending.delete(id!);
      clearTimeout(p.timer);
      if ('result' in msg) p.resolve(msg.result);
      else p.reject(isKartNetError(msg.error) ? msg.error : kartNetError('internal', 'malformed response'));
    } else if (msg.type === 'event') {
      this.emit('lobby-event', msg.event);
    }
  }

  private failPending(code: KartNetError['code'], message: string) {
    for (const [id, p] of this.pending) {
      clearTimeout(p.timer);
      this.pending.delete(id);
      p.reject(kartNetError(code, message));
    }
  }

  private onExit(child: ChildProcess, code: number | null, signal: NodeJS.Signals | null) {
    this.recover(child, `${code ?? signal}`);
    if (this.child === child) this.child = undefined;
  }

  /**
   * The sidecar is gone (its socket closed or it exited; whichever comes
   * first). Fails pending requests at once, then - if it had been ready -
   * restarts it once the old process has exited (never two at a time).
   */
  private recover(child: ChildProcess, reason: string) {
    if (this.recovering === child) return;
    this.recovering = child;
    this.socket?.destroy();
    this.socket = undefined;
    this.binary?.destroy();this.binary=undefined;this.binaryPending.clear();
    this.emit("binary-closed");
    this.failPending('sidecar_unavailable', `kart-net stopped unexpectedly (${reason}); retry`);
    if (this.state !== 'ready') return; // stopping, or a startup failure start() reports
    this.log(`sidecar lost (${reason}; gen ${this.generation}, pid ${child.pid})`);
    void this.revokeGrant();
    const delay = 500 * 2 ** this.recentRestarts();
    if (!this.takeRestart()) {
      this.state = 'failed';
      this.emit('status', 'failed');
      this.log('restart budget exhausted; kart-net stays unavailable');
      if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
      return;
    }
    this.state = 'starting';
    this.emit('status', 'restarting');
    this.log(`restarting in ${delay} ms (${this.recentRestarts()} restarts in the last minute)`);
    // Requests arriving meanwhile wait for this restart.
    this.startup = new Promise<void>((r) => (this.releaseRestart = r));
    const restart = () => {
      this.restartTimer = setTimeout(() => {
        if (this.state !== 'starting') return;
        const release = this.releaseRestart;
        this.state = 'idle';
        this.start()
          .catch(() => undefined)
          .finally(() => release?.());
      }, delay);
    };
    if (child.exitCode !== null || child.signalCode !== null) restart();
    else {
      child.once('exit', restart);
      child.kill('SIGKILL');
    }
  }

  private recentRestarts(): number {
    const now = Date.now();
    this.restarts = this.restarts.filter((t) => now - t < 60_000);
    return this.restarts.length;
  }

  /** Uses one restart from the per-minute budget, if any is left. */
  private takeRestart(): boolean {
    if (this.recentRestarts() >= this.o.maxRestarts) return false;
    this.restarts.push(Date.now());
    return true;
  }

  private teardown() {
    this.binary?.destroy();this.binary=undefined;this.binaryPending.clear();
    this.emit("binary-closed");
    this.socket?.destroy();
    this.socket = undefined;
    const child = this.child;
    if (child && child.exitCode === null && child.signalCode === null) child.kill('SIGKILL');
  }

  private async revokeGrant() {
    const revoke = this.revoke;
    this.revoke = undefined;
    if (!revoke) return;
    try {
      await Promise.race([revoke(), timeout(2000, 'revoke')]);
    } catch (e) {
      this.log(`revoking the sidecar's capability failed: ${describe(e)}`);
    }
  }
}

/** How long one pipe connection may take to send its hello (E7.WINDOWS.1). */
export const PIPE_HELLO_MS = 5_000;
/** Connections a pipe accepts before giving up on a start (squatters included). */
export const MAX_PIPE_CANDIDATES = 8;
/** A hello is small; anything larger is refused before it is buffered. */
const MAX_HELLO_BYTES = 1024;

/**
 * `\\.\pipe\turbo-kart-<128-bit random hex>`: fresh per start, never reused,
 * the only shape the sidecar opens (`transport::is_pipe_name`).
 *
 * Node creates the pipe (libuv `CreateNamedPipeW`) with
 * `FILE_FLAG_FIRST_PIPE_INSTANCE`, so a name someone else already holds makes
 * `listen` fail instead of being shared, but with the default security
 * descriptor (write access: this user, SYSTEM, Administrators; read access:
 * everyone) and without `PIPE_REJECT_REMOTE_CLIENTS`. Node cannot narrow it.
 * Nothing secret is sent before a connection proves the session key, and a
 * connection that cannot write cannot prove it (see `acceptProven`).
 */
export function windowsPipeName(namespace = '\\\\.\\pipe\\'): string {
  return `${namespace}turbo-kart-${crypto.randomBytes(16).toString('hex')}`;
}

/**
 * The first pipe connection whose hello carries the session key (pipe channel
 * only). Each connection gets `helloMs` to send it; one that sends anything
 * else, too much, or nothing in time is closed and does not consume the
 * start. Connections arriving after the proven one are closed. At most
 * `MAX_PIPE_CANDIDATES` are tried. The proven socket is returned paused with
 * its bytes put back, so the normal hello check reads them again.
 */
function acceptProven(server: net.Server, key: Buffer, helloMs: number, log: (l: string) => void): Promise<net.Socket> {
  return new Promise((resolve, reject) => {
    let candidates = 0;
    let chosen: net.Socket | undefined;
    const pending = new Set<net.Socket>();
    server.on('connection', (s: net.Socket) => {
      s.on('error', () => undefined);
      if (chosen) return void s.destroy();
      if (++candidates > MAX_PIPE_CANDIDATES) {
        s.destroy();
        return reject(new Error(`more than ${MAX_PIPE_CANDIDATES} pipe connections without the session key`));
      }
      pending.add(s);
      let buf = Buffer.alloc(0);
      const refuse = (why: string) => {
        clearTimeout(timer);
        s.off('data', onData);
        pending.delete(s);
        s.destroy();
        log(`refused a pipe connection (${why})`);
      };
      const timer = setTimeout(() => refuse('no hello in time'), helloMs);
      const onData = (chunk: Buffer) => {
        buf = Buffer.concat([buf, chunk]);
        if (buf.length < 4) return;
        const len = buf.readUInt32BE(0);
        if (len > MAX_HELLO_BYTES) return refuse('oversized hello');
        if (buf.length < 4 + len) return;
        let hello: { type?: unknown; session_key?: unknown } | null;
        try {
          hello = JSON.parse(buf.subarray(4, 4 + len).toString('utf-8'));
        } catch {
          return refuse('malformed hello');
        }
        const hex = typeof hello?.session_key === 'string' && /^[0-9a-fA-F]{64}$/.test(hello.session_key) ? hello.session_key : '';
        const theirs = Buffer.from(hex, 'hex');
        if (hello?.type !== 'hello' || theirs.length !== key.length || !crypto.timingSafeEqual(theirs, key)) {
          return refuse('session key not proven');
        }
        clearTimeout(timer);
        s.off('data', onData);
        pending.delete(s);
        chosen = s;
        for (const other of pending) other.destroy();
        pending.clear();
        s.pause();
        s.unshift(buf);
        resolve(s);
      };
      s.on('data', onData);
    });
  });
}

/** A fresh 0700 directory for one run's socket (random name, not shared). */
function privateDir(base: string): string {
  fs.mkdirSync(base, { recursive: true, mode: 0o700 });
  const dir = fs.mkdtempSync(path.join(base, 'tk-'));
  fs.chmodSync(dir, 0o700);
  return dir;
}

function timeout(ms: number, what: string): Promise<never> {
  return new Promise((_, reject) => setTimeout(() => reject(new Error(`${what} timed out`)), ms).unref());
}

function describe(e: unknown): string {
  if (isKartNetError(e)) return `${e.code}: ${e.message}`;
  return e instanceof Error ? e.message : String(e);
}
