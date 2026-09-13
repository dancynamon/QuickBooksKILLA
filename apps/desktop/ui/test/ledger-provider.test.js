// Ledger provider selection and the Tauri backend's tool/argument mapping —
// the ledger sibling of `test/provider.test.js`. `js/data/ledger/index.js`
// must pick the right backend, and `js/data/ledger/tauri.js` must call
// `invoke("ledger_query", { tool, args })` with exactly the tool name and
// argument keys `docs/MCP.md`'s ledger section documents, for all
// seventeen tools — a typo here would silently reach the real ledger with
// the wrong shape and only fail once someone runs it under Tauri.

import { test } from "node:test";
import assert from "node:assert/strict";

import { createTauriProvider } from "../js/data/ledger/tauri.js";
import { TOOL } from "../js/data/ledger/provider.js";

/** A fake `window.__TAURI__.core.invoke` that records every call instead of
 * doing anything — `test/provider.test.js`'s own pattern, since there is no
 * real Tauri runtime on this machine (`apps/desktop/README.md`). */
function stubTauri(result = {}) {
  const calls = [];
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke: async (command, payload) => {
          calls.push({ command, ...payload });
          return result;
        },
      },
    },
  };
  return calls;
}

test.afterEach(() => {
  delete globalThis.window;
});

test("ledger tauri provider calls invoke('ledger_query', ...) for every tool, with the documented tool name and argument keys", async () => {
  const calls = stubTauri();
  const ledger = createTauriProvider();

  await ledger.trialBalance("2026-09-30");
  await ledger.profitAndLoss("2026-01-01", "2026-09-30");
  await ledger.balanceSheet("2026-09-30");
  await ledger.salesTaxLines(2026, 3);
  await ledger.generalLedger("2026-09-01", "2026-09-30", "1200");
  await ledger.auditTrail("2026-09-01");
  await ledger.entriesForDocument("inv-1088");
  await ledger.listAdjustments("proposed");
  await ledger.bankStatus("stmt-2026-08");
  await ledger.lockedThrough();
  await ledger.saveAndPostDocument({ document_id: "d1", kind: "Invoice" }, "dan", { i1: { income: "4100", expense: "5000" } });
  await ledger.reverseEntry("je-1", "2026-09-15", "dan");
  await ledger.proposeAdjustment("joel", "why", [{ account: "6100", debit: "1.00" }]);
  await ledger.decideAdjustment("adj-1", "dan", true, "ok");
  await ledger.bankConfirmProposal("bl-1", "6600", "foam", "dan");
  await ledger.closePeriod("2026-08-31", "dan", "close note");
  await ledger.reopenPeriod("2026-08-31", "dan", "reopen note");

  assert.equal(calls.length, 17);
  for (const call of calls) assert.equal(call.command, "ledger_query");

  const byTool = Object.fromEntries(calls.map((c) => [c.tool, c.args]));

  assert.deepEqual(byTool[TOOL.trialBalance], { as_of: "2026-09-30" });
  assert.deepEqual(byTool[TOOL.profitAndLoss], { from: "2026-01-01", to: "2026-09-30" });
  assert.deepEqual(byTool[TOOL.balanceSheet], { as_of: "2026-09-30" });
  assert.deepEqual(byTool[TOOL.salesTaxLines], { year: 2026, quarter: 3 });
  assert.deepEqual(byTool[TOOL.generalLedger], { from: "2026-09-01", to: "2026-09-30", account: "1200" });
  assert.deepEqual(byTool[TOOL.auditTrail], { since: "2026-09-01" });
  assert.deepEqual(byTool[TOOL.entriesForDocument], { document_id: "inv-1088" });
  assert.deepEqual(byTool[TOOL.listAdjustments], { state: "proposed" });
  assert.deepEqual(byTool[TOOL.bankStatus], { statement_id: "stmt-2026-08" });
  assert.deepEqual(byTool[TOOL.lockedThrough], {});
  assert.deepEqual(byTool[TOOL.saveAndPostDocument], {
    document: { document_id: "d1", kind: "Invoice" }, actor: "dan",
    items: { i1: { income: "4100", expense: "5000" } },
  });
  assert.deepEqual(byTool[TOOL.reverseEntry], { entry_id: "je-1", on: "2026-09-15", actor: "dan" });
  assert.deepEqual(byTool[TOOL.proposeAdjustment], {
    requested_by: "joel", description: "why", lines: [{ account: "6100", debit: "1.00" }],
  });
  assert.deepEqual(byTool[TOOL.decideAdjustment], { request_id: "adj-1", decided_by: "dan", approve: true, note: "ok" });
  assert.deepEqual(byTool[TOOL.bankConfirmProposal], { line_id: "bl-1", account: "6600", class: "foam", actor: "dan" });
  assert.deepEqual(byTool[TOOL.closePeriod], { period_end: "2026-08-31", actor: "dan", note: "close note" });
  assert.deepEqual(byTool[TOOL.reopenPeriod], { period_end: "2026-08-31", actor: "dan", note: "reopen note" });
});

test("an omitted optional argument is absent from the call, not sent as undefined", async () => {
  const calls = stubTauri();
  const ledger = createTauriProvider();

  await ledger.auditTrail();
  await ledger.listAdjustments();
  await ledger.generalLedger("2026-09-01", "2026-09-30");

  assert.deepEqual(calls[0].args, {});
  assert.deepEqual(calls[1].args, {});
  assert.deepEqual(calls[2].args, { from: "2026-09-01", to: "2026-09-30" });
});

test("a tool-level failure from invoke becomes a rejected promise carrying the message", async () => {
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke: async () => {
          throw "period closed through 2026-08-31: cannot post this document on or before this date";
        },
      },
    },
  };
  const ledger = createTauriProvider();

  await assert.rejects(
    () => ledger.closePeriod("2026-07-01", "dan", "too early"),
    (err) => err instanceof Error && err.message.includes("period closed"),
  );
});

// ---------------------------------------------------------------------------
// backend selection — js/data/ledger/index.js
// ---------------------------------------------------------------------------

test("ledger index.js selects the fixture provider when no Tauri runtime is present", async () => {
  delete globalThis.window;
  const mod = await import("../js/data/ledger/index.js");
  assert.equal(mod.usingFixtures, true);
  const locked = await mod.ledgerProvider.lockedThrough();
  assert.ok("locked_through" in locked);
});

test("ledger index.js selects the tauri provider when window.__TAURI__ is present", async () => {
  const calls = stubTauri({ locked_through: null });
  const mod = await import("../js/data/ledger/index.js?with-tauri-runtime");
  assert.equal(mod.usingFixtures, false);
  await mod.ledgerProvider.lockedThrough();
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "ledger_query");
  assert.equal(calls[0].tool, TOOL.lockedThrough);
});
