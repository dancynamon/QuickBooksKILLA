// The provider contract. `docs/MCP.md` documents eleven read tools; this
// file is the one place that knows their wire names and argument shapes, so
// `fixture.js` and `tauri.js` each supply only a `callTool(tool, args)`
// function and get the same eleven typed methods for free — a screen can
// call `provider.arAging(asOf)` without knowing or caring which backend is
// answering.
//
// Every method returns a `Promise` of exactly the JSON shape `docs/MCP.md`
// documents for that tool: money as two-place decimal strings, enums as
// their `as_str` names, both backends alike. Neither backend reshapes a
// result on the way out — reshaping is a screen's job, if it needs one.

/** Wire tool names, in `docs/MCP.md`'s order. Exported so `schema.js` and
 * the provider tests can check a call was routed to the right one without
 * hard-coding the string twice. */
export const TOOL = Object.freeze({
  search: "search",
  documentDetail: "document_detail",
  listDocuments: "list_documents",
  openDocuments: "open_documents",
  contactDetail: "contact_detail",
  itemDetail: "item_detail",
  arAging: "ar_aging",
  apAging: "ap_aging",
  syncStatus: "sync_status",
  classTree: "class_tree",
  chartOfAccounts: "chart_of_accounts",
});

/**
 * Build a provider over `callTool(tool, args)`, which does the actual work —
 * an in-memory fixture lookup, or `invoke("query", { tool, args })` — and
 * must return a `Promise` resolving to that tool's `structuredContent`, or
 * rejecting with an `Error` carrying the tool's failure message.
 *
 * `realm_id` is threaded onto every call from the provider's own current
 * realm rather than from each method's arguments, so the realm switcher
 * (`ROADMAP.md` §B1) has exactly one place to change it —
 * `setRealmId` — instead of every call site needing to know it.
 *
 * @param {(tool: string, args: object) => Promise<unknown>} callTool
 * @param {string} initialRealmId
 */
export function createProvider(callTool, initialRealmId) {
  let realmId = initialRealmId;

  /** Merge `realm_id` onto an argument object, dropping any key whose value
   * is `undefined` so an omitted optional argument (`list_documents`'
   * `from`/`to`, a paging `offset`/`limit`) is actually absent from the
   * call rather than sent as the literal string `"undefined"`. */
  function args(extra) {
    const out = { realm_id: realmId };
    for (const [key, value] of Object.entries(extra)) {
      if (value !== undefined) out[key] = value;
    }
    return out;
  }

  return {
    getRealmId: () => realmId,
    setRealmId(id) {
      realmId = id;
    },

    search(query, limit) {
      return callTool(TOOL.search, args({ query, limit }));
    },
    documentDetail(qboId) {
      return callTool(TOOL.documentDetail, args({ qbo_id: qboId }));
    },
    listDocuments(docType, from, to, offset, limit) {
      return callTool(
        TOOL.listDocuments,
        args({ doc_type: docType, from, to, offset, limit }),
      );
    },
    openDocuments(docType, offset, limit) {
      return callTool(TOOL.openDocuments, args({ doc_type: docType, offset, limit }));
    },
    contactDetail(contactType, qboId) {
      return callTool(
        TOOL.contactDetail,
        args({ contact_type: contactType, qbo_id: qboId }),
      );
    },
    itemDetail(qboId) {
      return callTool(TOOL.itemDetail, args({ qbo_id: qboId }));
    },
    arAging(asOf) {
      return callTool(TOOL.arAging, args({ as_of: asOf }));
    },
    apAging(asOf) {
      return callTool(TOOL.apAging, args({ as_of: asOf }));
    },
    syncStatus() {
      return callTool(TOOL.syncStatus, args({}));
    },
    classTree() {
      return callTool(TOOL.classTree, args({}));
    },
    chartOfAccounts() {
      return callTool(TOOL.chartOfAccounts, args({}));
    },
  };
}
