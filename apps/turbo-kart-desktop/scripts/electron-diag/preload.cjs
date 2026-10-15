// Sandboxed preload: reports the renderer's actual isolation state to main.
const { ipcRenderer } = require('electron');

window.addEventListener('DOMContentLoaded', () => {
  ipcRenderer.send('diag:renderer', {
    sandboxed: process.sandboxed === true,
    contextIsolated: process.contextIsolated === true,
    nodeIntegration: typeof require === 'function' && typeof process.binding === 'function',
    pid: process.pid,
  });
});
