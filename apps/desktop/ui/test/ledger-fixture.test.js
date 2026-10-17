// Ledger schema contract test — the sibling of `test/fixture.test.js` for
// the ledger provider (`apps/desktop/README.md`'s ledger section). Asserts
// the fixture provider's output for all twenty-four `ledger-mcp` tools
// (`docs/MCP.md`) satisfies `js/data/ledger/schema.js` — derived from
// `apps/ledger/src/mcp.rs`'s own `_json` rendering functions — plus a few
// behavioural checks: the write gate refuses a write dated on or before
// `locked_through`, propose/decide moves the trial balance, and a bank
// confirm resolves its line.

import { test } from "node:test";
import assert from "node:assert/strict";

import { createFixtureProvider } from "../js/data/ledger/fixture.js";
import { SCHEMA, firstMissingField } from "../js/data/ledger/schema.js";

function assertRow(row, fields, label) {
  const missing = firstMissingField(row, fields);
  assert.equal(missing, null, `${label} is missing field ${missing}`);
}

function assertJournalEntry(entry, spec, label) {
  assertRow(entry, spec.entry ?? spec.top, `${label}`);
  assert.ok(Array.isArray(entry.lines), `${label}.lines should be an array`);
  for (const line of entry.lines) assertRow(line, spec.line, `${label}.lines[]`);
}

function assertWithLockedThrough(result, spec, label) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `${label} is missing top-level field ${missing}`);
  assert.ok("locked_through" in result, `${label} should carry locked_through`);
}

test("ledger fixture provider satisfies the schema for every read tool", async () => {
  const ledger = createFixtureProvider();

  const tb = await ledger.trialBalance("2026-09-30");
  assertRow(tb, SCHEMA.trial_balance.top, "trial_balance");
  assert.ok(tb.rows.length > 0, "trial_balance should have rows in this fixture");
  for (const row of tb.rows) assertRow(row, SCHEMA.trial_balance.row, "trial_balance.rows[]");
  assert.equal(tb.total_debits, tb.total_credits, "the fixture's own book should balance");

  const pnl = await ledger.profitAndLoss("2026-01-01", "2026-09-30");
  assertRow(pnl, SCHEMA.profit_and_loss.top, "profit_and_loss");
  for (const section of [pnl.income, pnl.cogs, pnl.expense]) {
    assertRow(section, SCHEMA.profit_and_loss.section, "profit_and_loss section");
    for (const r of section.by_account) assertRow(r, SCHEMA.profit_and_loss.byAccountRow, "by_account[]");
    for (const r of section.by_class) assertRow(r, SCHEMA.profit_and_loss.byClassRow, "by_class[]");
  }

  const bs = await ledger.balanceSheet("2026-09-30");
  assertRow(bs, SCHEMA.balance_sheet.top, "balance_sheet");
  for (const section of [bs.assets, bs.liabilities, bs.equity]) {
    assertRow(section, SCHEMA.balance_sheet.section, "balance_sheet section");
    for (const r of section.rows) assertRow(r, SCHEMA.balance_sheet.row, "balance_sheet row");
  }
  assert.equal(bs.assets.total, bs.total_liabilities_and_equity, "the balance sheet should balance");

  const tax = await ledger.salesTaxLines(2026, 3);
  assertRow(tax, SCHEMA.sales_tax_lines.top, "sales_tax_lines");
  assert.equal(tax.st50.length, 3);
  for (const row of tax.st50) assertRow(row, SCHEMA.sales_tax_lines.st50Row, "st50[]");
  assert.equal(tax.st50[0].amount, tax.a_total_income, "ST-50 line 1 should be line A");
  assert.equal(tax.st50[1].amount, tax.d_nontaxable_sales, "ST-50 line 2 should be line D");
  assert.equal(tax.st50[2].amount, tax.c_taxable_sales, "ST-50 line 3 should be line C");

  const gl = await ledger.generalLedger("2026-08-01", "2026-08-31");
  assertRow(gl, SCHEMA.general_ledger.top, "general_ledger");
  assert.ok(gl.sections.length > 0);
  for (const section of gl.sections) {
    assertRow(section, SCHEMA.general_ledger.section, "general_ledger section");
    for (const line of section.lines) assertRow(line, SCHEMA.general_ledger.line, "general_ledger line");
  }

  const audit = await ledger.auditTrail();
  assert.ok(Array.isArray(audit));
  assert.ok(audit.length > 0, "the fixture should have posted something since its own locked_through");
  for (const row of audit) assertRow(row, SCHEMA.audit_trail.row, "audit_trail[]");

  const entries = await ledger.entriesForDocument("inv-2001");
  assert.ok(Array.isArray(entries));
  assert.equal(entries.length, 1);
  for (const row of entries) {
    assertRow(row, SCHEMA.entries_for_document.row, "entries_for_document[]");
    assertJournalEntry(row.entry, SCHEMA.entries_for_document, "entries_for_document[].entry");
  }

  const adjustments = await ledger.listAdjustments();
  assert.ok(adjustments.length >= 3);
  for (const row of adjustments) {
    assertRow(row, SCHEMA.list_adjustments.row, "list_adjustments[]");
    for (const line of row.lines) assertRow(line, SCHEMA.list_adjustments.line, "list_adjustments[].lines[]");
  }
  assert.ok(adjustments.some((r) => r.state === "posted"));
  assert.ok(adjustments.some((r) => r.state === "proposed"));
  assert.ok(adjustments.some((r) => r.state === "rejected"));

  const bank = await ledger.bankStatus("stmt-2026-08");
  assertRow(bank, SCHEMA.bank_status.top, "bank_status");
  assertRow(bank.statement, SCHEMA.bank_status.statement, "bank_status.statement");
  for (const line of bank.lines) assertRow(line, SCHEMA.bank_status.line, "bank_status.lines[]");
  assertRow(bank.summary, SCHEMA.bank_status.summary, "bank_status.summary");
  assert.equal(bank.summary.matched + bank.summary.proposed + bank.summary.unmatched, bank.lines.length);

  const locked = await ledger.lockedThrough();
  assertRow(locked, SCHEMA.locked_through.top, "locked_through");
  assert.equal(locked.locked_through, "2026-07-31");
});

