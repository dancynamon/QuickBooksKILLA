// The ledger fixture provider: an in-memory book shaped exactly like
// `docs/MCP.md`'s ledger tool results, invented the same way
// `prototype/bunzbooks.html` and `js/data/fixture.js` (the qbo-local
// fixture) are (`prototype/README.md`: "invent nothing real"). It exists so
// every ledger screen runs the same code path in a plain browser as it
// would under Tauri — every screen reads only through `ledgerProvider`, and
// this is the only file that knows the book behind it is made up.
//
// Unlike the qbo-local fixture, there is no realm switch here: `ledger-mcp`
// is bound to one company for the life of its process (`docs/MCP.md`), so
// this fixture is one small, internally consistent book — a handful of
// posted entries an accrual accountant could actually read, re-deriving the
// report math `apps/ledger/src/report.rs` and `accountant.rs` implement in
// Rust (trial balance, P&L, balance sheet, the §9 sales-tax lines, GL
// running balances, the audit trail) rather than returning canned per-screen
// payloads, because a fixture that only returns canned payloads would not
// exercise anything a screen actually depends on.
//
// Every posted `JournalLine` always carries *both* `debit` and `credit` as
// decimal strings, one of them `"0.00"` — never `null` on one side — exactly
// matching `mcp.rs::journal_line_json`'s own `money_str` convention.
//
// `createFixtureProvider()` builds a fresh, independent book on every call
// — unlike `js/data/fixture.js` (read-only, so one shared `DATA` object is
// harmless), this fixture has real writes, and two `createFixtureProvider()`
// calls must never see each other's posted entries, exactly as two
// `ledger-mcp` processes over two different `.sqlite` files never would.

import { createProvider } from "./provider.js";

// ---------------------------------------------------------------------------
// money helpers — cent-integer arithmetic, `js/data/fixture.js`'s pattern.
// ---------------------------------------------------------------------------

function toCents(dollars) {
  if (dollars == null) return 0;
  return Math.round(Number(dollars) * 100);
}

