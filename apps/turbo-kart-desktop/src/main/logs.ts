import winston, { createLogger, transports, format } from 'winston';
import path from 'path';
import fs from 'fs';
import { KangarooFileSystem } from './filesystem';
import {
  HOLOCHAIN_ERROR,
  HOLOCHAIN_LOG,
  HolochainData,
  HolochainVersion,
  LAIR_ERROR,
  LAIR_LOG,
  WASM_LOG,
  KANGAROO_ERROR,
  KANGAROO_LOG,
  KangarooEmitter,
} from './eventEmitter';
import { KANGAROO_CONFIG } from './const';

const { combine, timestamp } = format;

const HOLOCHAIN_LOGGERS: Record<HolochainVersion, winston.Logger> = {};

// TODO define class LauncherLogger that can log all lair, holochain and launcher-specific stuff
// with methods logLair, logHolochain, logLauncher, logHapp, ...

export function setupLogs(
  kangarooEmitter: KangarooEmitter,
  kangarooFs: KangarooFileSystem,
  holochainLogsToTerminal: boolean
) {
  const logFilePath = path.join(
    kangarooFs.profileLogsDir,
    `${KANGAROO_CONFIG.productName.replace(' ', '_')}.log`
  );
  if (fs.existsSync(logFilePath)) {
    const stats = fs.statSync(logFilePath);
    // If existing logfile is larger than 1GB, delete it
    if (stats.size > 1e9) {
      console.log("Found a log file that's larger than 1GB. Deleting it.");
      fs.rmSync(logFilePath);
    }
  }

  // Use log file rotation with max size of a single file of 50MB and max 5 total files
  const logFileTransport = new transports.File({
    filename: logFilePath,
    maxsize: 50_000_000,
    maxFiles: 5,
  });

  const lairLogger = createLairLogger(logFileTransport);
  // E6.INTERNET.2: kart-net (sidecar and game diagnostics) lines go to the
  // durable profile log too, not only to the terminal.
  KART_NET_LOGGER = createLabelledLogger('KART-NET', logFileTransport);
  // E7.WINDOWS.1: startup diagnostics (platform, binaries, readiness, failures).
  STARTUP_LOGGER = createLabelledLogger('STARTUP', logFileTransport);

  kangarooEmitter.on(LAIR_LOG, (log) => {
    const logLine = `[LAIR] ${log}`;
    console.log(logLine);
    lairLogger.log('info', logLine);
  });
  kangarooEmitter.on(LAIR_ERROR, (log) => {
    const logLine = `[LAIR] ERROR: ${log}`;
    console.error(logLine);
    lairLogger.log('info', logLine);
  });
  kangarooEmitter.on(KANGAROO_ERROR, (log) => {
    const logLine = `[${KANGAROO_CONFIG.productName}] ERROR: ${log}`;
    console.error(logLine);
    lairLogger.log('info', logLine);
  });
  kangarooEmitter.on(KANGAROO_LOG, (log) => {
    const logLine = `[${KANGAROO_CONFIG.productName}] ${log}`;
    console.log(logLine);
    lairLogger.log('info', logLine);
  });
  kangarooEmitter.on(HOLOCHAIN_LOG, (holochainData) => {
    logHolochain(holochainData as HolochainData, logFileTransport, holochainLogsToTerminal);
  });
  kangarooEmitter.on(HOLOCHAIN_ERROR, (holochainData) => {
    logHolochain(holochainData as HolochainData, logFileTransport, holochainLogsToTerminal);
  });
  kangarooEmitter.on(WASM_LOG, (holochainData) => {
    logHolochain(holochainData as HolochainData, logFileTransport, holochainLogsToTerminal);
  });
}

function logHolochain(
  holochainData: HolochainData,
  logFileTransport: winston.transports.FileTransportInstance,
  printToTerminal: boolean
) {
  const holochainVersion = (holochainData as HolochainData).version;
  const line = (holochainData as HolochainData).data;
  if (printToTerminal) {
    const logLine = `[HOLOCHAIN ${holochainVersion}]: ${line}`;
    console.log(logLine);
  }
  let logger = HOLOCHAIN_LOGGERS[holochainVersion];
  if (logger) {
    logger.log('info', line);
  } else {
    logger = createHolochainLogger(holochainVersion, logFileTransport);
    HOLOCHAIN_LOGGERS[holochainVersion] = logger;
    logger.log('info', line);
  }
}

function createHolochainLogger(
  holochainVersion: HolochainVersion,
  logFileTransport: winston.transports.FileTransportInstance
): winston.Logger {
  return createLogger({
    transports: [logFileTransport],
    format: combine(
      timestamp(),
      format.printf(({ level, message, timestamp }) => {
        return JSON.stringify({
          timestamp,
          label: `HOLOCHAIN ${holochainVersion}`,
          level,
          message,
        });
      })
    ),
  });
}

let KART_NET_LOGGER: winston.Logger | undefined;

/** E6.INTERNET.2: one kart-net line to the terminal and, once logs are set up, the profile log file. */
export function logKartNet(line: string): void {
  console.log(line);
  KART_NET_LOGGER?.log('info', line);
}

let STARTUP_LOGGER: winston.Logger | undefined;

/** E7.WINDOWS.1: one startup diagnostic line to the terminal and the profile log. Never secrets. */
export function logStartup(line: string): void {
  console.log(`[startup] ${line}`);
  STARTUP_LOGGER?.log('info', line);
}

function createLabelledLogger(
  label: string,
  logFileTransport: winston.transports.FileTransportInstance
): winston.Logger {
  return createLogger({
    transports: [logFileTransport],
    format: combine(
      timestamp(),
      format.printf(({ level, message, timestamp }) => JSON.stringify({ timestamp, label, level, message }))
    ),
  });
}

function createLairLogger(
  logFileTransport: winston.transports.FileTransportInstance
): winston.Logger {
  return createLogger({
    transports: [logFileTransport],
    format: combine(
      timestamp(),
      format.printf(({ level, message, timestamp }) => {
        return JSON.stringify({
          timestamp,
          label: 'LAIR',
          level,
          message,
        });
      })
    ),
  });
}
