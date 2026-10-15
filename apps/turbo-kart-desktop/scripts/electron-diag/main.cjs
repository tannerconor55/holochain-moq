// Minimal Electron app for scripts/electron-startup-diag.mjs (E6.5a.1b).
// No Holochain, Lair, sidecar, MoQ/iroh or game code. The window uses the
// same security settings as the game window: sandbox, contextIsolation, no
// nodeIntegration. Each startup stage prints one `DIAG {json}` line on stdout.
const path = require('node:path');
const { app, BrowserWindow, ipcMain } = require('electron');

const t0 = Number(process.env.DIAG_T0) || Date.now();
const cp = (name, extra = {}) => process.stdout.write(`DIAG ${JSON.stringify({ cp: name, t: Date.now() - t0, ...extra })}\n`);

cp('main-js', { pid: process.pid, electron: process.versions.electron, chrome: process.versions.chrome, node: process.versions.node });
app.setPath('userData', process.env.DIAG_PROFILE);
app.on('child-process-gone', (_e, d) => cp('child-process-gone', { type: d.type, reason: d.reason, exitCode: d.exitCode }));
app.on('render-process-gone', (_e, _w, d) => cp('render-process-gone', { reason: d.reason, exitCode: d.exitCode }));
app.on('gpu-info-update', () => cp('gpu-info-update'));

// The launcher writes "quit" on stdin once it has snapshotted the healthy tree.
const quitFallback = setTimeout(() => app.quit(), Number(process.env.DIAG_QUIT_AFTER_MS) || 15000);
process.stdin.on('data', (b) => { if (String(b).includes('quit')) { clearTimeout(quitFallback); cp('quit-requested'); app.quit(); } });

app.whenReady().then(() => {
  cp('ready');
  const win = new BrowserWindow({
    width: 320, height: 200, show: process.env.DIAG_SHOW !== '0',
    webPreferences: { sandbox: true, contextIsolation: true, nodeIntegration: false, preload: path.join(__dirname, 'preload.cjs') },
  });
  cp('window-created', { webContentsPid: null });
  win.webContents.once('did-finish-load', () => cp('did-finish-load', { rendererPid: win.webContents.getOSProcessId() }));
  ipcMain.once('diag:renderer', (e, info) => {
    if (e.sender !== win.webContents) return;
    const metrics = app.getAppMetrics().map((m) => ({ pid: m.pid, type: m.type }));
    cp('renderer-ipc', { renderer: info, gpu: app.getGPUFeatureStatus(), metrics });
  });
  win.loadFile(path.join(__dirname, 'index.html'));
});
app.on('will-quit', () => cp('will-quit'));
app.on('window-all-closed', () => {});
