/** UI-root-relative path → contents; returns one message per unresolved import. */
export function missingModules(files: Map<string, string | Uint8Array>): string[];
/** One message per `./assets/…` URL literal in a packaged script whose file is not packaged. */
export function missingAssets(files: Map<string, string | Uint8Array>): string[];