function fromCents(cents) {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(Math.round(cents));
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, "0")}`;
}

class LedgerFixtureError extends Error {}

// ---------------------------------------------------------------------------
// the chart of accounts (`apps/ledger/src/chart.rs`'s numbers and names, a
// working subset rather than the full seed table) — read-only, so safe to
// share across every book this module creates.
// ---------------------------------------------------------------------------

const ACCOUNTS = [
  { number: "1100", name: "Checking", classification: "Asset", is_contra: false },
  { number: "1150", name: "Undeposited funds", classification: "Asset", is_contra: false },
  { number: "1200", name: "Accounts receivable", classification: "Asset", is_contra: false },
  { number: "1300", name: "Inventory, raw materials", classification: "Asset", is_contra: false },
  { number: "1310", name: "Inventory, finished goods", classification: "Asset", is_contra: false },
  { number: "2000", name: "Accounts payable", classification: "Liability", is_contra: false },
  { number: "2200", name: "Sales tax payable", classification: "Liability", is_contra: false },
  { number: "3900", name: "Retained earnings", classification: "Equity", is_contra: false },
  { number: "4100", name: "Sales income", classification: "Income", is_contra: false },
  { number: "4300", name: "Shipping income", classification: "Income", is_contra: false },
  { number: "4900", name: "Discounts given", classification: "Income", is_contra: true },
  { number: "5000", name: "Cost of goods sold", classification: "COGS", is_contra: false },
  { number: "5300", name: "Parts and labour applied", classification: "COGS", is_contra: true },
  { number: "6100", name: "Shop supplies and packaging", classification: "Expense", is_contra: false },
  { number: "6600", name: "Advertising and channel fees", classification: "Expense", is_contra: false },
  { number: "6900", name: "Other operating expense", classification: "Expense", is_contra: false },
].map((a) => ({ ...a, needs_mapping: false, source_ref: null }));

const ACCOUNTS_BY_NUMBER = new Map(ACCOUNTS.map((a) => [a.number, a]));

function accountOrThrow(number) {
  const account = ACCOUNTS_BY_NUMBER.get(number);
  if (!account) throw new LedgerFixtureError(`no account ${JSON.stringify(number)} in the chart`);
  return account;
}

/** §3's class rule: an income/COGS/expense line always carries a class; a
 * balance-sheet line never does. */
function classificationNeedsClass(classification) {
  return classification === "Income" || classification === "COGS" || classification === "Expense";
}

const NORMAL_SIDE_IS_DEBIT = { Asset: true, COGS: true, Expense: true, Liability: false, Equity: false, Income: false };

function buildEntry({ id, date, memo, sourceType, sourceId, sourceVersion = 1, reversalOf = null, isFlagged = false, actor, commandKind, at, lines }) {
  return {
    entry_id: id,
    entry_date: date,
    memo,
    source_type: sourceType,
    source_id: sourceId,
    source_version: sourceVersion,
    reversal_of: reversalOf,
    is_flagged: isFlagged,
    actor,
    command_kind: commandKind,
    at,
    lines: lines.map((l, i) => ({
      line_no: i + 1,
      account: l.account,
      class: l.class ?? null,
      debit_cents: toCents(l.debit ?? 0),
      credit_cents: toCents(l.credit ?? 0),
      memo: l.memo ?? null,
      entity: l.entity ?? null,
      is_taxable: Boolean(l.is_taxable),
      tax_amount_cents: l.tax_amount != null ? toCents(l.tax_amount) : null,
      tax_rate: l.tax_rate ?? null,
    })),
  };
}

function journalLineJson(line) {
  return {
    line_no: line.line_no,
    account: line.account,
    class: line.class,
    debit: fromCents(line.debit_cents),
    credit: fromCents(line.credit_cents),
    memo: line.memo,
    entity: line.entity,
    is_taxable: line.is_taxable,
    tax_amount: line.tax_amount_cents != null ? fromCents(line.tax_amount_cents) : null,
    tax_rate: line.tax_rate,
  };
}

function journalEntryJson(entry) {
  return {
    entry_date: entry.entry_date,
    memo: entry.memo,
    source_type: entry.source_type,
    source_id: entry.source_id,
    source_version: entry.source_version,
    reversal_of: entry.reversal_of,
    is_flagged: entry.is_flagged,
    lines: entry.lines.map(journalLineJson),
  };
}

/** A deliberately small posting engine — enough to move the trial balance
 * the way `post::post`'s rules table would for the document kinds this UI
 * actually exercises, not a re-implementation of `LEDGER-DESIGN.md` §1's
 * full table. Pure: takes the document and the caller's `items` map, does
 * not touch any book state. */
function deriveEntryLines(document, items) {
  const itemAccounts = (itemId) => {
    const spec = items?.[itemId];
    if (!spec) throw new LedgerFixtureError(`item ${JSON.stringify(itemId)} has no entry in "items"`);
    return spec;
  };
  const headerClass = document.header_class ?? null;
  const lineClass = (line) => line.class ?? (line.item_id ? itemAccounts(line.item_id).default_class : null) ?? headerClass;
  const lineTotalCents = document.lines.reduce((sum, l) => sum + toCents(l.amount), 0);
  const taxCents = document.tax ? toCents(document.tax.total_tax) : 0;

  function creditIncomeLines() {
    return document.lines.map((l) => ({
      account: l.item_id ? itemAccounts(l.item_id).income : l.account,
      class: lineClass(l),
      credit: Number(l.amount),
      is_taxable: Boolean(l.is_taxable),
    }));
  }
  function debitExpenseLines() {
    return document.lines.map((l) => ({
      account: l.item_id ? (itemAccounts(l.item_id).asset ?? itemAccounts(l.item_id).expense) : l.account,
      class: lineClass(l),
      debit: Number(l.amount),
    }));
  }

  const entity = document.contact
    ? { kind: document.contact.kind.toLowerCase(), id: document.contact.id }
    : null;

  switch (document.kind) {
    case "Invoice":
    case "SalesReceipt": {
      const debitAccount = document.kind === "SalesReceipt" ? "1150" : "1200";
      const lines = [{ account: debitAccount, debit: (lineTotalCents + taxCents) / 100, entity }, ...creditIncomeLines()];
      if (taxCents !== 0) lines.push({ account: "2200", credit: taxCents / 100 });
      return lines;
    }
    case "Payment": {
      return [
        { account: "1100", debit: lineTotalCents / 100, entity },
        { account: "1200", credit: lineTotalCents / 100, entity },
      ];
    }
    case "Bill":
    case "Purchase": {
      const creditAccount = document.kind === "Purchase" ? "1100" : "2000";
      return [...debitExpenseLines(), { account: creditAccount, credit: lineTotalCents / 100, entity }];
    }
    case "BillPayment": {
      return [
        { account: "2000", debit: lineTotalCents / 100, entity },
        { account: "1100", credit: lineTotalCents / 100, entity },
      ];
    }
    case "JournalEntry": {
      return document.lines.map((l) => ({
        account: l.account,
        class: l.class ?? headerClass,
        debit: l.posting === "Debit" ? Number(l.amount) : 0,
        credit: l.posting === "Credit" ? Number(l.amount) : 0,
        entity,
      }));
    }
    default:
      return [...debitExpenseLines(), { account: "2000", credit: lineTotalCents / 100, entity }];
  }
}

function enforceClassRule(lines) {
  for (const l of lines) {
    const account = accountOrThrow(l.account);
    const needsClass = classificationNeedsClass(account.classification);
    if (needsClass && !l.class) {
      throw new LedgerFixtureError(`account ${l.account} needs a class (income/COGS/expense line)`);
    }
    if (!needsClass && l.class) {
      throw new LedgerFixtureError(`account ${l.account} is a balance-sheet account and may not carry a class`);
    }
  }
}

function parseAdjustmentLines(rawLines) {
  if (!Array.isArray(rawLines) || rawLines.length === 0) {
    throw new LedgerFixtureError('"lines" must be a non-empty array');
  }
  const lines = rawLines.map((raw, i) => {
    if (!raw.account) throw new LedgerFixtureError(`line ${i + 1}: missing "account"`);
    const hasDebit = raw.debit != null;
    const hasCredit = raw.credit != null;
    if (hasDebit === hasCredit) {
      throw new LedgerFixtureError(`line ${i + 1}: exactly one of "debit" or "credit" is required`);
    }
    return {
      account: raw.account, class: raw.class ?? null,
      debit: hasDebit ? Number(raw.debit) : 0, credit: hasCredit ? Number(raw.credit) : 0,
      memo: raw.memo ?? null, entity: raw.entity ?? null,
    };
  });
  enforceClassRule(lines);
  const debitTotal = lines.reduce((s, l) => s + toCents(l.debit), 0);
  const creditTotal = lines.reduce((s, l) => s + toCents(l.credit), 0);
  if (debitTotal !== creditTotal) {
    throw new LedgerFixtureError(`lines do not balance: debits ${fromCents(debitTotal)} != credits ${fromCents(creditTotal)}`);
  }
  return lines;
}

// ---------------------------------------------------------------------------
// seed data builders — invented the same way `js/data/fixture.js`'s
// documents are. Called once per `createFixtureProvider()` so every book is
// independent (see the module doc comment above).
// ---------------------------------------------------------------------------

function seedJournal() {
  const BLUE_HARBOR = { kind: "customer", id: "cust-blue-harbor" };
  const FAIRVIEW = { kind: "customer", id: "cust-fairview" };
  const HARBORVIEW = { kind: "customer", id: "cust-harborview" };
  const SPECTRA = { kind: "vendor", id: "vend-spectra" };

  return [
    buildEntry({
      id: "je-1000", date: "2026-01-05", memo: "Opening balances",
      sourceType: "JournalEntry", sourceId: "opening-2026", isFlagged: true,
      actor: "import", commandKind: "import_replay", at: "2026-01-05T09:00:00Z",
      lines: [
        { account: "1100", debit: 42000.00 },
        { account: "1200", debit: 18500.00 },
        { account: "1300", debit: 21000.00 },
        { account: "1310", debit: 15500.00 },
        { account: "2000", credit: 9200.00 },
        { account: "2200", credit: 1100.00 },
        { account: "3900", credit: 86700.00 },
      ],
    }),
    buildEntry({
      id: "je-1001", date: "2026-07-14", memo: "Invoice 21234 — Blue Harbor Swim Club",
      sourceType: "Invoice", sourceId: "inv-2001",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-07-14T15:02:00Z",
      lines: [
        { account: "1200", debit: 159.94, entity: BLUE_HARBOR },
        { account: "4100", credit: 150.00, class: "foam", is_taxable: true, tax_amount: 9.94, tax_rate: "0.06625", entity: BLUE_HARBOR },
        { account: "2200", credit: 9.94, entity: BLUE_HARBOR },
      ],
    }),
    buildEntry({
      id: "je-1002", date: "2026-08-04", memo: "Invoice 21230 — Fairview County Parks",
      sourceType: "Invoice", sourceId: "inv-2002",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-08-04T11:00:00Z",
      lines: [
        { account: "1200", debit: 2990.00, entity: FAIRVIEW },
        { account: "4100", credit: 2990.00, class: "chairs", is_taxable: false, entity: FAIRVIEW },
      ],
    }),
    buildEntry({
      id: "je-1003", date: "2026-08-10", memo: "Bill BILL-2288 — Spectra EVA",
      sourceType: "Bill", sourceId: "bill-3001",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-08-10T09:30:00Z",
      lines: [
        { account: "1300", debit: 2776.80, entity: SPECTRA },
        { account: "2000", credit: 2776.80, entity: SPECTRA },
      ],
    }),
    buildEntry({
      id: "je-1004", date: "2026-08-15", memo: "Payment received — Blue Harbor Swim Club",
      sourceType: "Payment", sourceId: "pmt-4001",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-08-15T10:00:00Z",
      lines: [
        { account: "1100", debit: 159.94, entity: BLUE_HARBOR },
        { account: "1200", credit: 159.94, entity: BLUE_HARBOR },
      ],
    }),
    buildEntry({
      id: "je-1005", date: "2026-08-18", memo: "Bill payment — Spectra EVA",
      sourceType: "BillPayment", sourceId: "billpmt-5001",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-08-18T14:00:00Z",
      lines: [
        { account: "2000", debit: 2776.80, entity: SPECTRA },
        { account: "1100", credit: 2776.80, entity: SPECTRA },
      ],
    }),
    buildEntry({
      id: "je-1006", date: "2026-07-25", memo: "Reclassify July shop supplies from Checking",
      sourceType: "JournalEntry", sourceId: "adj-9001", isFlagged: true,
      actor: "dan", commandKind: "mcp_decide_adjustment", at: "2026-07-25T16:00:00Z",
      lines: [
        { account: "6100", debit: 125.00, class: "foam" },
        { account: "1100", credit: 125.00 },
      ],
    }),
    buildEntry({
      id: "je-1007", date: "2026-09-02", memo: "Invoice 21231 — Harborview Signs",
      sourceType: "Invoice", sourceId: "inv-2003",
      actor: "dan", commandKind: "mcp_save_document", at: "2026-09-02T13:15:00Z",
      lines: [
        { account: "1200", debit: 426.50, entity: HARBORVIEW },
        { account: "4100", credit: 400.00, class: "signs", is_taxable: true, tax_amount: 26.50, tax_rate: "0.06625", entity: HARBORVIEW },
        { account: "2200", credit: 26.50, entity: HARBORVIEW },
      ],
    }),
  ];
}

/** `document_id` -> saved version count, so a repeated `save_and_post_document`
 * in one session returns an incrementing `version` the way `Ledger::save_document`
 * would. */
function seedDocumentVersions() {
  return new Map(
    ["inv-2001", "inv-2002", "bill-3001", "pmt-4001", "billpmt-5001", "inv-2003"].map((id) => [id, 1]),
  );
}

/** `docs/ACCOUNTANT.md`'s "how a request flows" — one posted, one still
 * proposed, one rejected. */
function seedAdjustments() {
  return [
    {
      request_id: "adj-9001", requested_by: "joel", requested_at: "2026-07-24T10:00:00Z",
      description: "Reclassify July shop supplies from Checking",
      effective_date: "2026-07-25",
      lines: [
        { account: "6100", class: "foam", debit: 125.00 },
        { account: "1100", credit: 125.00 },
      ],
      state: "posted", decided_by: "dan", decided_at: "2026-07-25T16:00:00Z",
      decision_note: "agreed", posted_entry_id: "je-1006",
    },
    {
      request_id: "adj-9002", requested_by: "joel", requested_at: "2026-09-05T09:00:00Z",
      description: "Move August channel fees from other operating to advertising",
      effective_date: "2026-09-05",
      lines: [
        { account: "6600", class: "foam", debit: 64.30 },
        { account: "6900", class: "foam", credit: 64.30 },
      ],
      state: "proposed", decided_by: null, decided_at: null, decision_note: null, posted_entry_id: null,
    },
    {
      request_id: "adj-9003", requested_by: "joel", requested_at: "2026-07-20T09:00:00Z",
      description: "Correct duplicate freight charge",
      effective_date: "2026-07-20",
      lines: [
        { account: "1100", debit: 45.00 },
        { account: "6900", class: "foam", credit: 45.00 },
      ],
      state: "rejected", decided_by: "dan", decided_at: "2026-07-21T09:00:00Z",
      decision_note: "Not a duplicate — two separate shipments", posted_entry_id: null,
    },
  ];
}

/** `docs/BANK.md` — one statement, five lines: three resolved (exact), one
 * proposed, one fully unmatched. `opening_balance + total(all lines) ==
 * closing_balance` by construction, so the reconciliation screen's
 * difference reads exactly as the unresolved lines' own sum until they're
 * confirmed. */
function seedBankStatement() {
  return {
    statement_id: "stmt-2026-08",
    account_id: "1100",
    period_start: "2026-08-01",
    period_end: "2026-08-31",
    opening_balance_cents: toCents(41288.12),
    closing_balance_cents: toCents(41380.96),
    imported_at: "2026-09-01T08:00:00Z",
    closed_at: null,
  };
}

function seedBankLines() {
  return [
    { line_id: "bl-1", posted_on: "2026-08-05", amount: 2899.00, description: "DEPOSIT ACH NORTHGATE AQUATIC", external_id: "chase-0805-1", match_kind: "exact", matched_entry_id: "je-1002", matched_line_no: 1, matched_at: "2026-09-01T08:05:00Z" },
    { line_id: "bl-2", posted_on: "2026-08-12", amount: -2776.80, description: "ACH DEBIT SPECTRA EVA", external_id: "chase-0812-1", match_kind: "exact", matched_entry_id: "je-1005", matched_line_no: 2, matched_at: "2026-09-01T08:05:00Z" },
    { line_id: "bl-3", posted_on: "2026-08-18", amount: -125.00, description: "SQ *SHOP SUPPLY CO", external_id: "chase-0818-1", match_kind: "proposed", matched_entry_id: null, matched_line_no: null, matched_at: null },
    { line_id: "bl-4", posted_on: "2026-08-22", amount: -64.30, description: "FACEBK ADS 90210", external_id: "chase-0822-1", match_kind: null, matched_entry_id: null, matched_line_no: null, matched_at: null },
    { line_id: "bl-5", posted_on: "2026-08-27", amount: 159.94, description: "DEPOSIT ACH BLUE HARBOR SWIM", external_id: "chase-0827-1", match_kind: "exact", matched_entry_id: "je-1004", matched_line_no: 1, matched_at: "2026-09-01T08:05:00Z" },
  ];
}

// ---------------------------------------------------------------------------
// one book: every stateful read and write, closing over this call's own
// mutable data so two `createFixtureProvider()` calls never see each
// other's posted entries — the same isolation two `ledger-mcp` processes
// over two different `.sqlite` files would have.
// ---------------------------------------------------------------------------

function createBook() {
  const journal = seedJournal();
  const adjustments = seedAdjustments();
  const bankStatement = seedBankStatement();
  const bankLines = seedBankLines();
  const documentVersions = seedDocumentVersions();
  let nextEntrySeq = 1008; // seeded entries use je-1000..je-1007
  let nextRequestSeq = 9004; // seeded adjustments use adj-9001..adj-9003
  let lockedThrough = "2026-07-31";

  function allLinesWithEntry() {
    const out = [];
    for (const entry of journal) {
      for (const line of entry.lines) out.push({ entry, line });
    }
    return out;
  }

  function adjustmentJson(request) {
    return {
      request_id: request.request_id,
      requested_by: request.requested_by,
      requested_at: request.requested_at,
      description: request.description,
      lines: request.lines.map((l, i) => journalLineJson({
        line_no: i + 1,
        account: l.account,
        class: l.class ?? null,
        debit_cents: toCents(l.debit ?? 0),
        credit_cents: toCents(l.credit ?? 0),
        memo: l.memo ?? null,
        entity: l.entity ?? null,
        is_taxable: false,
        tax_amount_cents: null,
        tax_rate: null,
      })),
      state: request.state,
      decided_by: request.decided_by,
      decided_at: request.decided_at,
      decision_note: request.decision_note,
      posted_entry_id: request.posted_entry_id,
      // Recomputed against the *current* locked_through every time this is
      // rendered (`docs/ACCOUNTANT.md`: "not stored ... since a period that
      // was open when the request was made can close before Dan decides it").
      period_closed: request.state === "proposed" && lockedThrough != null && request.effective_date <= lockedThrough,
    };
  }

  function bankStatementJson() {
    return {
      statement_id: bankStatement.statement_id,
      account_id: bankStatement.account_id,
      period_start: bankStatement.period_start,
      period_end: bankStatement.period_end,
      opening_balance: fromCents(bankStatement.opening_balance_cents),
      closing_balance: fromCents(bankStatement.closing_balance_cents),
      imported_at: bankStatement.imported_at,
      closed_at: bankStatement.closed_at,
    };
  }

  function bankLineJson(line) {
    return {
      line_id: line.line_id,
      statement_id: bankStatement.statement_id,
      account_id: bankStatement.account_id,
      posted_on: line.posted_on,
      amount: fromCents(toCents(line.amount)),
      description: line.description,
      external_id: line.external_id,
      matched_entry_id: line.matched_entry_id,
      matched_line_no: line.matched_line_no,
      match_kind: line.match_kind,
      matched_at: line.matched_at,
    };
  }

  // --- reports ---------------------------------------------------------

  function trialBalance(asOf) {
    const totals = new Map(ACCOUNTS.map((a) => [a.number, { debit: 0, credit: 0 }]));
    for (const { entry, line } of allLinesWithEntry()) {
      if (entry.entry_date > asOf) continue;
      const t = totals.get(line.account);
      if (!t) continue;
      t.debit += line.debit_cents;
      t.credit += line.credit_cents;
    }
    let totalDebits = 0;
    let totalCredits = 0;
    const wrongSide = [];
    const rows = ACCOUNTS.map((a) => {
      const t = totals.get(a.number);
      const balance = t.debit - t.credit;
      totalDebits += t.debit;
      totalCredits += t.credit;
      if (!a.is_contra && balance !== 0) {
        const normalDebit = NORMAL_SIDE_IS_DEBIT[a.classification];
        const wrong = normalDebit ? balance < 0 : balance > 0;
        if (wrong) wrongSide.push(a.number);
      }
      return {
        account_id: a.number, number: a.number, name: a.name, classification: a.classification,
        is_contra: a.is_contra, needs_mapping: a.needs_mapping, source_ref: a.source_ref,
        debit: fromCents(t.debit), credit: fromCents(t.credit), balance: fromCents(balance),
      };
    });
    return { rows, total_debits: fromCents(totalDebits), total_credits: fromCents(totalCredits), wrong_side: wrongSide };
  }

  /** `report.rs::profit_and_loss` — income on its normal (credit) side, COGS
   * and expense on theirs (debit), so a contra account correctly reduces its
   * section's total. */
  function profitAndLoss(from, to) {
    const sections = {
      income: { by_account: new Map(), by_class: new Map(), total: 0 },
      cogs: { by_account: new Map(), by_class: new Map(), total: 0 },
      expense: { by_account: new Map(), by_class: new Map(), total: 0 },
    };
    for (const { entry, line } of allLinesWithEntry()) {
      if (entry.entry_date < from || entry.entry_date > to) continue;
      const account = accountOrThrow(line.account);
      let sectionKey;
      let amount;
      if (account.classification === "Income") {
        sectionKey = "income";
        amount = line.credit_cents - line.debit_cents;
      } else if (account.classification === "COGS") {
        sectionKey = "cogs";
        amount = line.debit_cents - line.credit_cents;
      } else if (account.classification === "Expense") {
        sectionKey = "expense";
        amount = line.debit_cents - line.credit_cents;
      } else {
        continue;
      }
      const section = sections[sectionKey];
      section.by_account.set(account.number, (section.by_account.get(account.number) ?? 0) + amount);
      section.by_class.set(line.class ?? null, (section.by_class.get(line.class ?? null) ?? 0) + amount);
      section.total += amount;
    }
    function finish(section) {
      return {
        by_account: [...section.by_account.entries()].sort(([a], [b]) => a.localeCompare(b))
          .map(([account_id, cents]) => ({ account_id, amount: fromCents(cents) })),
        by_class: [...section.by_class.entries()]
          .map(([class_id, cents]) => ({ class_id, amount: fromCents(cents) })),
        total: fromCents(section.total),
      };
    }
    const income = finish(sections.income);
    const cogs = finish(sections.cogs);
    const expense = finish(sections.expense);
    const grossMargin = sections.income.total - sections.cogs.total;
    const netIncome = grossMargin - sections.expense.total;
    return { income, cogs, expense, gross_margin: fromCents(grossMargin), net_income: fromCents(netIncome) };
  }

  function netIncomeCentsForYear(asOf) {
    const yearStart = `${asOf.slice(0, 4)}-01-01`;
    const pnl = profitAndLoss(yearStart, asOf);
    return toCents(pnl.net_income);
  }

  function balanceSheet(asOf) {
    const assets = { rows: [], total: 0 };
    const liabilities = { rows: [], total: 0 };
    const equity = { rows: [], total: 0 };
    const totals = new Map(ACCOUNTS.map((a) => [a.number, { debit: 0, credit: 0 }]));
    for (const { entry, line } of allLinesWithEntry()) {
      if (entry.entry_date > asOf) continue;
      const t = totals.get(line.account);
      if (!t) continue;
      t.debit += line.debit_cents;
      t.credit += line.credit_cents;
    }
    for (const a of ACCOUNTS) {
      const t = totals.get(a.number);
      let section;
      let amount;
      if (a.classification === "Asset") {
        section = assets; amount = t.debit - t.credit;
      } else if (a.classification === "Liability") {
        section = liabilities; amount = t.credit - t.debit;
      } else if (a.classification === "Equity") {
        section = equity; amount = t.credit - t.debit;
      } else {
        continue;
      }
      if (amount === 0) continue;
      section.total += amount;
      section.rows.push({ account_id: a.number, name: a.name, balance: fromCents(amount) });
    }
    const netIncome = netIncomeCentsForYear(asOf);
    equity.total += netIncome;
    equity.rows.push({ account_id: null, name: "Net income (current year)", balance: fromCents(netIncome) });
    const totalLiabAndEquity = liabilities.total + equity.total;
    return {
      assets: { rows: assets.rows, total: fromCents(assets.total) },
      liabilities: { rows: liabilities.rows, total: fromCents(liabilities.total) },
      equity: { rows: equity.rows, total: fromCents(equity.total) },
      total_liabilities_and_equity: fromCents(totalLiabAndEquity),
    };
  }

  /** `report.rs`'s quarter bounds — `(year, 1..=4)` to `[from, to]` inclusive. */
  function quarterBounds(year, quarter) {
    const starts = ["01-01", "04-01", "07-01", "10-01"];
    const ends = ["03-31", "06-30", "09-30", "12-31"];
    return [`${year}-${starts[quarter - 1]}`, `${year}-${ends[quarter - 1]}`];
  }

  /** `report.rs::sales_tax_lines`, §9: A from the P&L income total, B off the
   * 2200 tax account, C = B / rate, D = A - C, E the line-level taxable
   * check, variance = E - C. `st50` is literally `[(1, A), (2, D), (3, C)]`
   * (`LEDGER-DESIGN.md` §9's filing-mapping table). */
  function salesTaxLines(year, quarter, rate = 0.06625) {
    const [from, to] = quarterBounds(year, quarter);
    const a = toCents(profitAndLoss(from, to).income.total);
    let b = 0;
    let e = 0;
    for (const { entry, line } of allLinesWithEntry()) {
      if (entry.entry_date < from || entry.entry_date > to) continue;
      if (line.account === "2200") b += line.credit_cents - line.debit_cents;
      const account = ACCOUNTS_BY_NUMBER.get(line.account);
      if (account?.classification === "Income" && line.is_taxable) e += line.credit_cents - line.debit_cents;
    }
    const c = Math.round(b / rate);
    const d = a - c;
    const variance = e - c;
    return {
      a_total_income: fromCents(a),
      b_tax_collected: fromCents(b),
      c_taxable_sales: fromCents(c),
      d_nontaxable_sales: fromCents(d),
      e_line_level_taxable: fromCents(e),
      variance: fromCents(variance),
      st50: [
        { line: 1, amount: fromCents(a) },
        { line: 2, amount: fromCents(d) },
        { line: 3, amount: fromCents(c) },
      ],
    };
  }

  /** `accountant.rs::general_ledger` — every posted line in `[from, to]` per
   * account, opening balance carried in from everything before `from`. */
  function generalLedger(from, to, account) {
    const numbers = account ? [account] : ACCOUNTS.map((a) => a.number);
    const sections = numbers.map((number) => {
      const acct = accountOrThrow(number);
      let opening = 0;
      const inRange = [];
      for (const { entry, line } of allLinesWithEntry()) {
        if (line.account !== number) continue;
        if (entry.entry_date < from) {
          opening += line.debit_cents - line.credit_cents;
        } else if (entry.entry_date <= to) {
          inRange.push({ entry, line });
        }
      }
      inRange.sort((x, y) => (x.entry.entry_date < y.entry.entry_date ? -1
        : x.entry.entry_date > y.entry.entry_date ? 1
          : x.entry.entry_id.localeCompare(y.entry.entry_id)));
      let running = opening;
      const lines = inRange.map(({ entry, line }) => {
        running += line.debit_cents - line.credit_cents;
        return {
          entry_id: entry.entry_id, entry_date: entry.entry_date, source_type: entry.source_type,
          source_id: entry.source_id, memo: entry.memo, class: line.class, entity: line.entity,
          is_flagged: entry.is_flagged, reversal_of: entry.reversal_of,
          debit: fromCents(line.debit_cents), credit: fromCents(line.credit_cents),
          running_balance: fromCents(running),
        };
      });
      return {
        account_id: acct.number, number: acct.number, name: acct.name,
        opening_balance: fromCents(opening), lines, closing_balance: fromCents(running),
      };
    });
    return { sections };
  }

  /** `accountant.rs::audit_trail` — every posting since `since` (default the
   * current `locked_through`), flagged entries first. */
  function auditTrail(since) {
    const cutoff = since ?? lockedThrough ?? "0000-01-01";
    return journal
      .filter((e) => e.entry_date > cutoff)
      .sort((a, b) => {
        if (a.is_flagged !== b.is_flagged) return a.is_flagged ? -1 : 1;
        return a.entry_date < b.entry_date ? 1 : a.entry_date > b.entry_date ? -1 : 0;
      })
      .map((e) => ({
        entry_id: e.entry_id, entry_date: e.entry_date, source_type: e.source_type,
        source_id: e.source_id, source_version: e.source_version, memo: e.memo,
        is_flagged: e.is_flagged, reversal_of: e.reversal_of, actor: e.actor,
        command_kind: e.command_kind, at: e.at,
      }));
  }

  function entriesForDocument(documentId) {
    return journal
      .filter((e) => e.source_id === documentId)
      .map((e) => ({ entry_id: e.entry_id, entry: journalEntryJson(e), is_posted: true }));
  }

  // --- writes ------------------------------------------------------------

  function withLockedThrough(value) {
    return { ...value, locked_through: lockedThrough };
  }

  function refuseIfClosed(date, label = "entry") {
    if (lockedThrough != null && date <= lockedThrough) {
      throw new LedgerFixtureError(
        `period closed through ${lockedThrough}: cannot post this ${label} on or before this date`,
      );
    }
  }

  function saveAndPostDocument(document, actor, items) {
    if (!document || typeof document !== "object") throw new LedgerFixtureError('missing "document"');
    const { document_id: documentId, kind, txn_date: txnDate } = document;
    if (!documentId) throw new LedgerFixtureError('missing "document.document_id"');
    if (!kind) throw new LedgerFixtureError('missing "document.kind"');
    if (!txnDate) throw new LedgerFixtureError('missing "document.txn_date"');

    const version = (documentVersions.get(documentId) ?? 0) + 1;

    if (kind === "Estimate" || kind === "PurchaseOrder") {
      documentVersions.set(documentId, version);
      return withLockedThrough({ document_id: documentId, version, entry_id: null, entry: null });
    }

    refuseIfClosed(txnDate, "document");
    const rawLines = deriveEntryLines(document, items);
    const balanced = rawLines.map((l) => ({ ...l, debit: l.debit ?? 0, credit: l.credit ?? 0 }));
    enforceClassRule(balanced);
    const debitTotal = balanced.reduce((s, l) => s + toCents(l.debit), 0);
    const creditTotal = balanced.reduce((s, l) => s + toCents(l.credit), 0);
    if (debitTotal !== creditTotal) {
      throw new LedgerFixtureError(`unbalanced journal: debits ${fromCents(debitTotal)} != credits ${fromCents(creditTotal)}`);
    }

    const entry = buildEntry({
      id: `je-${nextEntrySeq++}`, date: txnDate, memo: document.memo ?? `${kind} ${documentId}`,
      sourceType: kind, sourceId: documentId, sourceVersion: version, isFlagged: false,
      actor, commandKind: "mcp_save_document", at: new Date().toISOString(),
      lines: balanced,
    });
    journal.push(entry);
    documentVersions.set(documentId, version);
    return withLockedThrough({ document_id: documentId, version, entry_id: entry.entry_id, entry: journalEntryJson(entry) });
  }

  function reverseEntry(entryId, on, actor) {
    const original = journal.find((e) => e.entry_id === entryId);
    if (!original) throw new LedgerFixtureError(`no posted entry ${JSON.stringify(entryId)}`);
    refuseIfClosed(on, "reversal");
    const reversal = buildEntry({
      id: `je-${nextEntrySeq++}`, date: on, memo: `Reversal of ${original.memo}`,
      sourceType: original.source_type, sourceId: original.source_id, sourceVersion: original.source_version,
      reversalOf: entryId, isFlagged: true, actor, commandKind: "mcp_reverse_entry", at: new Date().toISOString(),
      lines: original.lines.map((l) => ({
        account: l.account, class: l.class, entity: l.entity,
        debit: l.credit_cents / 100, credit: l.debit_cents / 100,
      })),
    });
    journal.push(reversal);
    return withLockedThrough({ entry_id: reversal.entry_id, entry: journalEntryJson(reversal), actor });
  }

  function proposeAdjustment(requestedBy, description, rawLines) {
    if (!requestedBy) throw new LedgerFixtureError('missing "requested_by"');
    if (!description) throw new LedgerFixtureError('missing "description"');
    const lines = parseAdjustmentLines(rawLines);
    const now = new Date();
    const request = {
      request_id: `adj-${nextRequestSeq++}`, requested_by: requestedBy,
      requested_at: now.toISOString(), description, effective_date: now.toISOString().slice(0, 10),
      lines, state: "proposed", decided_by: null, decided_at: null, decision_note: null, posted_entry_id: null,
    };
    adjustments.push(request);
    return withLockedThrough(adjustmentJson(request));
  }

  function decideAdjustment(requestId, decidedBy, approve, note = "") {
    const request = adjustments.find((r) => r.request_id === requestId);
    if (!request) throw new LedgerFixtureError(`no adjustment request ${JSON.stringify(requestId)}`);
    if (request.state !== "proposed") {
      throw new LedgerFixtureError(`request ${requestId} has already been decided (state: ${request.state})`);
    }
    if (!approve) {
      request.state = "rejected";
      request.decided_by = decidedBy;
      request.decided_at = new Date().toISOString();
      request.decision_note = note;
      return withLockedThrough(adjustmentJson(request));
    }
    refuseIfClosed(request.effective_date, "adjustment");
    const entry = buildEntry({
      id: `je-${nextEntrySeq++}`, date: request.effective_date, memo: request.description,
      sourceType: "JournalEntry", sourceId: request.request_id, isFlagged: true,
      actor: decidedBy, commandKind: "mcp_decide_adjustment", at: new Date().toISOString(),
      lines: request.lines,
    });
    journal.push(entry);
    request.state = "posted";
    request.decided_by = decidedBy;
    request.decided_at = new Date().toISOString();
    request.decision_note = note;
    request.posted_entry_id = entry.entry_id;
    return withLockedThrough(adjustmentJson(request));
  }

  function bankConfirmProposal(lineId, account, cls, actor) {
    const line = bankLines.find((l) => l.line_id === lineId);
    if (!line) throw new LedgerFixtureError(`no bank line ${JSON.stringify(lineId)}`);
    if (line.match_kind !== "proposed") {
      throw new LedgerFixtureError(`bank line ${lineId} is not currently a pending proposal`);
    }
    const acct = accountOrThrow(account);
    if (classificationNeedsClass(acct.classification) && !cls) {
      throw new LedgerFixtureError(`account ${account} needs a class`);
    }
    const isCredit = line.amount < 0; // bank-out: debit the chosen account, credit checking
    const amount = Math.abs(line.amount);
    const entry = buildEntry({
      id: `je-${nextEntrySeq++}`, date: line.posted_on, memo: line.description,
      sourceType: "BankLine", sourceId: line.line_id, isFlagged: false,
      actor, commandKind: "mcp_bank_confirm_proposal", at: new Date().toISOString(),
      lines: isCredit
        ? [{ account, class: cls ?? null, debit: amount }, { account: "1100", credit: amount }]
        : [{ account: "1100", debit: amount }, { account, class: cls ?? null, credit: amount }],
    });
    journal.push(entry);
    line.match_kind = "manual";
    line.matched_entry_id = entry.entry_id;
    line.matched_line_no = isCredit ? 1 : 2;
    line.matched_at = new Date().toISOString();
    return withLockedThrough({ entry_id: entry.entry_id, actor });
  }

  function closePeriod(periodEnd) {
    if (lockedThrough != null && periodEnd <= lockedThrough) {
      throw new LedgerFixtureError(`period_end ${periodEnd} must be after the current locked_through ${lockedThrough}`);
    }
    lockedThrough = periodEnd;
    return withLockedThrough({ period_end: periodEnd });
  }

  function reopenPeriod(periodEnd) {
    if (lockedThrough == null || periodEnd !== lockedThrough) {
      throw new LedgerFixtureError(`${periodEnd} was never closed (locked through ${lockedThrough ?? "nothing"})`);
    }
    // §5/D7: always loud — a real reopen always writes a close_history row
    // with is_reopen=1 regardless of `note`; this fixture has no separate
    // close-history tool to write into, so the loudness is the caller's own
    // job (`screens/ledger/periodClose.js`'s red banner), not this function's.
    lockedThrough = null;
    return withLockedThrough({ period_end: periodEnd, reopened: true });
  }

  // --- dispatch ----------------------------------------------------------

  return async function callTool(tool, args) {
    switch (tool) {
      case "trial_balance":
        return trialBalance(args.as_of);
      case "profit_and_loss":
        return profitAndLoss(args.from, args.to);
      case "balance_sheet":
        return balanceSheet(args.as_of);
      case "sales_tax_lines":
        return salesTaxLines(args.year, args.quarter, args.rate != null ? Number(args.rate) : undefined);
      case "general_ledger":
        return generalLedger(args.from, args.to, args.account);
      case "audit_trail":
        return auditTrail(args.since);
      case "entries_for_document":
        return entriesForDocument(args.document_id);
      case "list_adjustments":
        return adjustments.filter((r) => !args.state || r.state === args.state).map(adjustmentJson);
      case "bank_status": {
        if (args.statement_id !== bankStatement.statement_id) {
          throw new LedgerFixtureError(`no bank statement ${JSON.stringify(args.statement_id)}`);
        }
        const lines = bankLines.map(bankLineJson);
        const proposed = lines.filter((l) => l.match_kind === "proposed").length;
        const unmatched = lines.filter((l) => l.match_kind == null).length;
        const matched = lines.length - proposed - unmatched;
        return { statement: bankStatementJson(), lines, summary: { matched, proposed, unmatched } };
      }
      case "locked_through":
        return { locked_through: lockedThrough };
      case "save_and_post_document":
        return saveAndPostDocument(args.document, args.actor, args.items);
      case "reverse_entry":
        return reverseEntry(args.entry_id, args.on, args.actor);
      case "propose_adjustment":
        return proposeAdjustment(args.requested_by, args.description, args.lines);
      case "decide_adjustment":
        return decideAdjustment(args.request_id, args.decided_by, args.approve, args.note);
      case "bank_confirm_proposal":
        return bankConfirmProposal(args.line_id, args.account, args.class, args.actor);
      case "close_period":
        return closePeriod(args.period_end, args.actor, args.note);
      case "reopen_period":
        return reopenPeriod(args.period_end, args.actor, args.note);
      default:
        throw new LedgerFixtureError(`unknown tool: ${tool}`);
    }
  };
}

/** The provider backed by a fresh in-memory book (see the module doc
 * comment: every call gets its own, independent of any other). */
export function createFixtureProvider() {
  return createProvider(createBook());
}

/** Test-only escape hatch: the account chart, for a screen or test that
 * wants to label a number without a round trip through a report. Not part
 * of the provider contract — nothing under `js/screens/` should import this
 * module directly. */
export const __ACCOUNTS_FOR_TESTS__ = ACCOUNTS;
