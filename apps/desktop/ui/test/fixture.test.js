// Schema contract test. `ROADMAP.md` §B1: the fixture provider's output
// must satisfy `schema.js` for every tool — derived from `query.rs` and
// `mcp.rs` — so a screen written against the fixture is written against the
// same shape the real replica returns.

import { test } from "node:test";
import assert from "node:assert/strict";

import { createFixtureProvider } from "../js/data/fixture.js";
import { REALMS } from "../js/data/realms.js";
import {
  SCHEMA,
  firstMissingField,
} from "../js/data/schema.js";

function assertRow(row, fields, label) {
  const missing = firstMissingField(row, fields);
  assert.equal(missing, null, `${label} is missing field ${missing}`);
}

function assertArrayResult(result, spec, label) {
  assert.ok(Array.isArray(result), `${label} should be an array`);
  for (const row of result) assertRow(row, spec.row, `${label} row`);
}

function assertDocumentDetail(result, spec) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `document_detail is missing top-level field ${missing}`);
  assertRow(result.document, spec.document, "document_detail.document");
  assert.ok(Array.isArray(result.lines), "document_detail.lines should be an array");
  for (const line of result.lines) assertRow(line, spec.lines, "document_detail.lines[]");
  assertRow(result.lineage, spec.lineage, "document_detail.lineage");
  assert.ok(Array.isArray(result.lineage.documents), "lineage.documents should be an array");
  for (const doc of result.lineage.documents) assertRow(doc, spec.document, "lineage.documents[]");
  assert.ok(Array.isArray(result.lineage.edges), "lineage.edges should be an array");
  for (const edge of result.lineage.edges) assertRow(edge, spec.lineageEdge, "lineage.edges[]");
  assert.ok(Array.isArray(result.lineage.unresolved), "lineage.unresolved should be an array");
}

function assertContactDetail(result, spec) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `contact_detail is missing top-level field ${missing}`);
  assertRow(result.contact, spec.contact, "contact_detail.contact");
  for (const doc of result.open_documents) assertRow(doc, spec.document, "contact_detail.open_documents[]");
  for (const doc of result.recent_documents) assertRow(doc, spec.document, "contact_detail.recent_documents[]");
}

function assertItemDetail(result, spec) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `item_detail is missing top-level field ${missing}`);
  assertRow(result.item, spec.item, "item_detail.item");
  for (const doc of result.where_used) assertRow(doc, spec.document, "item_detail.where_used[]");
  assert.equal(typeof result.units_sold, "string", "item_detail.units_sold should be a decimal string");
}

function assertAging(result, spec, label) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `${label} is missing top-level field ${missing}`);
  assert.equal(typeof result.as_of, "string", `${label}.as_of should be a date string`);
  assertRow(result.totals, spec.buckets, `${label}.totals`);
  for (const row of result.rows) {
    assertRow(row, spec.row, `${label}.rows[]`);
    assertRow(row.buckets, spec.buckets, `${label}.rows[].buckets`);
  }
}

function assertSyncStatus(result, spec) {
  const missing = firstMissingField(result, spec.top);
  assert.equal(missing, null, `sync_status is missing top-level field ${missing}`);
  assert.equal(typeof result.write_enabled, "boolean");
  assert.equal(typeof result.quarantined_total, "number");
  assert.ok(Array.isArray(result.entities));
  for (const e of result.entities) assertRow(e, spec.entity, "sync_status.entities[]");
}

function assertSearch(result, spec) {
  assert.ok(Array.isArray(result), "search should return an array");
  for (const hit of result) {
    assertRow(hit, spec.row, "search[] hit");
    const kindFields = spec.hitByKind[hit.hit?.kind];
    assert.ok(kindFields, `search[] hit.hit.kind ${JSON.stringify(hit.hit?.kind)} is not a known kind`);
    assertRow(hit.hit, kindFields, `search[] hit.hit (${hit.hit.kind})`);
  }
}