test("ledger fixture provider satisfies the schema for every write tool", async () => {
  const ledger = createFixtureProvider();

  const saved = await ledger.saveAndPostDocument(
    {
      document_id: "test-inv-1", kind: "Invoice", txn_date: "2026-09-10",
      contact: { kind: "Customer", id: "cust-test" },
      header_class: "foam",
      lines: [{ amount: "100.00", account: "4100", is_taxable: false }],
    },
    "dan",
    {},
  );
  assertWithLockedThrough(saved, SCHEMA.save_and_post_document, "save_and_post_document");
  assertJournalEntry(saved.entry, SCHEMA.save_and_post_document, "save_and_post_document.entry");

  const reversed = await ledger.reverseEntry(saved.entry_id, "2026-09-11", "dan");
  assertWithLockedThrough(reversed, SCHEMA.reverse_entry, "reverse_entry");
  assertJournalEntry(reversed.entry, SCHEMA.reverse_entry, "reverse_entry.entry");

  const proposed = await ledger.proposeAdjustment("joel", "test proposal", [
    { account: "6100", class: "foam", debit: "5.00" },
    { account: "1100", credit: "5.00" },
  ]);
  assertWithLockedThrough(proposed, SCHEMA.propose_adjustment, "propose_adjustment");
  for (const line of proposed.lines) assertRow(line, SCHEMA.propose_adjustment.line, "propose_adjustment.lines[]");

  const decided = await ledger.decideAdjustment(proposed.request_id, "dan", true, "fine");
  assertWithLockedThrough(decided, SCHEMA.decide_adjustment, "decide_adjustment");
  assert.equal(decided.state, "posted");

  const confirmed = await ledger.bankConfirmProposal("bl-3", "6100", "foam", "dan");
  assertWithLockedThrough(confirmed, SCHEMA.bank_confirm_proposal, "bank_confirm_proposal");

  const closed = await ledger.closePeriod("2026-08-31", "dan", "August close");
  assertWithLockedThrough(closed, SCHEMA.close_period, "close_period");
  assert.equal(closed.locked_through, "2026-08-31");

  const reopened = await ledger.reopenPeriod("2026-08-31", "dan", "need to fix something");
  assertWithLockedThrough(reopened, SCHEMA.reopen_period, "reopen_period");
  assert.equal(reopened.reopened, true);
  assert.equal(reopened.locked_through, null);
});

test("a write dated on or before locked_through is refused, not silently accepted", async () => {
  const ledger = createFixtureProvider();
  await assert.rejects(
    () => ledger.saveAndPostDocument(
      { document_id: "too-early", kind: "Invoice", txn_date: "2026-07-01", lines: [{ amount: "10.00" }] },
      "dan",
      {},
    ),
    (err) => err instanceof Error && /closed/.test(err.message),
  );
});

