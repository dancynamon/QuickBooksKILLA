// Pure-function tests for the two ledger screen calculations named directly
// in `apps/desktop/README.md`: the sales-tax screen's ST-50 filing mapping
// (`LEDGER-DESIGN.md` §9 — line 1 is A, line 2 is D, line 3 is C) and the
// bank-reconciliation screen's live difference (`docs/BANK.md`'s close
// rule, opening + matched = closing). Both live in
// `js/screens/ledger/shared.js` as plain functions with no DOM, so they're
// exercised head-on here rather than through a rendered screen — the same
// reasoning `test/fixture.test.js` gives for testing `fixture.js`'s
// behaviour, not just its shape.

import { test } from "node:test";
import assert from "node:assert/strict";

import { st50Rows, reconciliationDifference, parseAdjustmentLineSpecs, quarterOf } from "../js/screens/ledger/shared.js";

test("st50Rows maps line 1 to A, line 2 to D, line 3 to C, and zeroes lines 4-9", () => {
  const result = {
    a_total_income: "3540.00",
    b_tax_collected: "36.44",
    c_taxable_sales: "550.04",
    d_nontaxable_sales: "2989.96",
  };
  const rows = st50Rows(result);
  assert.equal(rows.length, 9);
  assert.deepEqual(rows[0], { line: 1, label: "Gross receipts", source: "A", amount: "3540.00" });
  assert.deepEqual(rows[1], { line: 2, label: "Receipts not subject to tax", source: "D", amount: "2989.96" });
  assert.deepEqual(rows[2], { line: 3, label: "Receipts subject to tax", source: "C", amount: "550.04" });
  for (const row of rows.slice(3)) {
    assert.equal(row.amount, "0.00");
    assert.equal(row.source, null);
  }
  assert.deepEqual(rows.slice(3).map((r) => r.line), [4, 5, 6, 7, 8, 9]);
});

test("reconciliationDifference is zero once every line is resolved and matches the stated closing balance", () => {
  const statement = { opening_balance: "41288.12", closing_balance: "41417.06" };
  const lines = [
    { amount: "2899.00", match_kind: "exact" },
    { amount: "-2776.80", match_kind: "settlement" },
    { amount: "6.74", match_kind: "manual" },
  ];
  assert.equal(reconciliationDifference(statement, lines), "0.00");
});

test("reconciliationDifference counts only resolved lines — proposed and unmatched sit outside the sum", () => {
  const statement = { opening_balance: "41288.12", closing_balance: "41380.96" };
  const lines = [
    { amount: "2899.00", match_kind: "exact" },
    { amount: "-2776.80", match_kind: "exact" },
    { amount: "159.94", match_kind: "exact" },
    { amount: "-125.00", match_kind: "proposed" },
    { amount: "-64.30", match_kind: null },
  ];
  // opening + matched(2899.00 - 2776.80 + 159.94) = 41288.12 + 282.14 = 41570.26
  // closing (41380.96) - 41570.26 = -189.30, exactly the unresolved lines' sum.
  assert.equal(reconciliationDifference(statement, lines), "-189.30");
});

test("reconciliationDifference handles an already-balanced statement with nothing to resolve", () => {
  const statement = { opening_balance: "0.00", closing_balance: "0.00" };
  assert.equal(reconciliationDifference(statement, []), "0.00");
});

test("parseAdjustmentLineSpecs reads ACCOUNT:dr|cr:AMOUNT[:CLASS] lines", () => {
  const lines = parseAdjustmentLineSpecs("6100:dr:125.00:foam\n1100:cr:125.00");
  assert.deepEqual(lines, [
    { account: "6100", debit: "125.00", class: "foam" },
    { account: "1100", credit: "125.00" },
  ]);
});

test("parseAdjustmentLineSpecs rejects a line missing a required part", () => {
  assert.throws(() => parseAdjustmentLineSpecs("6100:dr"), /ACCOUNT:dr\|cr:AMOUNT/);
});

test("parseAdjustmentLineSpecs rejects a side that isn't dr or cr", () => {
  assert.throws(() => parseAdjustmentLineSpecs("6100:debit:125.00"), /"dr" or "cr"/);
});

test("quarterOf reports the calendar quarter containing a date", () => {
  assert.deepEqual(quarterOf("2026-01-15"), { year: 2026, quarter: 1 });
  assert.deepEqual(quarterOf("2026-07-01"), { year: 2026, quarter: 3 });
  assert.deepEqual(quarterOf("2026-12-31"), { year: 2026, quarter: 4 });
});
