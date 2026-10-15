// Lobby UI checks by canonical character id. The ids (crates/turbo-kart
// CHARACTER_IDS) are fixed; display names are game content (src/characters.js)
// and change, so a check looks the name up by id and reads one member's own row
// (multiplayer.js: `li[data-agent]` ending " — <name>"), never the whole screen:
// a short name such as "B" or "Art" also occurs in "HOST" or "Start Race".
import path from 'node:path';
import { pathToFileURL } from 'node:url';

/** id → display name, from the game's src/characters.js. */
export async function characterNames(gameDir = path.resolve(process.env.TURBO_KART_GAME_DIR || '../../../turbo-kart-rally-main')) {
  const { RACERS } = await import(pathToFileURL(path.join(gameDir, 'src/characters.js')).href);
  return Object.fromEntries(RACERS.map((r) => [r.id, r.name]));
}

/** Page expression: the text of `agent`'s row in a lobby section (`mp-active` / `mp-waitlist`), or null. */
export const memberRow = (section, agent) =>
  `(document.querySelector('.multiplayer-screen .${section} li[data-agent=${JSON.stringify(agent)}]') || {}).textContent ?? null`;

/** Whether a member row names exactly `name`. */
export const namesCharacter = (row, name) => typeof row === 'string' && row.endsWith(` — ${name}`);