test("proposing and approving an adjustment moves the trial balance", async () => {
  const ledger = createFixtureProvider();
  // An approved adjustment posts dated the day it is decided, so read the
  // balance as of a date after any run of this test, not a fixed past one.
  const asOf = "2099-12-31";
  const before = await ledger.trialBalance(asOf);
  const suppliesBefore = before.rows.find((r) => r.account_id === "6100").balance;

  const proposed = await ledger.proposeAdjustment("joel", "move supplies", [
    { account: "6100", class: "foam", debit: "30.00" },
    { account: "1100", credit: "30.00" },
  ]);
  await ledger.decideAdjustment(proposed.request_id, "dan", true, "ok");

  const after = await ledger.trialBalance(asOf);
  const suppliesAfter = after.rows.find((r) => r.account_id === "6100").balance;
  assert.equal((Number(suppliesAfter) - Number(suppliesBefore)).toFixed(2), "30.00");
});

test("confirming a bank proposal resolves that line and posts an entry", async () => {
  const ledger = createFixtureProvider();
  const before = await ledger.bankStatus("stmt-2026-08");
  assert.equal(before.lines.find((l) => l.line_id === "bl-3").match_kind, "proposed");

  const result = await ledger.bankConfirmProposal("bl-3", "6100", "foam", "dan");
  const after = await ledger.bankStatus("stmt-2026-08");
  const line = after.lines.find((l) => l.line_id === "bl-3");
  assert.equal(line.match_kind, "manual");
  assert.equal(line.matched_entry_id, result.entry_id);
  assert.equal(after.summary.proposed, 0);
});

test("decide_adjustment refuses to decide an already-decided request", async () => {
  const ledger = createFixtureProvider();
  await assert.rejects(() => ledger.decideAdjustment("adj-9001", "dan", true, "again"));
});

// --- master data, create_document, open items and aging (D28) -------------

test("ledger fixture provider satisfies the schema for the D28 master-data and open-item tools", async () => {
  const ledger = createFixtureProvider();

  const customers = await ledger.listContacts("customer");
  assert.ok(customers.length > 0);
  for (const c of customers) assertRow(c, SCHEMA.list_contacts.row, "list_contacts[]");
  assert.ok(customers.every((c) => c.kind === "customer"));

  const items = await ledger.listItems();
  assert.ok(items.length > 0);
  for (const i of items) assertRow(i, SCHEMA.list_items.row, "list_items[]");

  for (const book of ["receivable", "payable"]) {
    const open = await ledger.openDocuments(book, "2026-10-15");
    for (const d of open) assertRow(d, SCHEMA.open_documents.row, `open_documents(${book})[]`);
    const aging = await ledger.aging(book, "2026-10-15");
    assertRow(aging, SCHEMA.aging.top, `aging(${book})`);
    for (const r of [...aging.rows, aging.totals]) assertRow(r, SCHEMA.aging.row, `aging(${book}) row`);
  }

  const contact = await ledger.upsertContact({ contact_id: "cust-new", kind: "customer", display_name: "Lakeside Y", terms_days: 10, default_class: "foam" });
  assertWithLockedThrough(contact, SCHEMA.upsert_contact, "upsert_contact");
  assertRow(contact.contact, SCHEMA.upsert_contact.contact, "upsert_contact.contact");

  const item = await ledger.upsertItem({ item_id: "kb-1", name: "Kickboard", item_type: "non_inventory", income_account: "4100", expense_account: "5000", default_class: "foam", sales_price: "12.00", is_taxable: true });
  assertWithLockedThrough(item, SCHEMA.upsert_item, "upsert_item");
  assertRow(item.item, SCHEMA.upsert_item.item, "upsert_item.item");

  const created = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-01", contact: { kind: "Customer", id: "cust-new" },
    lines: [{ kind: "Item", item_id: "kb-1", qty: "5", amount: "60.00", is_taxable: true }],
  }, "dan");
  assertWithLockedThrough(created, SCHEMA.create_document, "create_document");
  assertJournalEntry(created.entry, SCHEMA.create_document, "create_document.entry");
});

test("create_document fills number, due date, header class and banker's-rounded tax from master data", async () => {
  const ledger = createFixtureProvider();
  const created = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-01", contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", item_id: "sign-custom", amount: "100.00", is_taxable: true }],
  }, "dan");
  // Next after the seeded 21234; Net 30; 100.00 × 6.625% = 6.625 -> 6.62 (half to even).
  assert.equal(created.number, "21235");
  assert.equal(created.due_date, "2026-10-31");
  assert.equal(created.tax, "6.62");
  assert.equal(created.total, "106.62");
  const income = created.entry.lines.find((l) => l.account === "4100");
  assert.equal(income.class, "signs", "an item's default class beats the customer's");

  const next = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-02", contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", item_id: "rt-50-red", amount: "19.95" }],
  }, "dan");
  assert.equal(next.number, "21236");
  assert.equal(next.tax, null, "no taxable line, no tax");

  const estimate = await ledger.createDocument({
    kind: "Estimate", txn_date: "2026-10-02", contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", item_id: "rt-50-red", amount: "19.95" }],
  }, "dan");
  assert.equal(estimate.number, "1001", "a kind with nothing saved starts at FIRST_NUMBER");
  assert.equal(estimate.entry_id, null, "an estimate posts nothing");
});

