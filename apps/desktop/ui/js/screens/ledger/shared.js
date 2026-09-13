// Small helpers shared by the nine ledger screens: date defaults, a couple
// of presentational tags, and the pure calculations the task's own tests
// name explicitly (the ST-50 filing mapping and the reconciliation
// difference) — pure functions, no DOM, so they're testable head-on from
// `node --test` the same way `js/data/ledger/fixture.js`'s report math is.

import { esc } from "../../format.js";

export function todayIso() {
  return new Date().toISOString().slice(0, 10);
}

/** The calendar quarter containing `isoDate`, as `{ year, quarter }` —
 * `report.rs`'s own `(year, 1..=4)` shape, for the sales-tax screen's
 * default quarter picker. */
export function quarterOf(isoDate) {
  const [year, month] = isoDate.split("-").map(Number);
  return { year, quarter: Math.ceil(month / 3) };
}

export function stateTag(state) {
  const cls = state === "posted" ? "ok" : state === "rejected" ? "crit" : state === "approved" ? "acc" : "warn";
  return `<span class="tag ${cls}">${esc(state)}</span>`;
}

export function matchKindTag(kind) {
  if (kind === "exact" || kind === "settlement") return `<span class="tag ok">${esc(kind)}</span>`;
  if (kind === "manual") return `<span class="tag acc">manual</span>`;
  if (kind === "proposed") return `<span class="tag warn">proposed</span>`;
  return `<span class="tag crit">unmatched</span>`;
}

/**
 * `LEDGER-DESIGN.md` §9's filing-mapping table, turned into the printed
 * ST-50 form's rows: line 1 is gross receipts (line A of the report), line
 * 2 is receipts not subject to tax (line D), line 3 is receipts subject to
 * tax (line C), and lines 4 through 9 have never carried an entry (11 Sep
 * 2026) so they print as zero. Takes the whole `sales_tax_lines` result and
 * reads `a_total_income`/`c_taxable_sales`/`d_nontaxable_sales` directly
 * rather than trusting the API's own `st50` field, so this mapping is
 * checked independently of whatever the server already computed.
 * @param {{ a_total_income: string, c_taxable_sales: string, d_nontaxable_sales: string }} result
 */
export function st50Rows(result) {
  return [
    { line: 1, label: "Gross receipts", source: "A", amount: result.a_total_income },
    { line: 2, label: "Receipts not subject to tax", source: "D", amount: result.d_nontaxable_sales },
    { line: 3, label: "Receipts subject to tax", source: "C", amount: result.c_taxable_sales },
    ...Array.from({ length: 6 }, (_, i) => ({ line: i + 4, label: "—", source: null, amount: "0.00" })),
  ];
}

function toCents(decimal) {
  if (decimal == null) return 0;
  return Math.round(Number(decimal) * 100);
}

function fromCents(cents) {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(cents);
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, "0")}`;
}

/**
 * `docs/BANK.md`'s close rule: a statement closes when `opening_balance +
 * sum(amounts of every matched line) == closing_balance`. This is the live
 * left-hand side compared against the statement's own `closing_balance` —
 * the difference the reconciliation screen shows as it is worked down to
 * zero. "Matched" here means resolved one way or another (`exact`,
 * `settlement`, or a confirmed `manual`) — a `proposed` line is a
 * suggestion, not yet a match, and an unmatched line (`null`) plainly isn't
 * one either, so both are left out of the sum on purpose.
 * @param {{ opening_balance: string, closing_balance: string }} statement
 * @param {{ amount: string, match_kind: string | null }[]} lines
 * @returns {string} signed decimal string; "0.00" once every line is resolved
 */
export function reconciliationDifference(statement, lines) {
  const resolvedCents = lines
    .filter((l) => l.match_kind === "exact" || l.match_kind === "settlement" || l.match_kind === "manual")
    .reduce((sum, l) => sum + toCents(l.amount), 0);
  const expectedClosingCents = toCents(statement.opening_balance) + resolvedCents;
  return fromCents(toCents(statement.closing_balance) - expectedClosingCents);
}

/**
 * Parses the propose-adjustment textarea's line syntax,
 * `ACCOUNT:dr|cr:AMOUNT[:CLASS]`, one line per journal line — the shape
 * `apps/desktop/README.md`'s ledger section documents for this form. Throws
 * a plain `Error` naming the bad line (1-based) rather than silently
 * dropping it; the caller shows that message rather than posting a partial
 * request.
 * @param {string} text
 * @returns {{ account: string, debit?: string, credit?: string, class?: string }[]}
 */
export function parseAdjustmentLineSpecs(text) {
  const rows = text.split("\n").map((l) => l.trim()).filter((l) => l.length > 0);
  if (rows.length === 0) throw new Error("at least one line is required");
  return rows.map((row, i) => {
    const parts = row.split(":").map((p) => p.trim());
    const [account, side, amount, cls] = parts;
    if (!account || !side || !amount) {
      throw new Error(`line ${i + 1}: expected ACCOUNT:dr|cr:AMOUNT[:CLASS], got ${JSON.stringify(row)}`);
    }
    const sideLower = side.toLowerCase();
    if (sideLower !== "dr" && sideLower !== "cr") {
      throw new Error(`line ${i + 1}: side must be "dr" or "cr", got ${JSON.stringify(side)}`);
    }
    if (!/^\d+(\.\d{1,2})?$/.test(amount)) {
      throw new Error(`line ${i + 1}: amount must be a decimal like "125.00", got ${JSON.stringify(amount)}`);
    }
    const line = { account };
    if (sideLower === "dr") line.debit = amount;
    else line.credit = amount;
    if (cls) line.class = cls;
    return line;
  });
}
