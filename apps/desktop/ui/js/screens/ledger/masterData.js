// Customers, vendors and items, the pure half: the editor form's fields as
// the `contact` and `item` `upsert_contact` / `upsert_item` take
// (`docs/MCP.md`), and the id a new record gets. No DOM, so
// `test/ledger-entry.test.js` exercises it head-on.
//
// The book is the judge of what is valid — a duplicate name, an account or
// class that doesn't exist, an inventory item without an asset account all
// come back as tool errors the screen shows. What is checked here is only
// what can't be sent at all: a missing name, a money value that isn't one.

import { termsLabel, termsOf } from "../../data/ledger/terms.js";

/** How a contact's terms read in a list or a note: "Net 30", "Due the 1st". */
export function contactTermsLabel(contact) {
  return termsLabel(termsOf(contact));
}

const MONEY_RE = /^\d+(\.\d{1,2})?$/;

function blankToNull(value) {
  const t = String(value ?? "").trim();
  return t === "" ? null : t;
}

function money(value, what) {
  const t = blankToNull(value);
  if (t == null) return null;
  if (!MONEY_RE.test(t)) throw new Error(`${what} must be an amount like "19.95", got ${JSON.stringify(t)}`);
  return t;
}

/** "Blue Harbor Swim Club" -> "blue-harbor-swim-club". */
export function slug(text) {
  return String(text).toLowerCase().normalize("NFKD").replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "new";
}

/** A new record's id: `prefix-slug`, suffixed `-2`, `-3`… past any id
 * already taken. An existing record keeps its id forever, whatever it is
 * renamed to — documents point at the id, not the name. */
export function newId(prefix, name, taken) {
  const base = `${prefix}-${slug(name)}`;
  if (!taken.has(base)) return base;
  let n = 2;
  while (taken.has(`${base}-${n}`)) n += 1;
  return `${base}-${n}`;
}

/**
 * @param {"customer" | "vendor"} kind
 * @param {Record<string, string | boolean>} form the editor's fields
 * @param {{ contact_id?: string, source_ref?: string | null, terms_source_ref?: string | null } | null} existing
 * @param {Set<string>} takenIds every contact id already in the book
 */
export function buildContact(kind, form, existing, takenIds) {
  const displayName = blankToNull(form.display_name);
  if (!displayName) throw new Error("a name is required");
  const dayOfMonth = form.terms_type === "day_of_month";
  const terms = dayOfMonth ? null : blankToNull(form.terms_days);
  if (terms != null && !/^\d{1,3}$/.test(terms)) throw new Error(`terms must be a whole number of days, got ${JSON.stringify(terms)}`);
  let byDay = null;
  if (dayOfMonth) {
    const day = blankToNull(form.due_day);
    if (day == null || !/^\d{1,2}$/.test(day) || Number(day) < 1 || Number(day) > 31) {
      throw new Error(`the due day must be 1 to 31, got ${JSON.stringify(day ?? "")}`);
    }
    const within = blankToNull(form.next_month_within) ?? "0";
    if (!/^\d{1,2}$/.test(within) || Number(within) > 31) {
      throw new Error(`"next month if within" must be 0 to 31 days, got ${JSON.stringify(within)}`);
    }
    byDay = { type: "day_of_month", day: Number(day), next_month_within: Number(within) };
  }
  return {
    contact_id: existing?.contact_id ?? newId(kind === "customer" ? "cust" : "vend", displayName, takenIds),
    kind: kind === "customer" ? "Customer" : "Vendor",
    display_name: displayName,
    company_name: blankToNull(form.company_name),
    email: blankToNull(form.email),
    phone: blankToNull(form.phone),
    bill_address: blankToNull(form.bill_address),
    ship_address: blankToNull(form.ship_address),
    terms_days: terms == null ? null : Number(terms),
    // Day-of-month terms (D41) go as `terms`; net terms keep `terms_days`.
    ...(byDay ? { terms: byDay } : {}),
    is_tax_exempt: kind === "customer" && Boolean(form.is_tax_exempt),
    default_class: blankToNull(form.default_class),
    is_active: form.is_active !== false,
    source_ref: existing?.source_ref ?? null,
    terms_source_ref: existing?.terms_source_ref ?? null,
  };
}

export const ITEM_TYPES = [
  { id: "non_inventory", label: "Non-inventory" },
  { id: "inventory", label: "Inventory" },
  { id: "service", label: "Service" },
];

/**
 * @param {Record<string, string | boolean>} form
 * @param {{ item_id?: string, unit_cost?: string | null, source_ref?: string | null } | null} existing
 * @param {Set<string>} takenIds
 */
export function buildItem(form, existing, takenIds) {
  const name = blankToNull(form.name);
  if (!name) throw new Error("a name is required");
  const itemType = form.item_type || "non_inventory";
  if (!ITEM_TYPES.some((t) => t.id === itemType)) throw new Error(`unknown item type ${JSON.stringify(itemType)}`);
  const income = blankToNull(form.income_account);
  const expense = blankToNull(form.expense_account);
  if (!income || !expense) throw new Error("an income account and an expense account are required");
  const sku = blankToNull(form.sku);
  return {
    item_id: existing?.item_id ?? newId("item", sku ?? name, takenIds),
    sku,
    name,
    description: blankToNull(form.description),
    item_type: itemType,
    income_account: income,
    expense_account: expense,
    // Only an inventory item carries one; the book refuses it on any other.
    asset_account: itemType === "inventory" ? blankToNull(form.asset_account) : null,
    default_class: blankToNull(form.default_class),
    sales_price: money(form.sales_price, "the sales price"),
    purchase_cost: money(form.purchase_cost, "the purchase cost"),
    // Moving-average cost (W12) is the book's, not the editor's.
    unit_cost: existing?.unit_cost ?? null,
    is_taxable: Boolean(form.is_taxable),
    is_active: form.is_active !== false,
    source_ref: existing?.source_ref ?? null,
  };
}
