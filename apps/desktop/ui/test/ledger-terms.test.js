// Date-driven terms (DECISIONS.md D41) in the desktop: the pure due-date
// rule the fixture fills with (the twin of `books::Terms::due_date`), the
// editor's form as `upsert_contact` takes it, and the fixture book taking
// `terms` and filling an invoice's due date from it.

import { test } from "node:test";
import assert from "node:assert/strict";

import { termsDueDate, termsLabel, termsOf } from "../js/data/ledger/terms.js";
import { buildContact, contactTermsLabel } from "../js/screens/ledger/masterData.js";
import { createFixtureProvider } from "../js/data/ledger/fixture.js";

const byDay = (day, within = 0) => ({ type: "day_of_month", day, next_month_within: within });

test("the 31st is the last day of a shorter month, February included", () => {
  assert.equal(termsDueDate(byDay(31), "2027-02-03"), "2027-02-28");
  assert.equal(termsDueDate(byDay(31), "2026-04-10"), "2026-04-30");
  assert.equal(termsDueDate(byDay(30), "2027-02-01"), "2027-02-28");
  assert.equal(termsDueDate(byDay(31, 5), "2027-01-28"), "2027-02-28");
});

test("a leap February ends on the 29th", () => {
  assert.equal(termsDueDate(byDay(31), "2028-02-03"), "2028-02-29");
  assert.equal(termsDueDate(byDay(29), "2027-02-03"), "2027-02-28");
  assert.equal(termsDueDate(byDay(31, 5), "2028-01-28"), "2028-02-29");
});

test("dated within the window, the due date rolls a month (inclusive), across the year too", () => {
  const first = byDay(1, 5);
  assert.equal(termsDueDate(first, "2026-10-07"), "2026-11-01");
  assert.equal(termsDueDate(first, "2026-10-26"), "2026-11-01");
  assert.equal(termsDueDate(first, "2026-10-27"), "2026-12-01");
  assert.equal(termsDueDate(first, "2026-12-28"), "2027-02-01");
  assert.equal(termsDueDate(first, "2026-11-01"), "2026-12-01");
  assert.equal(termsDueDate(byDay(31, 5), "2023-04-25"), "2023-05-31");
  assert.equal(termsDueDate({ type: "net", days: 30 }, "2026-09-01"), "2026-10-01");
});

test("terms read the way the book says them", () => {
  assert.equal(termsLabel(byDay(1, 5)), "Due the 1st");
  assert.equal(termsLabel(byDay(22)), "Due the 22nd");
  assert.equal(termsLabel(byDay(12)), "Due the 12th");
  assert.equal(termsLabel(byDay(31)), "Due the last of the month");
  assert.equal(contactTermsLabel({ terms_days: 30 }), "Net 30");
  assert.equal(contactTermsLabel({ terms_days: null }), "Due on receipt");
  assert.equal(contactTermsLabel({ due_day: { day: 1, next_month_within: 5 } }), "Due the 1st");
  assert.deepEqual(termsOf({ terms: byDay(3, 2), terms_days: null }), byDay(3, 2));
});

test("buildContact sends day-of-month terms as `terms` and keeps the QBO term id", () => {
  const c = buildContact("customer", { display_name: "Lakeside", terms_type: "day_of_month", due_day: "1", next_month_within: "5", terms_days: "30" },
    { contact_id: "cust-lakeside", source_ref: "32", terms_source_ref: "37" }, new Set());
  assert.deepEqual(c.terms, byDay(1, 5));
  assert.equal(c.terms_days, null);
  assert.equal(c.terms_source_ref, "37");
  const net = buildContact("customer", { display_name: "Lakeside", terms_type: "net", terms_days: "30", due_day: "1" }, null, new Set());
  assert.equal(net.terms, undefined);
  assert.equal(net.terms_days, 30);
  assert.equal(buildContact("customer", { display_name: "X", terms_type: "day_of_month", due_day: "31" }, null, new Set()).terms.next_month_within, 0);
  assert.throws(() => buildContact("customer", { display_name: "X", terms_type: "day_of_month", due_day: "32" }, null, new Set()), /1 to 31/);
  assert.throws(() => buildContact("customer", { display_name: "X", terms_type: "day_of_month", due_day: "" }, null, new Set()), /1 to 31/);
  assert.throws(() => buildContact("customer", { display_name: "X", terms_type: "day_of_month", due_day: "1", next_month_within: "40" }, null, new Set()), /0 to 31/);
});

test("the fixture book takes `terms`, says them back, and fills an invoice's due date from them", async () => {
  const ledger = createFixtureProvider();
  const seeded = (await ledger.listContacts("customer", "lakeside"))[0];
  assert.deepEqual(seeded.terms, byDay(1, 5));
  assert.equal(seeded.terms_label, "Due the 1st");
  assert.equal(seeded.terms_days, null);
  assert.equal(seeded.terms_source_ref, "37");

  const saved = await ledger.upsertContact({ contact_id: "cust-club", kind: "customer", display_name: "Swim Club", terms: byDay(31, 5) });
  assert.deepEqual(saved.contact.terms, byDay(31, 5));
  assert.equal(saved.contact.terms_label, "Due the last of the month");
  await assert.rejects(
    ledger.upsertContact({ contact_id: "cust-club", kind: "customer", display_name: "Swim Club", terms_days: 30, terms: byDay(1) }),
    /disagree/,
  );

  const item = (await ledger.listItems())[0];
  const created = await ledger.createDocument({
    kind: "Invoice", txn_date: "2026-10-28", contact: { kind: "Customer", id: "cust-lakeside" },
    lines: [{ kind: "Item", item_id: item.item_id, qty: "1", amount: "10.00", is_taxable: false }],
  }, "dan");
  assert.equal(created.due_date, "2026-12-01");

  // Back to net days: the day of the month goes, the QBO term id stays.
  const back = await ledger.upsertContact({ ...seeded, kind: "customer", terms: undefined, terms_days: 15 });
  assert.equal(back.contact.terms_label, "Net 15");
  assert.equal(back.contact.terms_source_ref, "37");
});
