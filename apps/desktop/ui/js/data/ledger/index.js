// Backend selection for the ledger provider — the same `window.__TAURI__`
// check `js/data/index.js` makes for the qbo-local replica provider
// (`apps/desktop/README.md`), kept as its own module because the ledger
// provider has its own backend pair (`ledger/fixture.js`, `ledger/tauri.js`)
// and its own Tauri command (`ledger_query`, not `query`).

import { createFixtureProvider } from "./fixture.js";
import { createTauriProvider } from "./tauri.js";

function hasTauri() {
  return typeof window !== "undefined" && Boolean(window.__TAURI__);
}

export const usingFixtures = !hasTauri();

/** The active ledger provider — `tauri.js` when a Tauri runtime is present,
 * `fixture.js` otherwise. Every ledger screen imports this and only this;
 * nothing outside `js/data/ledger/` should import `fixture.js` or
 * `tauri.js` directly. */
export const ledgerProvider = hasTauri() ? createTauriProvider() : createFixtureProvider();
