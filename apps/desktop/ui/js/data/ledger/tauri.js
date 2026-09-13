// The live ledger backend: one Tauri command, `ledger_query`, wrapping
// `qbo_desktop_commands::ledger_query` (`apps/desktop/commands`), which
// drives `ledger::mcp::Server::handle_line` the same way `js/data/tauri.js`
// drives `qbo_local::mcp::Server::handle_line` for the replica — this file's
// only job is the `invoke` call and turning a rejected tool call into a
// thrown `Error`. No tool dispatch, no argument shaping: that's
// `provider.js`'s job, shared with `fixture.js`, so the two backends can
// never drift apart on what a call looks like.
//
// Unlike `js/data/tauri.js`, there is no `realm_id`/company argument on this
// call: `LEDGER_DB` and `LEDGER_COMPANY` are read once, on the Rust side, at
// process startup (`docs/MCP.md` "Registering it") — the company this
// window is talking to is fixed for the life of the window, the same way
// `ledger-mcp` itself is bound to one company per process.

import { createProvider } from "./provider.js";

/**
 * Call the Tauri `ledger_query` command and unwrap its result. Mirrors
 * `qbo_desktop_commands::ledger_query`'s own contract: a resolved value is
 * exactly `structuredContent`; a tool-level failure (`isError` in MCP terms)
 * is surfaced as a rejected promise carrying the failure's own message, not
 * a generic "request failed".
 * @param {string} tool
 * @param {object} args
 */
async function callTool(tool, args) {
  try {
    return await window.__TAURI__.core.invoke("ledger_query", { tool, args });
  } catch (err) {
    // Tauri rejects with whatever the command's `Err` serialises to — for
    // `CommandError` that's its `Display` string (thiserror), already the
    // human-readable message a screen should show, not a wrapper to peel.
    const message = typeof err === "string" ? err : err?.message ?? String(err);
    throw new Error(message);
  }
}

/** The provider backed by the real ledger, through Tauri. */
export function createTauriProvider() {
  return createProvider(callTool);
}
