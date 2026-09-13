// The live backend: one Tauri command, `query`, wrapping
// `qbo_desktop_commands::query` (`apps/desktop/commands`), which itself
// drives `qbo_local::mcp::Server::handle_line` — so this file's only job is
// the `invoke` call and turning a rejected tool call into a thrown `Error`.
// No tool dispatch, no argument shaping: that's `provider.js`'s job, shared
// with `fixture.js`, so the two backends can never drift apart on what a
// call looks like.

import { createProvider } from "./provider.js";

/**
 * Call the Tauri `query` command and unwrap its result. Mirrors
 * `qbo_desktop_commands::query`'s own contract: a resolved value is exactly
 * `structuredContent`; a tool-level failure (`isError` in MCP terms) is
 * surfaced as a rejected promise carrying the failure's own message, not a
 * generic "request failed".
 * @param {string} tool
 * @param {object} args
 */
async function callTool(tool, args) {
  try {
    return await window.__TAURI__.core.invoke("query", { tool, args });
  } catch (err) {
    // Tauri rejects with whatever the command's `Err` serialises to — for
    // `CommandError` that's its `Display` string (thiserror), already the
    // human-readable message a screen should show, not a wrapper to peel.
    const message = typeof err === "string" ? err : err?.message ?? String(err);
    throw new Error(message);
  }
}

/**
 * The provider backed by the real replica, through Tauri. `initialRealmId`
 * is required — unlike the fixture provider there is no invented default
 * realm to fall back to, since a wrong guess here would be a wrong guess
 * about which company's book is on screen.
 * @param {string} initialRealmId
 */
export function createTauriProvider(initialRealmId) {
  return createProvider(callTool, initialRealmId);
}
