// The ledger provider contract. `docs/MCP.md`'s ledger section documents
// twenty-eight `ledger-mcp` tools (fifteen reads, thirteen gated writes); this
// file is the one place that knows their wire names and argument shapes, so
// `fixture.js` and `tauri.js` each supply only a `callTool(tool, args)`
// function and get the same twenty-eight typed methods for free.
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
  listContacts: "list_contacts",
  listItems: "list_items",
  openDocuments: "open_documents",
  aging: "aging",
  getDocument: "get_document",
  upsertContact: "upsert_contact",
  upsertItem: "upsert_item",
  createDocument: "create_document",
  updateDocument: "update_document",
  voidDocument: "void_document",
  applyCredit: "apply_credit",
  paymentLink: "payment_link",
  renderDocument: "render_document",
  renderStatement: "render_statement",
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
    // The book's own master data and open items (D28).
    listContacts(kind, query, includeInactive) {
      return callTool(TOOL.listContacts, args({ kind, query, include_inactive: includeInactive }));
    },
    listItems(query, includeInactive) {
      return callTool(TOOL.listItems, args({ query, include_inactive: includeInactive }));
    },
    /** `book` is `"receivable"` or `"payable"`. */
    openDocuments(book, asOf) {
      return callTool(TOOL.openDocuments, { book, as_of: asOf });
    },
    aging(book, asOf) {
      return callTool(TOOL.aging, { book, as_of: asOf });
    },
    /** A saved document as an editor needs it back, newest version. */
    getDocument(documentId) {
      return callTool(TOOL.getDocument, { document_id: documentId });
    },

    // --- writes ------------------------------------------------------------
    // Every write's result carries `locked_through` (`docs/MCP.md`'s write
    // gate, rule 2) — nothing to add here, the tool result already has it.
    upsertContact(contact) {
      return callTool(TOOL.upsertContact, { contact });
    },
    upsertItem(item) {
      return callTool(TOOL.upsertItem, { item });
    },
    /** The standalone entry path (D28): send what a person typed; the book
     * fills the number, due date, header class and sales tax. */
    createDocument(document, actor) {
      return callTool(TOOL.createDocument, { document, actor });
    },
    /** Edit a saved document: what `createDocument` takes, plus its
     * `document_id`. A new version; the old entry reversed and the new one
     * posted when the posting changed. The number is kept. */
    updateDocument(document, actor) {
      return callTool(TOOL.updateDocument, { document, actor });
    },
    /** Void a saved document: a new version and a reversal, never a delete. */
    voidDocument(documentId, reason, actor) {
      return callTool(TOOL.voidDocument, { document_id: documentId, reason, actor });
    },
    /** Apply an open credit memo (or vendor credit) to an open invoice (or
     * bill) of the same contact. `amount` omitted applies as much as both
     * have open. */
    applyCredit(creditId, targetDocumentId, amount, actor) {
      return callTool(
        TOOL.applyCredit,
        args({ credit_id: creditId, target_document_id: targetDocumentId, amount, actor }),
      );
    },
    /** D30: an Authorize.net hosted payment page for one open invoice, for
     * its open balance. Writes nothing; the token lasts 15 minutes, so it is
     * minted when the page is about to be opened, never printed. */
    paymentLink(documentId) {
      return callTool(TOOL.paymentLink, { document_id: documentId });
    },
    /** D39: write a PDF of one saved document; returns its path. `payLink`
     * false leaves an open invoice's Authorize.net page out. Sends nothing. */
    renderDocument(documentId, payLink) {
      return callTool(TOOL.renderDocument, args({ document_id: documentId, pay_link: payLink }));
    },
    /** D39: a customer statement PDF as of a date; returns its path. */
    renderStatement(contactId, asOf) {
      return callTool(TOOL.renderStatement, { contact_id: contactId, as_of: asOf });
    },
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
