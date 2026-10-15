import path from 'path';
import fs from 'fs';
import url from 'url';
import { InstalledAppId } from '@holochain/client';
import { BrowserWindow, nativeImage, net, session } from 'electron';
import { is } from '@electron-toolkit/utils';
import { ICON_PATH, KANGAROO_CONFIG, LICENSES_DIRECTORY } from './const';
import {
  APP_ORIGIN,
  contentSecurityPolicy,
  DEVTOOLS_ENABLED,
  SECURE_WEB_PREFERENCES,
} from './security';
import { SplashScreenType } from './types';
import { DiagLimiter, diagLine } from './kartNet/diag';
import { logKartNet } from './logs';

export type UISource = {
  type: 'path';
  path: string;
};

/**
 * Serves the game's static files under `webhapp://webhappwindow/`, confined
 * to `uiDir`. `index.html` is served unchanged (no injected launcher env, no
 * app token: the game makes no Holochain calls) with a Content-Security-Policy
 * header.
 */
function serveUi(uiDir: string): void {
  const root = path.resolve(uiDir);
  session.defaultSession.protocol.handle('webhapp', async (request) => {
    const { host, pathname } = new URL(request.url);
    if (host !== 'webhappwindow') return new Response(null, { status: 404 });
    let relative: string;
    try {
      relative = decodeURIComponent(pathname).replace(/^\/+/, '') || 'index.html';
    } catch {
      return new Response(null, { status: 400 });
    }
    const absolute = path.resolve(root, relative);
    if (absolute !== root && !absolute.startsWith(root + path.sep)) {
      return new Response(null, { status: 403 });
    }
    const platformLicence = licenceOverride(relative);
    if (platformLicence) return net.fetch(url.pathToFileURL(platformLicence).toString());
    if (relative !== 'index.html') {
      return net.fetch(url.pathToFileURL(absolute).toString());
    }
    const html = await fs.promises.readFile(absolute, 'utf-8');
    return new Response(html, {
      headers: {
        'content-type': 'text/html; charset=utf-8',
        'content-security-policy': contentSecurityPolicy(html),
      },
    });
  });
}

/**
 * E7.WINDOWS.1: on Windows, `licenses/<file>` comes from this package's own
 * licence set when it has that file (plain file names only); otherwise, and on
 * every other platform, from the game's files as before.
 */
export function licenceOverride(relative: string, platform: NodeJS.Platform = process.platform, dir = LICENSES_DIRECTORY): string | null {
  if (platform !== 'win32') return null;
  const m = /^licenses\/([A-Za-z0-9][A-Za-z0-9._-]*)$/.exec(relative);
  if (!m) return null;
  const file = path.join(dir, m[1]);
  return fs.existsSync(file) && fs.statSync(file).isFile() ? file : null;
}

export const createHappWindow = async (
  uiSource: UISource,
  appId: InstalledAppId
): Promise<BrowserWindow> => {
  serveUi(uiSource.path);

  const icon = fs.existsSync(ICON_PATH) ? nativeImage.createFromPath(ICON_PATH) : undefined;

  console.log(`Instantiating browser window for ${appId}`);

  const happWindow = new BrowserWindow({
    width: 1200,
    height: 800,
    show: false,
    icon,
    title: KANGAROO_CONFIG.productName,
    webPreferences: {
      ...SECURE_WEB_PREFERENCES,
      preload: path.resolve(__dirname, '../preload/happ.js'),
      // E6.5a.1: a multiplayer host keeps simulating (AI, authoritative progress)
      // while its window is covered, hidden or minimized; Chromium would otherwise
      // throttle the game's simulation timer in the background. A scheduling
      // setting only: sandbox, isolation and the renderer's API are unchanged.
      backgroundThrottling: false,
    },
  });

  if (!DEVTOOLS_ENABLED) {
    // Belt and braces: devTools is already false in webPreferences.
    happWindow.webContents.on('devtools-opened', () => happWindow.webContents.closeDevTools());
  }

  // E6.INTERNET.2: the game's periodic `[tk-diag]` lines go to the durable
  // profile log (filtered, truncated, rate limited; see kartNet/diag.ts).
  const diagLimiter = new DiagLimiter();
  happWindow.webContents.on('console-message', (details) => {
    const line = diagLine(details.message);
    if (line != null && diagLimiter.take()) logKartNet(line);
  });

  try {
    await happWindow.loadURL(`${APP_ORIGIN}/index.html`);
  } catch (e) {
    console.error('[ERROR] Failed to load index.html', e);
    await happWindow.loadFile(path.join(__dirname, '../renderer/indexNotFound2.html'));
  }

  happWindow.show();

  return happWindow;
};

export const createSplashWindow = (type: SplashScreenType): BrowserWindow => {
  let htmlFile: string;
  switch (type) {
    case SplashScreenType.EnterPassword: {
      htmlFile = 'enterPassword.html';
      break;
    }
    case SplashScreenType.LoadingOnly: {
      htmlFile = 'loading.html';
      break;
    }
    case SplashScreenType.PasswordSetup: {
      htmlFile = 'setupPassword.html';
      break;
    }
    case SplashScreenType.PasswordSetupOtional: {
      htmlFile = 'setupPasswordOptional.html';
      break;
    }
  }

  const splashWindow = new BrowserWindow({
    height: 450,
    width: 800,
    center: true,
    resizable: false,
    frame: false,
    show: false,
    backgroundColor: '#fbf9f7',
    webPreferences: {
      ...SECURE_WEB_PREFERENCES,
      preload: path.join(__dirname, '../preload/splashscreen.js'),
    },
  });

  if (is.dev && process.env['ELECTRON_RENDERER_URL']) {
    splashWindow.loadURL(`${process.env['ELECTRON_RENDERER_URL']}/${htmlFile}`);
  } else {
    splashWindow.loadFile(path.join(__dirname, `../renderer/${htmlFile}`));
  }

  splashWindow.once('ready-to-show', () => {
    splashWindow.webContents.send('name-and-version', {
      productName: KANGAROO_CONFIG.productName,
      version: KANGAROO_CONFIG.version,
    });
    splashWindow.show();
  });

  return splashWindow;
};
