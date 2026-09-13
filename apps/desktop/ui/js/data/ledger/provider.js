// The ledger provider contract. `docs/MCP.md`'s ledger section documents
// seventeen `ledger-mcp` tools (ten reads, seven gated writes); this file is
// the one place that knows their wire names and argument shapes, so
// `fixture.js` and `tauri.js` each supply only a `callTool(tool, args)`
// function and get the same seventeen typed methods for free.
//
// Unlike `js/data/provider.js` (the qbo-local provider), there is no
// `realm_id` here: `ledger-mcp` is bound to one company for the life of its
// process (`LEDGER_DB`/`LEDGER_COMPANY`, `docs/MCP.md` "Registering it") —
// the Rust side picks the company at startup, so nothing on this side needs
// to thread one through per call.
//
// Every method returns a `Promise` of exactly the JSON shape `docs/MCP.md`
// documents for that tool: money as two-place decimal strings, every write's
// result carrying the company's current `locked_through` alongside whatever
// else it returns (`docs/MCP.md` "The write gate", rule 2).

/** Wire tool names, in `docs/MCP.md`'s own order (reads, then writes).
 * Exported so `schema.js` and the provider tests can check a call was
 * routed to the right one without hard-coding the string twice. */
export const TOOL = Object.freeze({
  trialBalance: "trial_balance",
  profitAndLoss: "profit_and_loss",
  balanceSheet: "balance_sheet",
  salesTaxLines: "sales_tax_lines",
  generalLedger: "general_ledger",
  auditTrail: "audit_trail",
  entriesForDocument: "entries_for_document",
  listAdjustments: "list_adjustments",
  bankStatus: "bank_status",
  lockedThrough: "locked_through",
  saveAndPostDocument: "save_and_post_document",
  reverseEntry: "reverse_entry",
  proposeAdjustment: "propose_adjustment",
  decideAdjustment: "decide_adjustment",
  bankConfirmProposal: "bank_confirm_proposal",
  closePeriod: "close_period",
  reopenPeriod: "reopen_period",
});

/** Drop any key whose value is `undefined`, so an omitted optional argument
 * (`general_ledger`'s `account`, `audit_trail`'s `since`, a write's `note`)
 * is actually absent from the call rather than sent as `"undefined"`. */
function args(raw) {
  const out = {};
  for (const [key, value] of Object.entries(raw)) {
    if (value !== undefined) out[key] = value;
  }
  return out;
}

/**
 * Build a provider over `callTool(tool, args)`, which does the actual work —
 * an in-memory fixture, or `invoke("ledger_query", { tool, args })` — and
 * must return a `Promise` resolving to that tool's `structuredContent`, or
 * rejecting with an `Error` carrying the tool's failure message.
 *
 * @param {(tool: string, args: object) => Promise<unknown>} callTool
 */
export function createProvider(callTool) {
  return {
    // --- reads -----------------------------------------------------------
    trialBalance(asOf) {
      return callTool(TOOL.trialBalance, { as_of: asOf });
    },
    profitAndLoss(from, to) {
      return callTool(TOOL.profitAndLoss, { from, to });
    },
    balanceSheet(asOf) {
      return callTool(TOOL.balanceSheet, { as_of: asOf });
    },
    salesTaxLines(year, quarter, rate) {
      return callTool(TOOL.salesTaxLines, args({ year, quarter, rate }));
    },
    generalLedger(from, to, account) {
      return callTool(TOOL.generalLedger, args({ from, to, account }));
    },
    auditTrail(since) {
      return callTool(TOOL.auditTrail, args({ since }));
    },
    entriesForDocument(documentId) {
      return callTool(TOOL.entriesForDocument, { document_id: documentId });
    },
    listAdjustments(state) {
      return callTool(TOOL.listAdjustments, args({ state }));
    },
    bankStatus(statementId) {
      return callTool(TOOL.bankStatus, { statement_id: statementId });
    },
    lockedThrough() {
      return callTool(TOOL.lockedThrough, {});
    },

    // --- writes ------------------------------------------------------------
    // Every write's result carries `locked_through` (`docs/MCP.md`'s write
    // gate, rule 2) — nothing to add here, the tool result already has it.
    saveAndPostDocument(document, actor, items) {
      return callTool(TOOL.saveAndPostDocument, args({ document, actor, items }));
    },
    reverseEntry(entryId, on, actor) {
      return callTool(TOOL.reverseEntry, { entry_id: entryId, on, actor });
    },
    proposeAdjustment(requestedBy, description, lines) {
      return callTool(TOOL.proposeAdjustment, {
        requested_by: requestedBy,
        description,
        lines,
      });
    },
    decideAdjustment(requestId, decidedBy, approve, note) {
      return callTool(
        TOOL.decideAdjustment,
        args({ request_id: requestId, decided_by: decidedBy, approve, note }),
      );
    },
    bankConfirmProposal(lineId, account, cls, actor) {
      return callTool(
        TOOL.bankConfirmProposal,
        args({ line_id: lineId, account, class: cls, actor }),
      );
    },
    closePeriod(periodEnd, actor, note) {
      return callTool(TOOL.closePeriod, args({ period_end: periodEnd, actor, note }));
    },
    reopenPeriod(periodEnd, actor, note) {
      return callTool(TOOL.reopenPeriod, args({ period_end: periodEnd, actor, note }));
    },
  };
}
