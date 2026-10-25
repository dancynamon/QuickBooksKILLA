// The fixture provider: an in-memory replica shaped exactly like
// `docs/MCP.md`'s eleven tool results, built from data invented the same
// way `prototype/bunzbooks.html` is (`prototype/README.md`: "invent
// nothing real"). It exists so the UI runs the same code path in a plain
// browser (`python3 -m http.server` in `ui/`, per `apps/desktop/README.md`)
// as it does under Tauri — every screen reads through `provider`, and this
// is the only file that knows the data behind it is made up.
//
// This module re-derives the query logic `apps/qbo-local/src/store/query.rs`
// and `store/search.rs` implement in Rust — aging buckets, lineage walks,
// open-document status rules, search's ranked stages — because a fixture
// that merely returned canned per-screen payloads would not exercise any of
// the logic those screens actually depend on. `test/fixture.test.js` checks
// the *shape* against `schema.js`; the behaviour is cross-checked by hand
// against `query.rs`'s doc comments, cited inline below.

import { createProvider } from "./provider.js";
import { REALMS, DEFAULT_REALM_ID } from "./realms.js";

// ---------------------------------------------------------------------------
// small money/date helpers — cent-integer arithmetic so summing decimal
// strings never drifts the way repeated float addition would.
// ---------------------------------------------------------------------------

function toCents(decimal) {
  if (decimal == null) return 0;
  return Math.round(Number(decimal) * 100);
}

