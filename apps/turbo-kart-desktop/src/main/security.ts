// Turbo Kart desktop: renderer hardening, in one place (WP10 E2).
//
// The game renderer is untrusted UI: no Node, no Holochain credentials, no
// shell, filesystem or IPC beyond what a preload explicitly exposes (nothing
// yet; E3 adds `window.kartNet`). Holochain stays in the main process.
import { app, session, WebPreferences } from 'electron';
import { createHash } from 'crypto';

/** The game's origin (Kangaroo's `webhapp://` protocol). */
export const APP_ORIGIN = 'webhapp://webhappwindow';

/**
 * DevTools are off unless explicitly requested in an unpackaged development
 * run (`TURBO_KART_DEVTOOLS=1`). A packaged build never enables them.
 */
export const DEVTOOLS_ENABLED = !app.isPackaged && process.env.TURBO_KART_DEVTOOLS === '1';

/** Every window's renderer settings (stated explicitly, not left to defaults). */
export const SECURE_WEB_PREFERENCES: WebPreferences = {
  contextIsolation: true,
  nodeIntegration: false,
  nodeIntegrationInWorker: false,
  nodeIntegrationInSubFrames: false,
  sandbox: true,
  webSecurity: true,
  allowRunningInsecureContent: false,
  webviewTag: false,
  devTools: DEVTOOLS_ENABLED,
};

/**
 * The game page's Content-Security-Policy. Scripts: the app itself (E6.5a:
 * the game's pinned Three.js is vendored into the package, so no CDN), and the exact inline
 * scripts of `html` by hash (the game's import map); never `'unsafe-inline'`
 * or `'unsafe-eval'` for scripts. Styles allow inline (the game builds UI
 * with inline styles) and Google Fonts. connect-src: the app's own files
 * (three.js GLTFLoader fetches the kart .glb models) and blob: (the textures
 * embedded in a .glb are decoded by fetching blob: URLs the page itself made);
 * no network origin.
 */
export function contentSecurityPolicy(html: string): string {
  const inlineHashes = [...html.matchAll(/<script(?![^>]*\bsrc=)[^>]*>([\s\S]*?)<\/script>/gi)].map(
    (m) => `'sha256-${createHash('sha256').update(m[1], 'utf8').digest('base64')}'`
  );
  return [
    "default-src 'self'",
    `script-src 'self' ${inlineHashes.join(' ')}`.trim(),
    "style-src 'self' 'unsafe-inline' https://fonts.googleapis.com",
    "font-src 'self' https://fonts.gstatic.com",
    "img-src 'self' data: blob:",
    "media-src 'self' data: blob:",
    "connect-src 'self' blob:",
    "worker-src 'none'",
    "object-src 'none'",
    "frame-src 'none'",
    "base-uri 'none'",
    "form-action 'none'",
    "frame-ancestors 'none'",
  ].join('; ');
}

/** Whether `url` is inside the game origin. */
export function isAppUrl(url: string): boolean {
  return url === APP_ORIGIN || url.startsWith(`${APP_ORIGIN}/`);
}

/**
 * App-wide guards for every renderer: no navigation away from the game
 * origin, no popups or new windows, no `<webview>`, and no browser
 * permissions (the game requests none).
 */
export function hardenAllWebContents(): void {
  app.on('web-contents-created', (_e, contents) => {
    contents.on('will-navigate', (e) => {
      if (!isAppUrl(e.url)) e.preventDefault();
    });
    contents.on('will-frame-navigate', (e) => {
      if (!isAppUrl(e.url)) e.preventDefault();
    });
    contents.on('will-redirect', (e) => {
      if (!isAppUrl(e.url)) e.preventDefault();
    });
    contents.on('will-attach-webview', (e) => e.preventDefault());
    contents.setWindowOpenHandler(() => ({ action: 'deny' }));
  });
  app.whenReady().then(() => {
    session.defaultSession.setPermissionRequestHandler((_wc, _permission, callback) =>
      callback(false)
    );
    session.defaultSession.setPermissionCheckHandler(() => false);
  });
}
