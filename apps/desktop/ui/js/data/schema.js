// The schema contract. `docs/MCP.md` documents eleven tools' JSON shapes;
// this file is that documentation turned into something a test can check —
// derived by reading the `Serialize` field order on the structs in
// `apps/qbo-local/src/store/query.rs` (`DocumentRow`, `LineRow`,
// `ContactDetail`, `ItemDetail`, `AgingReport`, `SyncStatus`, `ClassRow`,
// `AccountRow`) and `store/search.rs` (`SearchHit`, `Hit`), and the dispatch
// in `apps/qbo-local/src/mcp.rs::call_tool`.
//
// `test/fixture.test.js` uses `checkShape` to assert the fixture provider's
// output actually has these fields — not their types beyond object/array,
// since a decimal-string money field and a plain string both pass `typeof
// === "string"` and the interesting failure mode here is a missing or
// renamed field, the kind of drift a screen reading `doc.qbo_id` would hit
// immediately, not a formatting nuance.

// `store.rs::DocumentRow`
export const DOCUMENT_ROW_FIELDS = [
  "qbo_id", "doc_type", "doc_number", "txn_date", "due_date", "contact_id",
  "contact_type", "contact_name", "class_id", "total", "balance", "doc_status",
  "po_number", "private_note", "customer_memo", "is_deleted",
];

// `store.rs::LineRow`
export const LINE_ROW_FIELDS = [
  "line_no", "item_id", "description", "qty", "unit_price", "amount", "class_id", "is_taxable",
];

// `store/lineage.rs::Lineage`
export const LINEAGE_FIELDS = ["root", "documents", "edges", "unresolved"];

// `store/lineage.rs::DocumentLink`
export const DOCUMENT_LINK_FIELDS = ["from_qbo_id", "from_type", "to_qbo_id", "to_type", "line_no"];

// `store/query.rs::ContactRow`
export const CONTACT_ROW_FIELDS = [
  "contact_type", "qbo_id", "display_name", "company_name", "email", "phone", "balance", "is_active",
];

// `store/query.rs::ItemRow`
export const ITEM_ROW_FIELDS = [
  "qbo_id", "name", "sku", "description", "item_type", "unit_price", "purchase_cost",
  "qty_on_hand", "income_account_id", "expense_account_id", "asset_account_id", "is_active",
];

// `store/query.rs::AgingBuckets`
export const AGING_BUCKETS_FIELDS = ["current", "d1_30", "d31_60", "d61_90", "over_90"];

// `store/query.rs::AgingRow`
export const AGING_ROW_FIELDS = ["contact_id", "contact_name", "buckets", "total"];

// `store/query.rs::EntitySyncStatus`
export const ENTITY_SYNC_STATUS_FIELDS = [
  "entity_type", "mirrored", "last_cdc_cursor", "last_full_sweep", "quarantined",
];

// `store/query.rs::ClassRow`
export const CLASS_ROW_FIELDS = ["qbo_id", "name", "fully_qualified_name", "parent_id", "is_active"];

// `store/query.rs::AccountRow`
export const ACCOUNT_ROW_FIELDS = [
  "qbo_id", "name", "acct_num", "account_type", "account_subtype", "classification", "balance", "is_active",
];

// `store/search.rs::Hit`'s per-`kind` field sets, `kind` itself included
// (`#[serde(tag = "kind")]`).
export const SEARCH_HIT_FIELDS_BY_KIND = {
  Document: ["kind", ...DOCUMENT_ROW_FIELDS],
  Contact: ["kind", "contact_type", "qbo_id", "display_name", "company_name", "balance"],
  Item: ["kind", "qbo_id", "name", "sku", "item_type", "unit_price"],
};

/**
 * One entry per MCP tool (`mcp.rs::tools`): `result` says what shape
 * `structuredContent` itself is; nested `object`/`array` describe what's
 * inside. `top` on an `object` result lists its own required keys; `row` on
 * an `array` result (or a nested array field) lists each element's required
 * keys.
 */
export const SCHEMA = {
  search: {
    result: "array",
    row: ["reason", "hit"],
    hitByKind: SEARCH_HIT_FIELDS_BY_KIND,
  },
  document_detail: {
    result: "object",
    top: ["document", "lines", "lineage"],
    document: DOCUMENT_ROW_FIELDS,
    lines: LINE_ROW_FIELDS,
    lineage: LINEAGE_FIELDS,
    lineageEdge: DOCUMENT_LINK_FIELDS,
  },
  list_documents: {
    result: "array",
    row: DOCUMENT_ROW_FIELDS,
  },
  open_documents: {
    result: "array",
    row: DOCUMENT_ROW_FIELDS,
  },
  contact_detail: {
    result: "object",
    top: ["contact", "open_documents", "recent_documents"],
    contact: CONTACT_ROW_FIELDS,
    document: DOCUMENT_ROW_FIELDS,
  },
  item_detail: {
    result: "object",
    top: ["item", "where_used", "units_sold"],
    item: ITEM_ROW_FIELDS,
    document: DOCUMENT_ROW_FIELDS,
  },
  ar_aging: {
    result: "object",
    top: ["as_of", "rows", "totals"],
    row: AGING_ROW_FIELDS,
    buckets: AGING_BUCKETS_FIELDS,
  },
  ap_aging: {
    result: "object",
    top: ["as_of", "rows", "totals"],
    row: AGING_ROW_FIELDS,
    buckets: AGING_BUCKETS_FIELDS,
  },
  sync_status: {
    result: "object",
    top: ["write_enabled", "quarantined_total", "entities"],
    entity: ENTITY_SYNC_STATUS_FIELDS,
  },
  class_tree: {
    result: "array",
    row: CLASS_ROW_FIELDS,
  },
  chart_of_accounts: {
    result: "array",
    row: ACCOUNT_ROW_FIELDS,
  },
};

/** Every field named in `fields` is a key of `obj` (`undefined` is fine —
 * an `Option<T>` field that's `null` still round-trips through JSON as a
 * present key set to `null`, per `serde_json`'s default; `in` also accepts
 * that). Returns the first missing field, or `null` if none are missing. */
export function firstMissingField(obj, fields) {
  if (obj == null || typeof obj !== "object") return "(not an object)";
  for (const field of fields) {
    if (!(field in obj)) return field;
  }
  return null;
}
