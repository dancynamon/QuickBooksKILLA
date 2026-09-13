// Provider selection and the Tauri backend's tool/argument mapping.
// `ROADMAP.md` §B1: `js/data/index.js` must pick the right backend, and
// `js/data/tauri.js` must call `invoke("query", { tool, args })` with
// exactly the tool name and argument keys `docs/MCP.md` documents — a typo
// here would silently reach the real replica with the wrong shape and only
// fail once someone runs it under Tauri.

import { test } from "node:test";
import assert from "node:assert/strict";

import { createTauriProvider } from "../js/data/tauri.js";
import { TOOL } from "../js/data/provider.js";

/** A fake `window.__TAURI__.core.invoke` that records every call instead of
 * doing anything — enough to assert the mapping without a real Tauri
 * runtime, which does not exist on this machine (`apps/desktop/README.md`). */
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

test("tauri provider calls invoke('query', ...) for every tool, with the documented tool name and argument keys", async () => {
  const calls = stubTauri();
  const provider = createTauriProvider("1234567890123456");

  await provider.search("21234", 5);
  await provider.documentDetail("418");
  await provider.listDocuments("Invoice", "2026-01-01", "2026-12-31", 10, 50);
  await provider.openDocuments("Bill", 0, 25);
  await provider.contactDetail("customer", "31");
  await provider.itemDetail("12");
  await provider.arAging("2026-09-11");
  await provider.apAging("2026-09-11");
  await provider.syncStatus();
  await provider.classTree();
  await provider.chartOfAccounts();

  assert.equal(calls.length, 11);
  for (const call of calls) assert.equal(call.command, "query");

  const byTool = Object.fromEntries(calls.map((c) => [c.tool, c.args]));

  assert.equal(byTool[TOOL.search].query, "21234");
  assert.equal(byTool[TOOL.search].limit, 5);
  assert.equal(byTool[TOOL.search].realm_id, "1234567890123456");

  assert.deepEqual(byTool[TOOL.documentDetail], { realm_id: "1234567890123456", qbo_id: "418" });

  assert.deepEqual(byTool[TOOL.listDocuments], {
    realm_id: "1234567890123456",
    doc_type: "Invoice",
    from: "2026-01-01",
    to: "2026-12-31",
    offset: 10,
    limit: 50,
  });

  assert.deepEqual(byTool[TOOL.openDocuments], {
    realm_id: "1234567890123456",
    doc_type: "Bill",
    offset: 0,
    limit: 25,
  });

  assert.deepEqual(byTool[TOOL.contactDetail], {
    realm_id: "1234567890123456",
    contact_type: "customer",
    qbo_id: "31",
  });

  assert.deepEqual(byTool[TOOL.itemDetail], { realm_id: "1234567890123456", qbo_id: "12" });
  assert.deepEqual(byTool[TOOL.arAging], { realm_id: "1234567890123456", as_of: "2026-09-11" });
  assert.deepEqual(byTool[TOOL.apAging], { realm_id: "1234567890123456", as_of: "2026-09-11" });
  assert.deepEqual(byTool[TOOL.syncStatus], { realm_id: "1234567890123456" });
  assert.deepEqual(byTool[TOOL.classTree], { realm_id: "1234567890123456" });
  assert.deepEqual(byTool[TOOL.chartOfAccounts], { realm_id: "1234567890123456" });
});

test("an omitted optional argument is absent from the call, not sent as undefined", async () => {
  const calls = stubTauri();
  const provider = createTauriProvider("1234567890123456");

  await provider.listDocuments("Invoice");

  assert.deepEqual(calls[0].args, { realm_id: "1234567890123456", doc_type: "Invoice" });
  assert.ok(!("from" in calls[0].args));
  assert.ok(!("to" in calls[0].args));
  assert.ok(!("offset" in calls[0].args));
  assert.ok(!("limit" in calls[0].args));
});

test("setRealmId changes the realm_id on every subsequent call", async () => {
  const calls = stubTauri();
  const provider = createTauriProvider("1111111111111111");

  await provider.syncStatus();
  provider.setRealmId("2222222222222222");
  await provider.syncStatus();

  assert.equal(calls[0].args.realm_id, "1111111111111111");
  assert.equal(calls[1].args.realm_id, "2222222222222222");
  assert.equal(provider.getRealmId(), "2222222222222222");
});

test("a tool-level failure from invoke becomes a rejected promise carrying the message", async () => {
  globalThis.window = {
    __TAURI__: {
      core: {
        invoke: async () => {
          throw "no bill \"does-not-exist\" in this realm"; // Tauri rejects with the Err payload, often a plain string
        },
      },
    },
  };
  const provider = createTauriProvider("1234567890123456");

  await assert.rejects(
    () => provider.documentDetail("does-not-exist"),
    (err) => err instanceof Error && err.message.includes("does-not-exist"),
  );
});

// ---------------------------------------------------------------------------
// backend selection — js/data/index.js
// ---------------------------------------------------------------------------

test("index.js selects the fixture provider when no Tauri runtime is present", async () => {
  delete globalThis.window;
  const mod = await import("../js/data/index.js");
  assert.equal(mod.usingFixtures, true);
  // The fixture provider should actually work end to end through the
  // selected `provider` export.
  const status = await mod.provider.syncStatus();
  assert.equal(typeof status.write_enabled, "boolean");
});

test("index.js selects the tauri provider when window.__TAURI__ is present", async () => {
  const calls = stubTauri({ ok: true });
  // A distinct query string forces Node's ESM loader to re-evaluate the
  // module under the `window.__TAURI__` set up above, rather than reusing
  // the cached instance from the previous test (which saw no `window` at
  // all and permanently decided on the fixture at import time).
  const mod = await import("../js/data/index.js?with-tauri-runtime");
  assert.equal(mod.usingFixtures, false);
  await mod.provider.syncStatus();
  assert.equal(calls.length, 1);
  assert.equal(calls[0].command, "query");
});
