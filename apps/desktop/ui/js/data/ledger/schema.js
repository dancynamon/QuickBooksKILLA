// The ledger schema contract. `docs/MCP.md`'s ledger section documents
// seventeen tools (ten reads, seven gated writes); this file is that
// documentation turned into something a test can check — derived by reading
// the `_json` rendering functions in `apps/ledger/src/mcp.rs` field by field
// (`tb_row_json`, `pnl_json`, `bs_row_json`, `sales_tax_lines_json`,
// `gl_line_json`/`gl_section_json`/`gl_detail_json`, `audit_row_json`,
// `journal_line_json`/`journal_entry_json`, `adjustment_request_json`,
// `bank_statement_json`/`bank_line_json`) and the dispatch in
// `LedgerToolSet::call_tool`.
//
// `test/ledger-fixture.test.js` uses `checkShape` (`../js/data/schema.js`'s
// `firstMissingField`, reused rather than duplicated) to assert the fixture
// provider's output actually has these fields for every one of the
// seventeen tools — the same "missing/renamed field" check the qbo-local
// schema makes, not a formatting nuance.

// `mcp.rs::journal_line_json`
export const JOURNAL_LINE_FIELDS = [
  "line_no", "account", "class", "debit", "credit", "memo", "entity",
  "is_taxable", "tax_amount", "tax_rate",
];

// `mcp.rs::journal_entry_json`
export const JOURNAL_ENTRY_FIELDS = [
  "entry_date", "memo", "source_type", "source_id", "source_version",
  "reversal_of", "is_flagged", "lines",
];

// `mcp.rs::tb_row_json`
export const TB_ROW_FIELDS = [
  "account_id", "number", "name", "classification", "is_contra",
  "needs_mapping", "source_ref", "debit", "credit", "balance",
];

// `mcp.rs::by_section_json`
export const BY_SECTION_FIELDS = ["by_account", "by_class", "total"];
export const BY_ACCOUNT_ROW_FIELDS = ["account_id", "amount"];
export const BY_CLASS_ROW_FIELDS = ["class_id", "amount"];

// `mcp.rs::bs_row_json` / `bs_section_json`
export const BS_ROW_FIELDS = ["account_id", "name", "balance"];
export const BS_SECTION_FIELDS = ["rows", "total"];

// `mcp.rs::sales_tax_lines_json`
export const SALES_TAX_LINES_FIELDS = [
  "a_total_income", "b_tax_collected", "c_taxable_sales", "d_nontaxable_sales",
  "e_line_level_taxable", "variance", "st50",
];
export const ST50_ROW_FIELDS = ["line", "amount"];

// `mcp.rs::gl_line_json`
export const GL_LINE_FIELDS = [
  "entry_id", "entry_date", "source_type", "source_id", "memo", "class",
  "entity", "is_flagged", "reversal_of", "debit", "credit", "running_balance",
];

// `mcp.rs::gl_section_json`
export const GL_SECTION_FIELDS = [
  "account_id", "number", "name", "opening_balance", "lines", "closing_balance",
];

// `mcp.rs::audit_row_json`
export const AUDIT_ROW_FIELDS = [
  "entry_id", "entry_date", "source_type", "source_id", "source_version",
  "memo", "is_flagged", "reversal_of", "actor", "command_kind", "at",
];

// `mcp.rs::adjustment_request_json`
export const ADJUSTMENT_REQUEST_FIELDS = [
  "request_id", "requested_by", "requested_at", "description", "lines",
  "state", "decided_by", "decided_at", "decision_note", "posted_entry_id",
  "period_closed",
];

// `mcp.rs::bank_statement_json`
export const BANK_STATEMENT_FIELDS = [
  "statement_id", "account_id", "period_start", "period_end",
  "opening_balance", "closing_balance", "imported_at", "closed_at",
];

// `mcp.rs::bank_line_json`
export const BANK_LINE_FIELDS = [
  "line_id", "statement_id", "account_id", "posted_on", "amount",
  "description", "external_id", "matched_entry_id", "matched_line_no",
  "match_kind", "matched_at",
];

/** Every write tool's result carries the company's current `locked_through`
 * (`docs/MCP.md` "The write gate", rule 2) alongside whatever the tool
 * itself returns. */
const WITH_LOCKED_THROUGH = ["locked_through"];

export const SCHEMA = {
  // --- reads -----------------------------------------------------------
  trial_balance: {
    result: "object",
    top: ["rows", "total_debits", "total_credits", "wrong_side"],
    row: TB_ROW_FIELDS,
  },
  profit_and_loss: {
    result: "object",
    top: ["income", "cogs", "expense", "gross_margin", "net_income"],
    section: BY_SECTION_FIELDS,
    byAccountRow: BY_ACCOUNT_ROW_FIELDS,
    byClassRow: BY_CLASS_ROW_FIELDS,
  },
  balance_sheet: {
    result: "object",
    top: ["assets", "liabilities", "equity", "total_liabilities_and_equity"],
    section: BS_SECTION_FIELDS,
    row: BS_ROW_FIELDS,
  },
  sales_tax_lines: {
    result: "object",
    top: SALES_TAX_LINES_FIELDS,
    st50Row: ST50_ROW_FIELDS,
  },
  general_ledger: {
    result: "object",
    top: ["sections"],
    section: GL_SECTION_FIELDS,
    line: GL_LINE_FIELDS,
  },
  audit_trail: {
    result: "array",
    row: AUDIT_ROW_FIELDS,
  },
  entries_for_document: {
    result: "array",
    row: ["entry_id", "entry", "is_posted"],
    entry: JOURNAL_ENTRY_FIELDS,
    line: JOURNAL_LINE_FIELDS,
  },
  list_adjustments: {
    result: "array",
    row: ADJUSTMENT_REQUEST_FIELDS,
    line: JOURNAL_LINE_FIELDS,
  },
  bank_status: {
    result: "object",
    top: ["statement", "lines", "summary"],
    statement: BANK_STATEMENT_FIELDS,
    line: BANK_LINE_FIELDS,
    summary: ["matched", "proposed", "unmatched"],
  },
  locked_through: {
    result: "object",
    top: ["locked_through"],
  },

  // --- writes — every one carries WITH_LOCKED_THROUGH -------------------
  save_and_post_document: {
    result: "object",
    top: ["document_id", "version", "entry_id", "entry", ...WITH_LOCKED_THROUGH],
    entry: JOURNAL_ENTRY_FIELDS,
    line: JOURNAL_LINE_FIELDS,
  },
  reverse_entry: {
    result: "object",
    top: ["entry_id", "entry", "actor", ...WITH_LOCKED_THROUGH],
    entry: JOURNAL_ENTRY_FIELDS,
    line: JOURNAL_LINE_FIELDS,
  },
  propose_adjustment: {
    result: "object",
    top: [...ADJUSTMENT_REQUEST_FIELDS, ...WITH_LOCKED_THROUGH],
    line: JOURNAL_LINE_FIELDS,
  },
  decide_adjustment: {
    result: "object",
    top: [...ADJUSTMENT_REQUEST_FIELDS, ...WITH_LOCKED_THROUGH],
    line: JOURNAL_LINE_FIELDS,
  },
  bank_confirm_proposal: {
    result: "object",
    top: ["entry_id", "actor", ...WITH_LOCKED_THROUGH],
  },
  close_period: {
    result: "object",
    top: ["period_end", ...WITH_LOCKED_THROUGH],
  },
  reopen_period: {
    result: "object",
    top: ["period_end", "reopened", ...WITH_LOCKED_THROUGH],
  },
};

export { firstMissingField } from "../schema.js";
