/* eslint-disable import-x/no-named-as-default-member */
import { app } from 'electron';
import path from 'path';
import fs from 'fs';
import yaml from 'js-yaml';
import { KangarooConfig } from './types';

const RESOURCES_DIRECTORY = app.isPackaged
  ? path.join(app.getAppPath(), '../app.asar.unpacked/resources')
  : path.join(app.getAppPath(), './resources');

const kangarooConfigString = fs.readFileSync(
  path.join(RESOURCES_DIRECTORY, 'kangaroo.config.json'),
  'utf-8'
);
export const KANGAROO_CONFIG: KangarooConfig = JSON.parse(kangarooConfigString);
const conductorConfigTemplateString = fs.readFileSync(
  path.join(RESOURCES_DIRECTORY, 'conductor-config.yaml'),
  'utf-8'
);
export const CONDUCTOR_CONFIG_TEMPLATE = yaml.load(conductorConfigTemplateString);

/**
 * E6.5a.1-R2: upper bound for Kitsune2's bootstrap poll/push backoff
 * (`network.advanced.coreBootstrap.backoffMaxMs`; Kitsune2 0.5.0 default:
 * doubling from 5 s to 5 min, never reset).
 *
 * A relaunched conductor gets a new random iroh transport URL, and Holochain
 * 0.7.0's connection preflight announces it for only one of a conductor's
 * spaces (holochain_p2p `BootWrap::put` overwrites one shared preflight per
 * space). A peer drops every message from a URL it has no agent info for in
 * that space, so in the other space it learns the new URL only from its next
 * bootstrap poll - minutes apart once it has run a while. Capping the backoff
 * bounds that window (measured: <= ~8 s) at the cost of one bootstrap poll per
 * space every 10 s. Agent infos stay signed and verified; nothing is trusted
 * more. See E65A1-RECONNECT.md.
 */
export const BOOTSTRAP_BACKOFF_MAX_MS = 10_000;

const binariesAppendix = KANGAROO_CONFIG.appId.slice(0, 10).replace(' ', '-');

const BINARIES_DIRECTORY = path.join(RESOURCES_DIRECTORY, 'bins');

export const HOLOCHAIN_BINARY = path.join(
  BINARIES_DIRECTORY,
  `holochain-v${KANGAROO_CONFIG.bins.holochainVersion}-${binariesAppendix}${
    process.platform === 'win32' ? '.exe' : ''
  }`
);

export const LAIR_BINARY = path.join(
  BINARIES_DIRECTORY,
  `lair-keystore-${binariesAppendix}${
    process.platform === 'win32' ? '.exe' : ''
  }`
);

// Two hApps, one agent (WP10 E2): the shared platform hApp and the game's
// hApp are separate cells; neither bundles the other's DNA. The game hApp is
// the one whose UI this window shows.
/** The kart-net sidecar (WP10 E3), built by scripts/build-sidecar.mjs. */
export const SIDECAR_BINARY = path.join(
  BINARIES_DIRECTORY,
  `kart-net-sidecar${process.platform === 'win32' ? '.exe' : ''}`
);

export const HAPP_PATH = path.join(RESOURCES_DIRECTORY, 'kangaroo.happ');
export const HAPP_APP_ID = 'turbo_kart';
export const PLATFORM_HAPP_PATH = path.join(RESOURCES_DIRECTORY, 'gaming_platform.happ');
export const PLATFORM_APP_ID = 'gaming_platform';
/** `{ "<role>": "<DNA hash>" }`, written by `scripts/pack-happs.mjs`. */
export const EXPECTED_DNA_HASHES_PATH = path.join(RESOURCES_DIRECTORY, 'expected-dna-hashes.json');

export const UI_DIRECTORY = path.join(RESOURCES_DIRECTORY, 'ui');

/**
 * E7.WINDOWS.1: the licence set built for this package's platform
 * (resources/licenses; also installed as <app>/licenses). On Windows the
 * game's About screen reads `licenses/*` from here, because the webhapp's own
 * copy is the verified Linux build's (D2: the webhapp is never rebuilt).
 */
export const LICENSES_DIRECTORY = path.join(RESOURCES_DIRECTORY, 'licenses');

export const ICON_PATH = path.join(RESOURCES_DIRECTORY, 'ui', 'icon.png');

export const SYSTRAY_ICON_PATH = path.join(RESOURCES_DIRECTORY, 'icons', '32x32@2x.png');
export const NOTIFICATIONS_ICON_PATH = path.join(RESOURCES_DIRECTORY, 'icons', '128x128.png');

export const isMac = process.platform === 'darwin';
export const isWindows = process.platform === 'win32';
export const isLinux = process.platform === 'linux';
