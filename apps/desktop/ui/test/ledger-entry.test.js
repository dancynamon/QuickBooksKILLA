// The document-entry form's pure half (`js/screens/ledger/entry.js`): the
// line arithmetic, the running totals and the draft-to-document mapping
// `create_document` receives. The screen itself is driven in a browser; this
// is the part that has to be exactly right.

import { test } from "node:test";
import assert from "node:assert/strict";

import { buildDocument, lineAmount, lineForItem, previewTotals } from "../js/screens/ledger/entry.js";
import { controlDifference } from "../js/screens/ledger/openItems.js";

test("lineAmount is quantity × rate, half away from zero to the cent", () => {
  assert.equal(lineAmount("10", "19.95"), "199.50");
  assert.equal(lineAmount("1.5", "0.03"), "0.05"); // 0.045 -> 0.05
  assert.equal(lineAmount("0.3333", "3.00"), "1.00"); // 0.9999 -> 1.00
  assert.equal(lineAmount("", "3.00"), null);
  assert.throws(() => lineAmount("1", "3.001"), /at most 2 decimal places/);
  assert.throws(() => lineAmount("abc", "3.00"), /a quantity/);
});

test("previewTotals adds NJ tax on taxable lines with the book's half-to-even rounding", () => {
  const lines = [
    { amount: "100.00", taxable: true },
    { qty: "2", rate: "50.00", taxable: false },
    { amount: "", qty: "", rate: "" },
  ];
  assert.deepEqual(previewTotals("Invoice", lines, null), { subtotal: "200.00", tax: "6.62", total: "206.62", exempt: false });
  assert.deepEqual(previewTotals("Invoice", lines, { is_tax_exempt: true }), { subtotal: "200.00", tax: "0.00", total: "200.00", exempt: true });
  assert.equal(previewTotals("Bill", lines, null).tax, "0.00", "a purchase carries no sales tax");
});

test("previewTotals for a payment is the sum of what is applied, ignoring half-typed amounts", () => {
  const t = previewTotals("Payment", [], null, [{ amount: "100.00" }, { amount: "12." }, { amount: "" }]);
  assert.equal(t.total, "100.00");
});

test("buildDocument sends only what was typed, leaving number, due date, class and tax to the book", () => {
  const doc = buildDocument("Invoice", {
    txnDate: "2026-10-01", contactId: "cust-blue-harbor", number: "", memo: " ",
    lines: [
      { itemId: "rt-50-red", description: "Rescue tube", qty: "10", rate: "19.95", amount: "", cls: "", taxable: true },
      { itemId: "", account: "", description: "", qty: "", rate: "", amount: "" },
    ],
  });
  assert.deepEqual(doc, {
    kind: "Invoice",
    txn_date: "2026-10-01",
    contact: { kind: "Customer", id: "cust-blue-harbor" },
    lines: [{ kind: "Item", amount: "199.50", is_taxable: true, item_id: "rt-50-red", qty: "10", description: "Rescue tube" }],
  });
});

test("buildDocument maps a bill's account line, its vendor number and a class override", () => {
  const doc = buildDocument("Bill", {
    txnDate: "2026-10-01", contactId: "vend-spectra", number: "SPX-2001",
    lines: [{ account: "6100", amount: "84.10", cls: "foam", taxable: true }],
  });
  assert.equal(doc.number, "SPX-2001");
  assert.deepEqual(doc.contact, { kind: "Vendor", id: "vend-spectra" });
  assert.deepEqual(doc.lines, [{ kind: "Account", amount: "84.10", is_taxable: false, account: "6100", class: "foam" }]);
});

test("buildDocument refuses a draft the book would only refuse later", () => {
  const base = { txnDate: "2026-10-01", contactId: "c" };
  assert.throws(() => buildDocument("Invoice", { ...base, contactId: "" }), /choose a customer/);
  assert.throws(() => buildDocument("Invoice", { ...base, lines: [{}] }), /at least one line/);
  assert.throws(() => buildDocument("Invoice", { ...base, lines: [{ account: "4100", amount: "1.00" }] }), /line 1: choose an item$/);
  assert.throws(() => buildDocument("Invoice", { ...base, lines: [{ itemId: "i", description: "x" }] }), /line 1: an amount/);
  assert.throws(() => buildDocument("Invoice", { ...base, lines: [{ itemId: "i", amount: "1.005" }] }), /line 1: an amount must be/);
  assert.throws(() => buildDocument("Payment", { ...base, applications: [{ documentId: "d", amount: "" }] }), /at least one open invoice/);
});

test("buildDocument turns a payment's typed amounts into applications", () => {
  const doc = buildDocument("Payment", {
    txnDate: "2026-10-10", contactId: "cust-blue-harbor", bank: "1100",
    applications: [
      { documentId: "inv-1", kind: "Invoice", number: "21235", amount: "50" },
      { documentId: "inv-2", kind: "Invoice", number: "21236", amount: "" },
    ],
  });
  assert.deepEqual(doc.applications, [{ target_document_id: "inv-1", target_kind: "Invoice", amount: "50.00" }]);
  assert.equal(doc.deposit_to, "1100");
  assert.equal("lines" in doc, false);
});

