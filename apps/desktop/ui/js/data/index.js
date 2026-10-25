// Backend selection. `ROADMAP.md` §B1 / D14: the same view code runs against
// a live replica under Tauri and against the fixture in a plain browser
// (`apps/desktop/README.md`); this is the one `if` that decides which.
//
// `window.__TAURI__` is injected by the Tauri runtime before any page script
// runs, so its presence is a reliable signal — a page opened with
// `python3 -m http.server` never has it. Guarded with `typeof window` so
// this module also loads under `node --test`, which has no `window` at all.

import { createFixtureProvider } from "./fixture.js";
import { createTauriProvider } from "./tauri.js";
import { DEFAULT_REALM_ID } from "./realms.js";

function hasTauri() {
  return typeof window !== "undefined" && Boolean(window.__TAURI__);
}

export const usingFixtures = !hasTauri();

/** The active provider — `tauri.js` when a Tauri runtime is present,
 * `fixture.js` otherwise. Every screen imports this and only this; nothing
 * outside `js/data/` should import `fixture.js` or `tauri.js` directly. */
export const provider = hasTauri()
  ? createTauriProvider(DEFAULT_REALM_ID)
  : createFixtureProvider(DEFAULT_REALM_ID);