for (const realm of REALMS) {
  test(`fixture provider satisfies the schema for every tool — ${realm.name}`, async () => {
    const provider = createFixtureProvider(realm.id);

    assertSearch(await provider.search("21234", 20), SCHEMA.search);
    assertSearch(await provider.search("nothing matches this", 20), SCHEMA.search);

    const invoices = await provider.listDocuments("Invoice");
    assertArrayResult(invoices, SCHEMA.list_documents, "list_documents(Invoice)");

    const openInvoices = await provider.openDocuments("Invoice");
    assertArrayResult(openInvoices, SCHEMA.open_documents, "open_documents(Invoice)");

    if (invoices.length > 0) {
      const detail = await provider.documentDetail(invoices[0].qbo_id);
      assertDocumentDetail(detail, SCHEMA.document_detail);
    }

    const [customer] = realm.id === REALMS[0].id ? [{ type: "customer", id: "31" }] : [{ type: "customer", id: "61" }];
    const contact = await provider.contactDetail(customer.type, customer.id);
    assertContactDetail(contact, SCHEMA.contact_detail);

    const itemId = realm.id === REALMS[0].id ? "201" : "261";
    const item = await provider.itemDetail(itemId);
    assertItemDetail(item, SCHEMA.item_detail);

    const ar = await provider.arAging("2026-08-20");
    assertAging(ar, SCHEMA.ar_aging, "ar_aging");

    const ap = await provider.apAging("2026-08-20");
    assertAging(ap, SCHEMA.ap_aging, "ap_aging");

    const sync = await provider.syncStatus();
    assertSyncStatus(sync, SCHEMA.sync_status);

    const classes = await provider.classTree();
    assertArrayResult(classes, SCHEMA.class_tree, "class_tree");

    const accounts = await provider.chartOfAccounts();
    assertArrayResult(accounts, SCHEMA.chart_of_accounts, "chart_of_accounts");
  });
}

test("fixture provider rejects an unknown realm the way a bad realm_id would", async () => {
  const provider = createFixtureProvider(REALMS[0].id);
  provider.setRealmId("not-a-real-realm");
  await assert.rejects(() => provider.syncStatus());
});

test("fixture provider rejects an unknown document id", async () => {
  const provider = createFixtureProvider(REALMS[0].id);
  await assert.rejects(() => provider.documentDetail("does-not-exist"));
});

test("document_detail's lineage actually walks the estimate -> invoice -> payment chain", async () => {
  const provider = createFixtureProvider(REALMS[0].id);
  // Invoice 21233 (qbo_id 411) is built from estimate 3608 (qbo_id 400).
  const detail = await provider.documentDetail("411");
  const ids = detail.lineage.documents.map((d) => d.qbo_id).sort();
  assert.ok(ids.includes("400"), "lineage should reach the estimate this invoice came from");
  assert.ok(ids.includes("411"), "lineage should include the root document itself");
});

test("ar_aging buckets a known invoice into the bucket its due date implies", async () => {
  const provider = createFixtureProvider(REALMS[0].id);
  const report = await provider.arAging("2026-08-20");
  // Invoice 21226 (Northgate, qbo_id 417) is due 2026-06-20 — 61 days past
  // due as of 2026-08-20, the 61-90 bucket.
  const row = report.rows.find((r) => r.contact_id === "36");
  assert.ok(row, "Northgate should appear in the aging report");
  assert.notEqual(row.buckets.d61_90, "0.00");
});

test("open_documents(Estimate) includes a pending estimate and excludes a closed one", async () => {
  const provider = createFixtureProvider(REALMS[0].id);
  const open = await provider.openDocuments("Estimate");
  const ids = open.map((d) => d.qbo_id);
  assert.ok(ids.includes("402"), "the pending estimate should be open");
  assert.ok(!ids.includes("403"), "the closed estimate should not be open");
});