test("lineForItem prices a sale at the sales price and a purchase at cost, untaxed", () => {
  const item = { item_id: "eva", name: "EVA sheet", sales_price: "80.00", purchase_cost: "46.28", is_taxable: true };
  assert.equal(lineForItem("Invoice", item).rate, "80.00");
  assert.equal(lineForItem("Invoice", item).taxable, true);
  assert.equal(lineForItem("PurchaseOrder", item).rate, "46.28");
  assert.equal(lineForItem("PurchaseOrder", item).taxable, false);
});

test("controlDifference reads a payable's credit balance with its sign", () => {
  assert.equal(controlDifference("receivable", "21916.50", "21916.50"), 0);
  assert.equal(controlDifference("payable", "9200.00", "-9200.00"), 0);
  assert.equal(controlDifference("receivable", "100.00", "90.00"), 1000);
});

test("buildDocument names the money account only when one is chosen, and a bill payment must name one", () => {
  const apps = [{ documentId: "b1", amount: "10.00" }];
  const payment = buildDocument("Payment", { txnDate: "2026-10-10", contactId: "c", applications: apps });
  assert.equal("deposit_to" in payment, false, "1150 is the book's default; nothing to send");
  const billPayment = buildDocument("BillPayment", { txnDate: "2026-10-10", contactId: "v", applications: apps });
  assert.equal(billPayment.pay_from, "1100");
  assert.deepEqual(billPayment.applications, [{ target_document_id: "b1", target_kind: "Bill", amount: "10.00" }]);
  assert.deepEqual(billPayment.contact, { kind: "Vendor", id: "v" });
  const receipt = buildDocument("SalesReceipt", {
    txnDate: "2026-10-10", contactId: "c", bank: "1100", lines: [{ itemId: "i", amount: "5.00", taxable: true }],
  });
  assert.equal(receipt.deposit_to, "1100");
  assert.equal(receipt.lines[0].is_taxable, true);
});

// --- master data editors (`js/screens/ledger/masterData.js`) ---------------

import { buildContact, buildItem, newId, slug } from "../js/screens/ledger/masterData.js";

test("newId slugs the name and steps past ids already taken; an existing record keeps its id", () => {
  assert.equal(slug("Blue Harbor Swim Club!"), "blue-harbor-swim-club");
  assert.equal(newId("cust", "Lakeside Y", new Set()), "cust-lakeside-y");
  assert.equal(newId("cust", "Lakeside Y", new Set(["cust-lakeside-y", "cust-lakeside-y-2"])), "cust-lakeside-y-3");
  const renamed = buildContact("customer", { display_name: "Blue Harbor Aquatics" }, { contact_id: "cust-blue-harbor", source_ref: "qbo:58" }, new Set());
  assert.equal(renamed.contact_id, "cust-blue-harbor");
  assert.equal(renamed.source_ref, "qbo:58");
});

test("buildContact sends the book's casing, nulls for blanks, and no exemption on a vendor", () => {
  const c = buildContact("customer", { display_name: " Lakeside Y ", terms_days: "15", is_tax_exempt: true, email: "", is_active: true }, null, new Set());
  assert.equal(c.kind, "Customer");
  assert.equal(c.display_name, "Lakeside Y");
  assert.equal(c.terms_days, 15);
  assert.equal(c.email, null);
  assert.equal(c.is_tax_exempt, true);
  const v = buildContact("vendor", { display_name: "Spectra", is_tax_exempt: true }, null, new Set());
  assert.equal(v.kind, "Vendor");
  assert.equal(v.is_tax_exempt, false);
  assert.throws(() => buildContact("customer", { display_name: "" }, null, new Set()), /name is required/);
  assert.throws(() => buildContact("customer", { display_name: "X", terms_days: "net 30" }, null, new Set()), /whole number of days/);
});

test("buildItem keeps the asset account on inventory only and leaves moving-average cost to the book", () => {
  const form = { name: "EVA sheet", sku: "EVA-2", item_type: "service", income_account: "4100", expense_account: "5000", asset_account: "1300", sales_price: "80.00" };
  assert.equal(buildItem(form, null, new Set()).asset_account, null);
  assert.equal(buildItem({ ...form, item_type: "inventory" }, null, new Set()).asset_account, "1300");
  assert.equal(buildItem(form, null, new Set()).item_id, "item-eva-2");
  assert.equal(buildItem(form, { item_id: "eva-sheet", unit_cost: "46.28" }, new Set()).unit_cost, "46.28");
  assert.throws(() => buildItem({ ...form, sales_price: "80.005" }, null, new Set()), /sales price/);
  assert.throws(() => buildItem({ ...form, income_account: "" }, null, new Set()), /income account/);
});