test("an exempt customer is charged no tax", async () => {
  const ledger = createFixtureProvider();
  const created = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-01", contact: { kind: "Customer", id: "cust-fairview" },
    lines: [{ kind: "Item", item_id: "lgc-50", amount: "2990.00", is_taxable: true }],
  }, "dan");
  assert.equal(created.tax, "0.00");
  assert.equal(created.total, "2990.00");
});

test("create_document refuses unknown, wrong-kind and duplicate references and saves nothing", async () => {
  const ledger = createFixtureProvider();
  const invoice = (contact, extra = {}) => ({
    kind: "Invoice", txn_date: "2026-10-01", contact,
    lines: [{ kind: "Item", item_id: "rt-50-red", amount: "19.95" }], ...extra,
  });
  await assert.rejects(ledger.createDocument(invoice({ kind: "Customer", id: "nobody" }), "dan"), /unknown customer nobody/);
  await assert.rejects(ledger.createDocument(invoice({ kind: "Vendor", id: "vend-spectra" }), "dan"), /needs a customer/);
  await assert.rejects(ledger.createDocument(invoice({ kind: "Customer", id: "cust-blue-harbor" }, { number: "21234" }), "dan"), /already used by document inv-2001/);
  await assert.rejects(ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-01", contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", item_id: "no-such-item", amount: "1.00" }],
  }, "dan"), /unknown item no-such-item/);
  await assert.rejects(ledger.createDocument(invoice({ kind: "Customer", id: "cust-blue-harbor" }, { txn_date: "2026-07-01" }), "dan"), /closed/);

  const created = await ledger.createDocument(invoice({ kind: "Customer", id: "cust-blue-harbor" }), "dan");
  assert.equal(created.number, "21235", "no refused document consumed a number");
});

test("the aging ties to 1200 and 2000, and a payment takes an invoice out of it", async () => {
  const ledger = createFixtureProvider();
  const asOf = "2026-10-15";
  const control = async (number) => (await ledger.trialBalance(asOf)).rows.find((r) => r.number === number).balance;

  let ar = await ledger.aging("receivable", asOf);
  assert.equal(ar.totals.total, await control("1200"));
  const ap = await ledger.aging("payable", asOf);
  assert.equal(`-${ap.totals.total}`, await control("2000"));

  const created = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-01", contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", item_id: "sign-custom", amount: "100.00", is_taxable: true }],
  }, "dan");
  ar = await ledger.aging("receivable", asOf);
  assert.equal(ar.totals.total, await control("1200"));
  const blue = ar.rows.find((r) => r.contact_id === "cust-blue-harbor");
  assert.equal(blue.current, "106.62", "due 31 Oct, so current on 15 Oct");

  await ledger.createDocument({
    kind: "Payment", txn_date: "2026-10-10", contact: { kind: "Customer", id: "cust-blue-harbor" },
    applications: [{ target_document_id: created.document_id, target_kind: "Invoice", amount: "50.00" }],
  }, "dan");
  const open = (await ledger.openDocuments("receivable", asOf)).find((d) => d.document_id === created.document_id);
  assert.equal(open.applied, "50.00");
  assert.equal(open.open, "56.62");
  ar = await ledger.aging("receivable", asOf);
  assert.equal(ar.totals.total, await control("1200"), "the payment's 1200 credit and the application agree");

  // As of the day before the payment, the invoice is still open in full.
  const before = (await ledger.openDocuments("receivable", "2026-10-09")).find((d) => d.document_id === created.document_id);
  assert.equal(before.open, "106.62");
});

test("upsert_contact refuses a second customer with the same name; upsert_item enforces the asset-account rule", async () => {
  const ledger = createFixtureProvider();
  await assert.rejects(
    ledger.upsertContact({ contact_id: "cust-dup", kind: "customer", display_name: "blue harbor swim club" }),
    /already exists/,
  );
  await ledger.upsertContact({ contact_id: "vend-dup", kind: "vendor", display_name: "Blue Harbor Swim Club" });
  await assert.rejects(
    ledger.upsertItem({ item_id: "x", name: "X", item_type: "inventory", income_account: "4100", expense_account: "5000" }),
    /asset account/,
  );
  await assert.rejects(
    ledger.upsertItem({ item_id: "y", name: "Y", item_type: "service", income_account: "4100", expense_account: "5000", asset_account: "1300" }),
    /asset account/,
  );
});
