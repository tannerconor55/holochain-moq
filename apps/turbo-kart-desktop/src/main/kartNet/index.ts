// Electron side of kart-net: eleven fixed control channels, lobby events,
// one bounded binary diagnostic channel, and the sidecar lifecycle.
// The renderer never chooses what runs: the binary path, its arguments and
// the conductor credentials are all decided here.

import { app, BrowserWindow, ipcMain, IpcMainInvokeEvent, MessageChannelMain, MessagePortMain } from 'electron';
import fs from 'fs';
import os from 'os';
import path from 'path';
import { AdminWebsocket } from '@holochain/client';
import { SIDECAR_BINARY } from '../const';
import { isAppUrl } from '../security';
import { SidecarHost } from './host';
import { CommandName, isKartNetError, KartNetError, kartNetError } from './protocol';
import { provisionSidecar } from './provision';
import { validate } from './validate';
import { body, HEADER, MAX_PENDING } from './binary';
import { logKartNet } from '../logs';
import { REALTIME_AVAILABLE, unsupportedCommand } from './platform';

/** Renderer → main channels (the preload uses exactly these). */
export const KART_NET_CHANNELS: Record<string, CommandName> = {
  'kart-net:net-init': 'net_init',
  'kart-net:lobby-create': 'lobby_create',
  'kart-net:lobby-list': 'lobby_list',
  'kart-net:lobby-get': 'lobby_get',
  'kart-net:lobby-join': 'lobby_join',
  'kart-net:lobby-leave': 'lobby_leave',
  'kart-net:race-start': 'race_start',
  'kart-net:race-get': 'race_get',
  'kart-net:session-start': 'session_start',
  'kart-net:session-status': 'session_status',
  'kart-net:session-stop': 'session_stop',
  'kart-net:session-ready': 'session_ready',
};
/** Main → renderer: kart-net's lobby events, nothing else. */
export const KART_NET_EVENT_CHANNEL = 'kart-net:lobby-event';

export type Reply = { ok: true; value: unknown } | { ok: false; error: KartNetError };

let HOST: SidecarHost | undefined;
let attachBinary: () => void = () => undefined;
let gameWindow: () => BrowserWindow | null | undefined = () => undefined;

/**
 * The per-user runtime directory for the socket: `$XDG_RUNTIME_DIR` if it is
 * ours and private, otherwise a 0700 directory in the app's data.
 */
function runtimeDir(): string {
  const xdg = process.env.XDG_RUNTIME_DIR;
  if (xdg && process.platform !== 'win32') {
    try {
      const st = fs.statSync(xdg);
      if (st.isDirectory() && st.uid === os.userInfo().uid && (st.mode & 0o077) === 0) return xdg;
    } catch {
      /* fall through */
    }
  }
  return path.join(app.getPath('userData'), 'run');
}

/** Only the game window's top frame, on the app origin, may call. */
function fromGame(e: Pick<IpcMainInvokeEvent, 'sender' | 'senderFrame'>): boolean {
  const win = gameWindow();
  return (
    !!win &&
    !win.isDestroyed() &&
    e.sender === win.webContents &&
    e.senderFrame !== null &&
    e.senderFrame === e.sender.mainFrame &&
    isAppUrl(e.senderFrame.url)
  );
}