function fromCents(cents) {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(cents);
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, "0")}`;
}

function daysBetween(asOf, dateStr) {
  const a = new Date(`${asOf}T00:00:00Z`).getTime();
  const b = new Date(`${dateStr}T00:00:00Z`).getTime();
  return Math.round((a - b) / 86_400_000);
}

/** `query.rs::bucket_for`: not-yet-due (including due today) is `current`. */
function bucketFor(asOf, dueDate) {
  const daysPastDue = daysBetween(asOf, dueDate);
  if (daysPastDue <= 0) return "current";
  if (daysPastDue <= 30) return "d1_30";
  if (daysPastDue <= 60) return "d31_60";
  if (daysPastDue <= 90) return "d61_90";
  return "over_90";
}

function emptyBuckets() {
  return { current: 0, d1_30: 0, d31_60: 0, d61_90: 0, over_90: 0 };
}

function bucketsToMoney(buckets) {
  return {
    current: fromCents(buckets.current),
    d1_30: fromCents(buckets.d1_30),
    d31_60: fromCents(buckets.d31_60),
    d61_90: fromCents(buckets.d61_90),
    over_90: fromCents(buckets.over_90),
  };
}

function bucketsTotal(buckets) {
  return buckets.current + buckets.d1_30 + buckets.d31_60 + buckets.d61_90 + buckets.over_90;
}

// ---------------------------------------------------------------------------
// entity types, in `EntityType::ALL`'s order (`apps/qbo-local/src/domain.rs`)
// ---------------------------------------------------------------------------

const ENTITY_TYPES = [
  "CompanyInfo", "Account", "Class", "Customer", "Vendor", "Item", "TaxCode", "TaxRate", "Term",
  "Estimate", "Invoice", "SalesReceipt", "CreditMemo", "RefundReceipt", "Payment",
  "PurchaseOrder", "Bill", "BillPayment", "VendorCredit", "Purchase", "Deposit", "JournalEntry",
  "Department", "Preferences", "Attachable",
];

// ---------------------------------------------------------------------------
// realm 1 — "Harborworks, Inc.", the prototype's own fictional book
// ---------------------------------------------------------------------------

const HW_CLASSES = [
  { qbo_id: "10", name: "Foam Products", fully_qualified_name: "Foam Products", parent_id: null, is_active: true },
  { qbo_id: "11", name: "Signs", fully_qualified_name: "Signs", parent_id: null, is_active: true },
  { qbo_id: "12", name: "Lifeguard Chairs", fully_qualified_name: "Lifeguard Chairs", parent_id: null, is_active: true },
  { qbo_id: "13", name: "Dropship/Resale", fully_qualified_name: "Dropship/Resale", parent_id: null, is_active: true },
  { qbo_id: "14", name: "CNC Cutting", fully_qualified_name: "CNC Cutting", parent_id: null, is_active: true },
  { qbo_id: "15", name: "UV Printing", fully_qualified_name: "UV Printing", parent_id: null, is_active: true },
];

const HW_ACCOUNTS = [
  { qbo_id: "601", name: "Checking", acct_num: "1100", account_type: "Asset", account_subtype: "Checking", classification: "Asset", balance: "48210.00", is_active: true },
  { qbo_id: "602", name: "Accounts receivable", acct_num: "1200", account_type: "Asset", account_subtype: "AccountsReceivable", classification: "Asset", balance: "18211.03", is_active: true },
  { qbo_id: "603", name: "Inventory — raw materials", acct_num: "1300", account_type: "Asset", account_subtype: "Inventory", classification: "Asset", balance: "31840.00", is_active: true },
  { qbo_id: "604", name: "Inventory — finished goods", acct_num: "1310", account_type: "Asset", account_subtype: "Inventory", classification: "Asset", balance: "22110.00", is_active: true },
  { qbo_id: "605", name: "Accounts payable", acct_num: "2000", account_type: "Liability", account_subtype: "AccountsPayable", classification: "Liability", balance: "12045.00", is_active: true },
  { qbo_id: "606", name: "Sales tax payable", acct_num: "2200", account_type: "Liability", account_subtype: "SalesTaxPayable", classification: "Liability", balance: "2118.40", is_active: true },
  { qbo_id: "607", name: "Retained earnings", acct_num: "3900", account_type: "Equity", account_subtype: "RetainedEarnings", classification: "Equity", balance: null, is_active: true },
  { qbo_id: "608", name: "Sales income", acct_num: "4100", account_type: "Income", account_subtype: "SalesOfProductIncome", classification: "Income", balance: null, is_active: true },
  { qbo_id: "609", name: "Cost of goods sold", acct_num: "5000", account_type: "COGS", account_subtype: "CostOfLaborCos", classification: "Expense", balance: null, is_active: true },
  { qbo_id: "610", name: "Manufacturing variance", acct_num: "5100", account_type: "COGS", account_subtype: "SuppliesMaterialsCogs", classification: "Expense", balance: null, is_active: true },
  { qbo_id: "611", name: "Parts and labour applied", acct_num: "5300", account_type: "COGS", account_subtype: "SuppliesMaterialsCogs", classification: "Expense", balance: null, is_active: true },
  { qbo_id: "612", name: "Shop supplies and packaging", acct_num: "6100", account_type: "Expense", account_subtype: "OfficeGeneralAdministrativeExpenses", classification: "Expense", balance: null, is_active: true },
  { qbo_id: "613", name: "Vehicle and fuel", acct_num: "6200", account_type: "Expense", account_subtype: "AutomobileExpense", classification: "Expense", balance: null, is_active: true },
  { qbo_id: "614", name: "Other operating expense", acct_num: "6900", account_type: "Expense", account_subtype: "OtherMiscellaneousServiceCost", classification: "Expense", balance: null, is_active: true },
];

const HW_CUSTOMERS = [
  { qbo_id: "31", display_name: "Blue Harbor Swim Club", company_name: "BLUE HARBOR SWIM SCHOOLS", email: "ap@blueharborswim.example", phone: "555-010-1031", balance: "144.69", is_active: true },
  { qbo_id: "32", display_name: "Fairview County Parks", company_name: "FAIRVIEW COUNTY PARKS & REC.", email: "finance@fairviewparks.example", phone: "555-010-1032", balance: "1895.00", is_active: true },
  { qbo_id: "33", display_name: "Lakeshore Community", company_name: "LAKESHORE COMMUNITY CENTER", email: "ap@lakeshorecc.example", phone: null, balance: "0.00", is_active: true },
  { qbo_id: "34", display_name: "Ridgeline Aquatics", company_name: "RIDGELINE AQUATICS & FITNESS", email: "billing@ridgelineaquatics.example", phone: "555-010-1034", balance: "2610.34", is_active: true },
  { qbo_id: "35", display_name: "Harborview Signs", company_name: "HARBORVIEW SIGNWORKS", email: "orders@harborviewsigns.example", phone: null, balance: "0.00", is_active: true },
  { qbo_id: "36", display_name: "Northgate Aquatic", company_name: "NORTHGATE AQUATIC SUPPLY", email: "ap@northgateaquatic.example", phone: "555-010-1036", balance: "12888.00", is_active: true },
  { qbo_id: "37", display_name: "Ellisburg Rec", company_name: "TOWNSHIP OF ELLISBURG REC", email: "rec@ellisburgtwp.example", phone: null, balance: "624.00", is_active: true },
  { qbo_id: "38", display_name: "Summit Sports", company_name: "SUMMIT SPORTS OUTFITTERS", email: "po@summitsports.example", phone: null, balance: "0.00", is_active: true },
  { qbo_id: "39", display_name: "Tidewater Brand", company_name: "TIDEWATER BRAND", email: "ap@tidewaterbrand.example", phone: null, balance: "0.00", is_active: true },
];

const HW_VENDORS = [
  { qbo_id: "101", display_name: "Continental Foam", company_name: null, email: "orders@continentalfoam.example", phone: null, balance: "0.00", is_active: true },
  { qbo_id: "102", display_name: "Spectra EVA", company_name: null, email: "sales@spectraeva.example", phone: null, balance: "2776.80", is_active: true },
  { qbo_id: "103", display_name: "Polymer Source", company_name: null, email: "ap@polymersource.example", phone: null, balance: "9184.00", is_active: true },
  { qbo_id: "104", display_name: "Poolcraft Products", company_name: null, email: "orders@poolcraft.example", phone: null, balance: "84.20", is_active: true },
  { qbo_id: "105", display_name: "Aquabot Systems", company_name: null, email: "dealers@aquabot.example", phone: null, balance: "0.00", is_active: true },
  { qbo_id: "106", display_name: "M. Alvarez Fabrication", company_name: null, email: "shop@alvarezfab.example", phone: null, balance: "0.00", is_active: true },
];

const HW_ITEMS = [
  { qbo_id: "201", name: "40in ExoTube, custom logo", sku: "XRT-40-SFSP", description: "40in ExoTube, 6ft towline, custom logo", item_type: "Inventory", unit_price: "49.25", purchase_cost: "21.80", qty_on_hand: "214", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "202", name: "50in ExoTube rescue tube, red", sku: "XRT-50-STD", description: "50in ExoTube rescue tube, red", item_type: "Inventory", unit_price: "54.50", purchase_cost: "24.10", qty_on_hand: "388", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "203", name: "Ring buoy holder, stainless", sku: "1 RH 001", description: "Ring buoy holder — stainless steel", item_type: "Inventory", unit_price: "19.95", purchase_cost: "9.40", qty_on_hand: "64", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "204", name: "Floating mat 6x12, blue XLPE", sku: "PM-6X12-BLU", description: "Floating mat 6x12, blue XLPE", item_type: "Inventory", unit_price: "899.00", purchase_cost: "412.00", qty_on_hand: "12", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "205", name: "Swim club kickboard, EVA", sku: "AM-SC-KB", description: "Swim club kickboard, EVA", item_type: "Inventory", unit_price: "12.50", purchase_cost: "4.70", qty_on_hand: "940", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "206", name: "60in lifeguard chair", sku: "LG-CHAIR-60", description: "60in lifeguard chair, powder-coated", item_type: "Inventory", unit_price: "1495.00", purchase_cost: "780.00", qty_on_hand: "6", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "207", name: "NJ pool rules sign", sku: "SIGN-NJ-POOL", description: "NJ pool rules sign, 24x36 aluminum", item_type: "Inventory", unit_price: "124.00", purchase_cost: "48.50", qty_on_hand: "47", income_account_id: "608", expense_account_id: "609", asset_account_id: "604", is_active: true },
  { qbo_id: "208", name: "CNC cutting, shop hour", sku: "CNC-CUT-HR", description: "CNC cutting — shop hour", item_type: "Service", unit_price: "95.00", purchase_cost: "34.00", qty_on_hand: null, income_account_id: "608", expense_account_id: "609", asset_account_id: null, is_active: true },
  { qbo_id: "209", name: "UV flatbed printing, per sq ft", sku: "UV-PRINT-SF", description: "UV flatbed printing — per sq ft", item_type: "Service", unit_price: "14.50", purchase_cost: "5.20", qty_on_hand: null, income_account_id: "608", expense_account_id: "609", asset_account_id: null, is_active: true },
  { qbo_id: "210", name: "Shipping, UPS Ground", sku: "SHIP-U-GND", description: "Shipping — UPS Ground", item_type: "NonInventory", unit_price: "24.99", purchase_cost: "24.99", qty_on_hand: null, income_account_id: "608", expense_account_id: null, asset_account_id: null, is_active: true },
];

/** Internal document shape: a `DocumentRow` plus `lines` (`LineRow[]`) and
 * `links` (this document's own outgoing `LinkedTxn`s — `query.rs`'s
 * `links_from`). `toDocumentRow`/`toLineRow` strip these back down to
 * exactly the wire shape before anything is returned. */
const HW_DOCUMENTS = [
  // Estimates
  { qbo_id: "400", doc_type: "Estimate", doc_number: "3608", txn_date: "2026-08-02", due_date: null, contact_id: "36", contact_type: "Customer", contact_name: "Northgate Aquatic", class_id: "10", total: "2899.00", balance: null, doc_status: "Accepted", po_number: "KA-2026-4417", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "204", description: "Floating mat 6x12, blue XLPE", qty: "3", unit_price: "899.00", amount: "2697.00", class_id: "10", is_taxable: false },
    ], links: [] },
  { qbo_id: "401", doc_type: "Estimate", doc_number: "3601", txn_date: "2026-07-21", due_date: null, contact_id: "32", contact_type: "Customer", contact_name: "Fairview County Parks", class_id: "12", total: "2990.00", balance: null, doc_status: "Accepted", po_number: "UC-PR-55210", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "206", description: "60in lifeguard chair, powder-coated", qty: "2", unit_price: "1495.00", amount: "2990.00", class_id: "12", is_taxable: false },
    ], links: [] },
  { qbo_id: "402", doc_type: "Estimate", doc_number: "3611", txn_date: "2026-08-20", due_date: null, contact_id: "38", contact_type: "Customer", contact_name: "Summit Sports", class_id: "12", total: "1378.36", balance: null, doc_status: "Pending", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "206", description: "60in lifeguard chair, powder-coated", qty: "1", unit_price: "1245.00", amount: "1245.00", class_id: "12", is_taxable: true },
      { line_no: 2, item_id: "210", description: "Shipping — UPS Ground", qty: "1", unit_price: "128.36", amount: "128.36", class_id: null, is_taxable: false },
    ], links: [] },
  { qbo_id: "403", doc_type: "Estimate", doc_number: "3599", txn_date: "2026-07-09", due_date: null, contact_id: "34", contact_type: "Customer", contact_name: "Ridgeline Aquatics", class_id: "11", total: "842.00", balance: null, doc_status: "Closed", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "207", description: "NJ pool rules sign, 24x36 aluminum", qty: "6", unit_price: "124.00", amount: "744.00", class_id: "11", is_taxable: true },
    ], links: [] },

  // Invoices — due dates set to land in every aging bucket relative to the
  // fixture's own "as of" reference date, 2026-08-20.
  { qbo_id: "410", doc_type: "Invoice", doc_number: "21234", txn_date: "2026-08-15", due_date: "2026-09-05", contact_id: "31", contact_type: "Customer", contact_name: "Blue Harbor Swim Club", class_id: "13", total: "144.69", balance: "144.69", doc_status: null, po_number: "4471882", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "203", description: "Ring buoy holder — stainless steel", qty: "6", unit_price: "19.95", amount: "119.70", class_id: "13", is_taxable: false },
      { line_no: 2, item_id: "210", description: "Shipping — UPS Ground", qty: "1", unit_price: "24.99", amount: "24.99", class_id: null, is_taxable: false },
    ], links: [] },
  { qbo_id: "411", doc_type: "Invoice", doc_number: "21233", txn_date: "2026-08-14", due_date: "2026-08-14", contact_id: "36", contact_type: "Customer", contact_name: "Northgate Aquatic", class_id: "10", total: "2899.00", balance: "2899.00", doc_status: null, po_number: "KA-2026-4417", private_note: null, customer_memo: "From estimate 3608, invoiced in full on shipment.", is_deleted: false,
    lines: [
      { line_no: 1, item_id: "204", description: "Floating mat 6x12, blue XLPE", qty: "3", unit_price: "899.00", amount: "2697.00", class_id: "10", is_taxable: false },
      { line_no: 2, item_id: "202", description: "50in ExoTube rescue tube, red", qty: "3", unit_price: "54.50", amount: "163.50", class_id: "10", is_taxable: false },
    ], links: [{ to_qbo_id: "400", to_type: "Estimate", line_no: null }] },
  { qbo_id: "412", doc_type: "Invoice", doc_number: "21232", txn_date: "2026-08-13", due_date: "2026-07-25", contact_id: "34", contact_type: "Customer", contact_name: "Ridgeline Aquatics", class_id: "11", total: "398.34", balance: "398.34", doc_status: null, po_number: "HAF-8871", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "207", description: "NJ pool rules sign, 24x36 aluminum", qty: "3", unit_price: "124.00", amount: "372.00", class_id: "11", is_taxable: true },
    ], links: [] },
  { qbo_id: "413", doc_type: "Invoice", doc_number: "21231", txn_date: "2026-08-12", due_date: "2026-09-11", contact_id: "35", contact_type: "Customer", contact_name: "Harborview Signs", class_id: "14", total: "1482.00", balance: "0.00", doc_status: null, po_number: "HVSW-0912", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "208", description: "CNC cutting — shop hour", qty: "12", unit_price: "95.00", amount: "1140.00", class_id: "14", is_taxable: false },
      { line_no: 2, item_id: "209", description: "UV flatbed printing — per sq ft", qty: "24", unit_price: "14.50", amount: "348.00", class_id: "15", is_taxable: false },
    ], links: [] },
  { qbo_id: "414", doc_type: "Invoice", doc_number: "21230", txn_date: "2026-08-11", due_date: "2026-08-01", contact_id: "32", contact_type: "Customer", contact_name: "Fairview County Parks", class_id: "12", total: "2990.00", balance: "1895.00", doc_status: null, po_number: "UC-PR-55210", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "206", description: "60in lifeguard chair, powder-coated", qty: "2", unit_price: "1495.00", amount: "2990.00", class_id: "12", is_taxable: false },
    ], links: [{ to_qbo_id: "401", to_type: "Estimate", line_no: null }] },
  { qbo_id: "415", doc_type: "Invoice", doc_number: "21228", txn_date: "2026-08-06", due_date: "2026-07-15", contact_id: "37", contact_type: "Customer", contact_name: "Ellisburg Rec", class_id: "11", total: "624.00", balance: "624.00", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "207", description: "NJ pool rules sign, 24x36 aluminum", qty: "5", unit_price: "124.00", amount: "620.00", class_id: "11", is_taxable: false },
    ], links: [] },
  { qbo_id: "416", doc_type: "Invoice", doc_number: "21227", txn_date: "2026-08-04", due_date: "2026-09-03", contact_id: "31", contact_type: "Customer", contact_name: "Blue Harbor Swim Club", class_id: "10", total: "1970.00", balance: "0.00", doc_status: null, po_number: "4471560", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "201", description: "40in ExoTube, 6ft towline, custom logo", qty: "40", unit_price: "49.25", amount: "1970.00", class_id: "10", is_taxable: false },
    ], links: [] },
  { qbo_id: "417", doc_type: "Invoice", doc_number: "21226", txn_date: "2026-07-29", due_date: "2026-06-20", contact_id: "36", contact_type: "Customer", contact_name: "Northgate Aquatic", class_id: "10", total: "9989.00", balance: "9989.00", doc_status: null, po_number: "KA-2026-4180", private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "204", description: "Floating mat 6x12, blue XLPE", qty: "10", unit_price: "899.00", amount: "8990.00", class_id: "10", is_taxable: false },
      { line_no: 2, item_id: "202", description: "50in ExoTube rescue tube, red", qty: "18", unit_price: "54.50", amount: "981.00", class_id: "10", is_taxable: false },
    ], links: [] },
  { qbo_id: "418", doc_type: "Invoice", doc_number: "21224", txn_date: "2026-07-18", due_date: "2026-05-01", contact_id: "34", contact_type: "Customer", contact_name: "Ridgeline Aquatics", class_id: "10", total: "2212.00", balance: "2212.00", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "202", description: "50in ExoTube rescue tube, red", qty: "40", unit_price: "54.50", amount: "2180.00", class_id: "10", is_taxable: false },
    ], links: [] },

  // Payments
  { qbo_id: "440", doc_type: "Payment", doc_number: "PMT-5512", txn_date: "2026-08-14", due_date: null, contact_id: "35", contact_type: "Customer", contact_name: "Harborview Signs", class_id: null, total: "1482.00", balance: null, doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [], links: [{ to_qbo_id: "413", to_type: "Invoice", line_no: null }] },
  { qbo_id: "441", doc_type: "Payment", doc_number: "PMT-5505", txn_date: "2026-08-11", due_date: null, contact_id: "32", contact_type: "Customer", contact_name: "Fairview County Parks", class_id: null, total: "1095.00", balance: null, doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [], links: [{ to_qbo_id: "414", to_type: "Invoice", line_no: null }] },
  { qbo_id: "442", doc_type: "Payment", doc_number: "PMT-5498", txn_date: "2026-08-06", due_date: null, contact_id: "31", contact_type: "Customer", contact_name: "Blue Harbor Swim Club", class_id: null, total: "1970.00", balance: null, doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [], links: [{ to_qbo_id: "416", to_type: "Invoice", line_no: null }] },

  // Purchase orders
  { qbo_id: "420", doc_type: "PurchaseOrder", doc_number: "PO-4471", txn_date: "2026-08-14", due_date: null, contact_id: "101", contact_type: "Vendor", contact_name: "Continental Foam", class_id: "10", total: "4224.00", balance: null, doc_status: "Open", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "XLPE sheet 2lb, blue, 4x8x2", qty: "12", unit_price: "228.00", amount: "2736.00", class_id: "10", is_taxable: false },
      { line_no: 2, item_id: null, description: "XLPE sheet 2lb, red, 4x8x2", qty: "6", unit_price: "248.00", amount: "1488.00", class_id: "10", is_taxable: false },
    ], links: [] },
  { qbo_id: "421", doc_type: "PurchaseOrder", doc_number: "PO-4470", txn_date: "2026-08-12", due_date: null, contact_id: "104", contact_type: "Vendor", contact_name: "Poolcraft Products", class_id: "13", total: "84.20", balance: null, doc_status: "Closed", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "203", description: "Ring buoy holder — stainless steel", qty: "6", unit_price: "9.40", amount: "56.40", class_id: "13", is_taxable: false },
      { line_no: 2, item_id: null, description: "Inbound freight", qty: "1", unit_price: "27.80", amount: "27.80", class_id: "13", is_taxable: false },
    ], links: [] },
  { qbo_id: "422", doc_type: "PurchaseOrder", doc_number: "PO-4469", txn_date: "2026-08-09", due_date: null, contact_id: "102", contact_type: "Vendor", contact_name: "Spectra EVA", class_id: "10", total: "4628.00", balance: null, doc_status: "Open", po_number: null, private_note: null, customer_memo: "Blue only received — yellow backordered to 29 Aug.", is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "EVA sheet 38kg, blue", qty: "12", unit_price: "231.40", amount: "2776.80", class_id: "10", is_taxable: false },
      { line_no: 2, item_id: null, description: "EVA sheet 38kg, yellow", qty: "8", unit_price: "231.40", amount: "1851.20", class_id: "10", is_taxable: false },
    ], links: [] },
  { qbo_id: "423", doc_type: "PurchaseOrder", doc_number: "PO-4468", txn_date: "2026-08-05", due_date: null, contact_id: "105", contact_type: "Vendor", contact_name: "Aquabot Systems", class_id: "13", total: "399.00", balance: null, doc_status: "Closed", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "Robotic cleaner drive motor", qty: "1", unit_price: "399.00", amount: "399.00", class_id: "13", is_taxable: false },
    ], links: [] },
  { qbo_id: "424", doc_type: "PurchaseOrder", doc_number: "PO-4467", txn_date: "2026-07-31", due_date: null, contact_id: "103", contact_type: "Vendor", contact_name: "Polymer Source", class_id: "10", total: "9184.00", balance: null, doc_status: "Open", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "PE resin, natural, 55lb bag", qty: "24", unit_price: "382.66", amount: "9183.84", class_id: "10", is_taxable: false },
    ], links: [] },

  // Bills
  { qbo_id: "430", doc_type: "Bill", doc_number: "BILL-2291", txn_date: "2026-08-13", due_date: "2026-09-12", contact_id: "104", contact_type: "Vendor", contact_name: "Poolcraft Products", class_id: "13", total: "84.20", balance: "84.20", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "203", description: "Ring buoy holder — stainless steel", qty: "6", unit_price: "9.40", amount: "56.40", class_id: "13", is_taxable: false },
      { line_no: 2, item_id: null, description: "Inbound freight", qty: "1", unit_price: "27.80", amount: "27.80", class_id: "13", is_taxable: false },
    ], links: [{ to_qbo_id: "421", to_type: "PurchaseOrder", line_no: null }] },
  { qbo_id: "431", doc_type: "Bill", doc_number: "BILL-2288", txn_date: "2026-08-10", due_date: "2026-08-05", contact_id: "102", contact_type: "Vendor", contact_name: "Spectra EVA", class_id: "10", total: "2776.80", balance: "2776.80", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "EVA sheet 38kg, blue", qty: "12", unit_price: "231.40", amount: "2776.80", class_id: "10", is_taxable: false },
    ], links: [{ to_qbo_id: "422", to_type: "PurchaseOrder", line_no: null }] },
  { qbo_id: "432", doc_type: "Bill", doc_number: "BILL-2284", txn_date: "2026-08-06", due_date: "2026-09-05", contact_id: "105", contact_type: "Vendor", contact_name: "Aquabot Systems", class_id: "13", total: "399.00", balance: "0.00", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "Robotic cleaner drive motor", qty: "1", unit_price: "399.00", amount: "399.00", class_id: "13", is_taxable: false },
    ], links: [{ to_qbo_id: "423", to_type: "PurchaseOrder", line_no: null }] },
  { qbo_id: "433", doc_type: "Bill", doc_number: "BILL-2280", txn_date: "2026-04-01", due_date: "2026-04-01", contact_id: "103", contact_type: "Vendor", contact_name: "Polymer Source", class_id: "10", total: "9184.00", balance: "9184.00", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: null, description: "PE resin, natural, 55lb bag", qty: "24", unit_price: "382.66", amount: "9183.84", class_id: "10", is_taxable: false },
    ], links: [{ to_qbo_id: "424", to_type: "PurchaseOrder", line_no: null }] },

  // Bill payment
  { qbo_id: "450", doc_type: "BillPayment", doc_number: "BP-877", txn_date: "2026-08-12", due_date: null, contact_id: "105", contact_type: "Vendor", contact_name: "Aquabot Systems", class_id: null, total: "399.00", balance: null, doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [], links: [{ to_qbo_id: "432", to_type: "Bill", line_no: null }] },
];

// ---------------------------------------------------------------------------
// realm 2 — "Tidewater CNC Works", small on purpose: it only needs to prove
// the realm switcher actually changes what every screen shows.
// ---------------------------------------------------------------------------

const TW_CLASSES = [
  { qbo_id: "20", name: "CNC Cutting", fully_qualified_name: "CNC Cutting", parent_id: null, is_active: true },
  { qbo_id: "21", name: "UV Printing", fully_qualified_name: "UV Printing", parent_id: null, is_active: true },
];

const TW_ACCOUNTS = [
  { qbo_id: "701", name: "Checking", acct_num: "1100", account_type: "Asset", account_subtype: "Checking", classification: "Asset", balance: "9420.00", is_active: true },
  { qbo_id: "702", name: "Accounts receivable", acct_num: "1200", account_type: "Asset", account_subtype: "AccountsReceivable", classification: "Asset", balance: "560.00", is_active: true },
  { qbo_id: "703", name: "Sales income", acct_num: "4100", account_type: "Income", account_subtype: "SalesOfProductIncome", classification: "Income", balance: null, is_active: true },
  { qbo_id: "704", name: "Cost of goods sold", acct_num: "5000", account_type: "COGS", account_subtype: "CostOfLaborCos", classification: "Expense", balance: null, is_active: true },
];

const TW_CUSTOMERS = [
  { qbo_id: "61", display_name: "Bayline Marine Distributors", company_name: "BAYLINE MARINE DISTRIBUTORS LLC", email: "ap@baylinemarine.example", phone: null, balance: "560.00", is_active: true },
];

const TW_VENDORS = [
  { qbo_id: "161", display_name: "Precision Tool Supply", company_name: null, email: "sales@precisiontool.example", phone: null, balance: "0.00", is_active: true },
];

const TW_ITEMS = [
  { qbo_id: "261", name: "CNC cutting, shop hour", sku: "CNC-CUT-HR", description: "CNC cutting — shop hour", item_type: "Service", unit_price: "105.00", purchase_cost: "38.00", qty_on_hand: null, income_account_id: "703", expense_account_id: "704", asset_account_id: null, is_active: true },
  { qbo_id: "262", name: "UV flatbed printing, per sq ft", sku: "UV-PRINT-SF", description: "UV flatbed printing — per sq ft", item_type: "Service", unit_price: "15.75", purchase_cost: "5.60", qty_on_hand: null, income_account_id: "703", expense_account_id: "704", asset_account_id: null, is_active: true },
];

const TW_DOCUMENTS = [
  { qbo_id: "480", doc_type: "Estimate", doc_number: "E-118", txn_date: "2026-08-10", due_date: null, contact_id: "61", contact_type: "Customer", contact_name: "Bayline Marine Distributors", class_id: "20", total: "630.00", balance: null, doc_status: "Pending", po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "261", description: "CNC cutting — shop hour", qty: "6", unit_price: "105.00", amount: "630.00", class_id: "20", is_taxable: false },
    ], links: [] },
  { qbo_id: "481", doc_type: "Invoice", doc_number: "9042", txn_date: "2026-07-28", due_date: "2026-08-27", contact_id: "61", contact_type: "Customer", contact_name: "Bayline Marine Distributors", class_id: "20", total: "560.00", balance: "560.00", doc_status: null, po_number: null, private_note: null, customer_memo: null, is_deleted: false,
    lines: [
      { line_no: 1, item_id: "261", description: "CNC cutting — shop hour", qty: "5", unit_price: "105.00", amount: "525.00", class_id: "20", is_taxable: false },
      { line_no: 2, item_id: "262", description: "UV flatbed printing — per sq ft", qty: "2", unit_price: "15.75", amount: "31.50", class_id: "21", is_taxable: false },
    ], links: [] },
];

// ---------------------------------------------------------------------------
// realm registry
// ---------------------------------------------------------------------------

const DATA = {
  [REALMS[0].id]: {
    writeEnabled: false,
    lastFullSweep: "2026-09-13T08:02:00Z",
    lastCdcCursor: "2026-09-13T11:58:04Z",
    quarantined: { Invoice: 2 },
    classes: HW_CLASSES,
    accounts: HW_ACCOUNTS,
    customers: HW_CUSTOMERS,
    vendors: HW_VENDORS,
    items: HW_ITEMS,
    documents: HW_DOCUMENTS,
  },
  [REALMS[1].id]: {
    writeEnabled: false,
    lastFullSweep: "2026-09-12T22:15:00Z",
    lastCdcCursor: "2026-09-13T11:47:31Z",
    quarantined: {},
    classes: TW_CLASSES,
    accounts: TW_ACCOUNTS,
    customers: TW_CUSTOMERS,
    vendors: TW_VENDORS,
    items: TW_ITEMS,
    documents: TW_DOCUMENTS,
  },
};

class FixtureError extends Error {}

function realmData(realmId) {
  const realm = DATA[realmId];
  if (!realm) throw new FixtureError(`invalid realm_id: no such realm ${JSON.stringify(realmId)}`);
  return realm;
}

function allContacts(realm) {
  return [
    ...realm.customers.map((c) => ({ ...c, contact_type: "Customer" })),
    ...realm.vendors.map((v) => ({ ...v, contact_type: "Vendor" })),
  ];
}

function toDocumentRow(doc) {
  const {
    qbo_id, doc_type, doc_number, txn_date, due_date, contact_id, contact_type,
    contact_name, class_id, total, balance, doc_status, po_number, private_note,
    customer_memo, is_deleted,
  } = doc;
  return {
    qbo_id, doc_type, doc_number, txn_date, due_date, contact_id, contact_type,
    contact_name, class_id, total, balance, doc_status, po_number, private_note,
    customer_memo, is_deleted,
  };
}

function toLineRow(line) {
  const { line_no, item_id, description, qty, unit_price, amount, class_id, is_taxable } = line;
  return { line_no, item_id, description, qty, unit_price, amount, class_id, is_taxable };
}

function sortDocsDesc(docs) {
  return [...docs].sort((a, b) => {
    if (a.txn_date !== b.txn_date) return a.txn_date < b.txn_date ? 1 : -1;
    return a.qbo_id < b.qbo_id ? 1 : a.qbo_id > b.qbo_id ? -1 : 0;
  });
}

function paginate(rows, offset = 0, limit = 200) {
  const capped = Math.min(Number(limit) || 200, 1000);
  return rows.slice(Number(offset) || 0, (Number(offset) || 0) + capped);
}

// ---------------------------------------------------------------------------
// search — `store/search.rs`'s ranked stages, D10's "a bare number is a
// reference, not an amount".
// ---------------------------------------------------------------------------

function looksLikeAmount(query) {
  return query.includes(".") || query.includes(",") || query.startsWith("$");
}

function normalizeAmount(query) {
  return query.replace(/[$,]/g, "");
}

function search(realm, query, limit = 20) {
  const q = (query ?? "").trim();
  const cap = Math.min(Number(limit) || 20, 1000);
  if (!q || cap === 0) return [];

  const results = [];
  const seen = new Set();
  const qLower = q.toLowerCase();

  function add(reason, hit, key) {
    if (results.length >= cap || seen.has(key)) return;
    seen.add(key);
    results.push({ reason, hit });
  }

  const docs = realm.documents;
  const contacts = allContacts(realm);
  const items = realm.items;

  // 1. exact document number
  for (const d of docs) {
    if (results.length >= cap) break;
    if (d.doc_number && d.doc_number.toLowerCase() === qLower) {
      add("DocumentNumber", { kind: "Document", ...toDocumentRow(d) }, `doc:${d.qbo_id}`);
    }
  }
  // 2. exact customer PO number
  for (const d of docs) {
    if (results.length >= cap) break;
    if (d.po_number && d.po_number.toLowerCase() === qLower) {
      add("PurchaseOrderNumber", { kind: "Document", ...toDocumentRow(d) }, `doc:${d.qbo_id}`);
    }
  }
  // 3. exact SKU
  for (const it of items) {
    if (results.length >= cap) break;
    if (it.sku && it.sku.toLowerCase() === qLower) {
      add("Sku", { kind: "Item", qbo_id: it.qbo_id, name: it.name, sku: it.sku, item_type: it.item_type, unit_price: it.unit_price }, `item:${it.qbo_id}`);
    }
  }
  // 4. amount, only when the query reads as one (D10)
  if (results.length < cap && looksLikeAmount(q)) {
    const target = normalizeAmount(q);
    for (const d of docs) {
      if (results.length >= cap) break;
      if (d.total === target || d.balance === target) {
        add("Amount", { kind: "Document", ...toDocumentRow(d) }, `doc:${d.qbo_id}`);
      }
    }
  }
  // 5. document number prefix
  for (const d of docs) {
    if (results.length >= cap) break;
    if (d.doc_number && d.doc_number.toLowerCase().startsWith(qLower)) {
      add("DocumentNumberPrefix", { kind: "Document", ...toDocumentRow(d) }, `doc:${d.qbo_id}`);
    }
  }
  // 6. full text — contacts, items, then document memos/line descriptions
  if (results.length < cap) {
    for (const c of contacts) {
      if (results.length >= cap) break;
      const hay = `${c.display_name} ${c.company_name ?? ""}`.toLowerCase();
      if (hay.includes(qLower)) {
        add("Text", { kind: "Contact", contact_type: c.contact_type, qbo_id: c.qbo_id, display_name: c.display_name, company_name: c.company_name, balance: c.balance }, `contact:${c.contact_type}:${c.qbo_id}`);
      }
    }
    for (const it of items) {
      if (results.length >= cap) break;
      const hay = `${it.name} ${it.description ?? ""}`.toLowerCase();
      if (hay.includes(qLower)) {
        add("Text", { kind: "Item", qbo_id: it.qbo_id, name: it.name, sku: it.sku, item_type: it.item_type, unit_price: it.unit_price }, `item:${it.qbo_id}`);
      }
    }
    for (const d of docs) {
      if (results.length >= cap) break;
      const hay = `${d.customer_memo ?? ""} ${d.lines.map((l) => l.description ?? "").join(" ")}`.toLowerCase();
      if (hay.includes(qLower)) {
        add("Text", { kind: "Document", ...toDocumentRow(d) }, `doc:${d.qbo_id}`);
      }
    }
  }

  return results;
}

// ---------------------------------------------------------------------------
// lineage — `store/lineage.rs`'s breadth-first walk over `document_links`.
// ---------------------------------------------------------------------------

function allEdges(realm) {
  const edges = [];
  for (const d of realm.documents) {
    for (const link of d.links) {
      edges.push({
        from_qbo_id: d.qbo_id,
        from_type: d.doc_type,
        to_qbo_id: link.to_qbo_id,
        to_type: link.to_type,
        line_no: link.line_no ?? null,
      });
    }
  }
  return edges;
}

function lineage(realm, rootId, maxDepth = 8) {
  const edges = allEdges(realm);
  const visited = [rootId];
  let frontier = [rootId];
  const foundEdges = [];

  for (let depth = 0; depth < maxDepth; depth += 1) {
    const next = [];
    for (const current of frontier) {
      for (const edge of edges) {
        if (edge.from_qbo_id !== current && edge.to_qbo_id !== current) continue;
        const neighbour = edge.from_qbo_id === current ? edge.to_qbo_id : edge.from_qbo_id;
        if (!foundEdges.some((e) => e.from_qbo_id === edge.from_qbo_id && e.to_qbo_id === edge.to_qbo_id && e.line_no === edge.line_no)) {
          foundEdges.push(edge);
        }
        if (!visited.includes(neighbour)) {
          visited.push(neighbour);
          next.push(neighbour);
        }
      }
    }
    if (next.length === 0) break;
    frontier = next;
  }

  const documents = [];
  const unresolved = [];
  for (const id of visited) {
    const doc = realm.documents.find((d) => d.qbo_id === id);
    if (doc) documents.push(toDocumentRow(doc));
    else unresolved.push(id);
  }

  return { root: rootId, documents, edges: foundEdges, unresolved };
}

// ---------------------------------------------------------------------------
// the eleven tools
// ---------------------------------------------------------------------------

function documentDetail(realm, qboId) {
  const doc = realm.documents.find((d) => d.qbo_id === qboId);
  if (!doc) throw new FixtureError(`no document ${JSON.stringify(qboId)} in this realm`);
  return {
    document: toDocumentRow(doc),
    lines: doc.lines.map(toLineRow),
    lineage: lineage(realm, qboId),
  };
}

function listDocuments(realm, docType, from, to, offset, limit) {
  const lower = from ?? "0000-01-01";
  const upper = to ?? "9999-12-31";
  const rows = realm.documents.filter((d) => (
    !d.is_deleted && d.doc_type === docType && d.txn_date >= lower && d.txn_date <= upper
  ));
  return paginate(sortDocsDesc(rows), offset, limit).map(toDocumentRow);
}

function openDocuments(realm, docType, offset, limit) {
  const rows = realm.documents.filter((d) => {
    if (d.is_deleted || d.doc_type !== docType) return false;
    if (toCents(d.balance) > 0) return true;
    if (docType === "PurchaseOrder" && d.doc_status === "Open") return true;
    if (docType === "Estimate" && (d.doc_status === "Pending" || d.doc_status === "Accepted")) return true;
    return false;
  });
  return paginate(sortDocsDesc(rows), offset, limit).map(toDocumentRow);
}

function contactDetail(realm, contactType, qboId) {
  const list = contactType === "customer" ? realm.customers : contactType === "vendor" ? realm.vendors : null;
  if (!list) throw new FixtureError(`contact_type must be "customer" or "vendor", got ${JSON.stringify(contactType)}`);
  const contact = list.find((c) => c.qbo_id === qboId);
  if (!contact) throw new FixtureError(`no contact ${JSON.stringify(qboId)} in this realm`);

  const wireType = contactType === "customer" ? "Customer" : "Vendor";
  const rows = realm.documents.filter((d) => !d.is_deleted && d.contact_type === wireType && d.contact_id === qboId);
  const open = rows.filter((d) => toCents(d.balance) > 0);
  const recent = sortDocsDesc(rows).slice(0, 50);

  return {
    contact: { contact_type: wireType, qbo_id: contact.qbo_id, display_name: contact.display_name, company_name: contact.company_name, email: contact.email, phone: contact.phone, balance: contact.balance, is_active: contact.is_active },
    open_documents: sortDocsDesc(open).map(toDocumentRow),
    recent_documents: recent.map(toDocumentRow),
  };
}

function itemDetail(realm, qboId) {
  const it = realm.items.find((i) => i.qbo_id === qboId);
  if (!it) throw new FixtureError(`no item ${JSON.stringify(qboId)} in this realm`);

  const whereUsed = realm.documents.filter((d) => !d.is_deleted && d.lines.some((l) => l.item_id === qboId));
  let unitsSold = 0;
  for (const d of whereUsed) {
    if (d.doc_type !== "Invoice" && d.doc_type !== "SalesReceipt") continue;
    for (const l of d.lines) {
      if (l.item_id !== qboId) continue;
      const qty = Number(l.qty);
      if (Number.isFinite(qty)) unitsSold += qty;
    }
  }

  return {
    item: { qbo_id: it.qbo_id, name: it.name, sku: it.sku, description: it.description, item_type: it.item_type, unit_price: it.unit_price, purchase_cost: it.purchase_cost, qty_on_hand: it.qty_on_hand, income_account_id: it.income_account_id, expense_account_id: it.expense_account_id, asset_account_id: it.asset_account_id, is_active: it.is_active },
    where_used: sortDocsDesc(whereUsed).map(toDocumentRow),
    units_sold: String(unitsSold),
  };
}

function agingReport(realm, docType, asOf) {
  const open = realm.documents.filter((d) => !d.is_deleted && d.doc_type === docType && toCents(d.balance) > 0);
  const byContact = new Map();

  for (const d of open) {
    const due = d.due_date ?? d.txn_date;
    const bucket = bucketFor(asOf, due);
    const key = d.contact_id ?? "";
    if (!byContact.has(key)) {
      byContact.set(key, { contact_id: key, contact_name: d.contact_name ?? key, buckets: emptyBuckets() });
    }
    byContact.get(key).buckets[bucket] += toCents(d.balance);
  }

  const rows = [...byContact.values()]
    .map((r) => ({ contact_id: r.contact_id, contact_name: r.contact_name, buckets: bucketsToMoney(r.buckets), total: fromCents(bucketsTotal(r.buckets)) }))
    .sort((a, b) => {
      const diff = toCents(b.total) - toCents(a.total);
      return diff !== 0 ? diff : a.contact_id.localeCompare(b.contact_id);
    });

  const totalBuckets = emptyBuckets();
  for (const r of byContact.values()) {
    for (const key of Object.keys(totalBuckets)) totalBuckets[key] += r.buckets[key];
  }

  return { as_of: asOf, rows, totals: bucketsToMoney(totalBuckets) };
}

function syncStatus(realm) {
  const countByType = {
    Class: realm.classes.length,
    Account: realm.accounts.length,
    Customer: realm.customers.length,
    Vendor: realm.vendors.length,
    Item: realm.items.length,
    CompanyInfo: 1,
  };
  for (const d of realm.documents) {
    countByType[d.doc_type] = (countByType[d.doc_type] ?? 0) + 1;
  }

  const entities = ENTITY_TYPES.map((entity_type) => {
    const mirrored = countByType[entity_type] ?? 0;
    const isPeripheral = entity_type === "Department" || entity_type === "Preferences" || entity_type === "Attachable";
    const isUnmodeled = entity_type === "TaxCode" || entity_type === "TaxRate" || entity_type === "Term";
    const neverSynced = isPeripheral || (isUnmodeled && mirrored === 0);
    return {
      entity_type,
      mirrored,
      last_cdc_cursor: neverSynced ? null : realm.lastCdcCursor,
      last_full_sweep: neverSynced ? null : realm.lastFullSweep,
      quarantined: realm.quarantined[entity_type] ?? 0,
    };
  });

  const quarantined_total = entities.reduce((a, e) => a + e.quarantined, 0);

  return { write_enabled: realm.writeEnabled, quarantined_total, entities };
}

// ---------------------------------------------------------------------------
// the fixture provider
// ---------------------------------------------------------------------------

async function callTool(tool, args) {
  const realm = realmData(args.realm_id);
  switch (tool) {
    case "search":
      return search(realm, args.query, args.limit);
    case "document_detail":
      return documentDetail(realm, args.qbo_id);
    case "list_documents":
      return listDocuments(realm, args.doc_type, args.from, args.to, args.offset, args.limit);
    case "open_documents":
      return openDocuments(realm, args.doc_type, args.offset, args.limit);
    case "contact_detail":
      return contactDetail(realm, args.contact_type, args.qbo_id);
    case "item_detail":
      return itemDetail(realm, args.qbo_id);
    case "ar_aging":
      return agingReport(realm, "Invoice", args.as_of);
    case "ap_aging":
      return agingReport(realm, "Bill", args.as_of);
    case "sync_status":
      return syncStatus(realm);
    case "class_tree":
      return realm.classes;
    case "chart_of_accounts":
      return realm.accounts;
    default:
      throw new FixtureError(`unknown tool: ${tool}`);
  }
}

/** The provider backed by the in-memory fixture above. */
export function createFixtureProvider(initialRealmId = DEFAULT_REALM_ID) {
  return createProvider(callTool, initialRealmId);
}