/** Registers the IPC handlers once, at startup (before any window). */
export function registerKartNetIpc(getGameWindow: () => BrowserWindow | null | undefined): void {
  gameWindow = getGameWindow;
  let port: MessagePortMain | undefined;
  let deliveryId=1;
  let binaryGeneration=0,binaryRace='';
  let preparingSession=false;
  const pendingDeliveries=new Set<number>();
  const queued:Buffer[]=[];
  const close=()=> { const old=port;port=undefined;binaryGeneration=0;binaryRace='';preparingSession=false;HOST?.retireBinaryRequests();old?.close();pendingDeliveries.clear();queued.length=0;HOST?.resumeBinary(); if (old && HOST?.status==='ready') void HOST.request('session_stop',{}).catch(()=>undefined); };
  const flush=()=> {
    while (port && queued.length && pendingDeliveries.size < MAX_PENDING) {
      const b=queued.shift()!;
      if(b[4]===3 && (b.readUInt32BE(12)!==binaryGeneration || b.toString('ascii',24,56)!==binaryRace)) continue;
      if (b[4]===3) { const id=deliveryId++;b.writeUInt32BE(id,8);pendingDeliveries.add(id); }
      const bytes=Uint8Array.from(b);port.postMessage(bytes.buffer);
    }
    if (queued.length || pendingDeliveries.size >= MAX_PENDING) HOST?.pauseBinary(); else HOST?.resumeBinary();
  };
  ipcMain.on('kart-net:binary-open',(e,...input:unknown[])=> {
    if (!fromGame(e) || input.length || !HOST || HOST.status !== 'ready') return;
    close();
    const channel=new MessageChannelMain();port=channel.port1;
    port.on('message',(event)=> {
      try {
        const b=body(event.data);
        if (b[4]===4) {
          if (b.length!==HEADER || !pendingDeliveries.delete(b.readUInt32BE(8))) throw new Error('invalid delivery credit');
          flush();
        } else if (b[4]===1 || b[4]===5) { HOST!.sendBinary(b); }
        else throw new Error('fixed binary operation refused');
      } catch (error) { console.error('[kart-net] fixed binary channel refused:', error instanceof Error ? error.message : 'invalid frame'); close(); }
    });
    const current=port;port.on('close',()=> { if (port===current) close(); });port.start();
    e.sender.postMessage('kart-net:binary-port',null,[channel.port2]);
  });
  app.on('web-contents-created',(_e,contents)=>contents.on('did-start-navigation',()=> { if (contents===gameWindow()?.webContents) close(); }));
  app.once('before-quit',close);
  attachBinary=()=> {
    HOST!.on('binary',(b:Buffer)=> {
      if (!port) return;
      if(b[4]===3 && !preparingSession && (b.readUInt32BE(12)!==binaryGeneration || b.toString('ascii',24,56)!==binaryRace)) return;
      // Coalesce not-yet-transferred Latest state for this authenticated
      // member/lane. Reliable data and acknowledgments never get replaced.
      const prior=b[4]===3 && b[5]!==2 ? queued.findIndex(q=>q[4]===3 && q[5]===b[5] && q[6]===b[6] && q.readUInt32BE(12)===b.readUInt32BE(12)) : -1;
      if(prior>=0) queued[prior]=Buffer.from(b);
      else {
        if (queued.length >= 64) { close();void HOST!.request('session_stop',{}).catch(()=>undefined);return; }
        queued.push(Buffer.from(b));
      }
      if (!preparingSession) flush();
    });
    HOST!.on('binary-closed',close);
  };


  for (const [channel, command] of Object.entries(KART_NET_CHANNELS)) {
    ipcMain.handle(channel, async (e, ...input: unknown[]): Promise<Reply> => {
      if (!fromGame(e)) return { ok: false, error: kartNetError('forbidden', 'not the game window') };
      const unsupported = unsupportedCommand(command);
      if (unsupported) return { ok: false, error: unsupported };
      if (command==='session_start' && !port) return {ok:false,error:kartNetError('not_initialised','open the fixed test binary channel before session start')};
      const v = validate(command, input);
      if (!v.ok) return v;
      if (!HOST) return { ok: false, error: kartNetError('sidecar_unavailable', 'kart-net has not started') };
      if(command==='session_start' && preparingSession) return {ok:false,error:kartNetError('busy','session preparation already in progress')};
      if(command==='session_start') preparingSession=true;
      try {
        const value=command==='session_start' ? await HOST.startSession(v.args as {lobby_id:string}) : await HOST.request(command,v.args);
        if(command==='session_start') {
          const status=value as {generation:number;race_id:string};
          binaryGeneration=status.generation;binaryRace=status.race_id;preparingSession=false;flush();
        } else if(command==='session_stop') close();
        return { ok: true, value };
      } catch (err) {
        if(command==='session_start') {preparingSession=false;queued.length=0;}
        return {
          ok: false,
          error: isKartNetError(err) ? err : kartNetError('internal', String(err)),
        };
      }
    });
  }
}

/**
 * Starts the sidecar once Kangaroo's conductor runs and both hApps are
 * installed. Idempotent: one sidecar per app run.
 */
export function startKartNet(admin: AdminWebsocket, binaryArgs?: string[]): Promise<void> {
  if (!HOST) {
    if (binaryArgs) logKartNet(`[kart-net] Internet mode: ${binaryArgs.join(' ')}`);
    HOST = new SidecarHost({
      binary: SIDECAR_BINARY,
      binaryArgs,
      binaryEnabled: REALTIME_AVAILABLE,
      runtimeDir: runtimeDir(),
      provision: (signingKey) => provisionSidecar(admin, signingKey, path.join(app.getPath('userData'), 'kart-endpoint.key')),
      log: (line) => logKartNet(line),
    });
    attachBinary();
    // Forwarded to the current game window only; no listener is kept per window.
    HOST.on('lobby-event', (event) => {
      const win = gameWindow();
      if (win && !win.isDestroyed()) win.webContents.send(KART_NET_EVENT_CHANNEL, event);
    });
  }
  return HOST.start();
}

export async function stopKartNet(): Promise<void> {
  await HOST?.stop();
}

/** For diagnostics and the e2e test (main process only). */
export function kartNetHost(): SidecarHost | undefined {
  return HOST;
}
