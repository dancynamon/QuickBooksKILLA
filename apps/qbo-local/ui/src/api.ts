// The wire contract with `apps/qbo-local/src/api.rs`. Field names here are the
// serde camelCase names there; a rename on either side should be made on both.
//
// Money is integer minor units. It is never converted to a float for display:
// see `formatMoney`.

export type DocType =
  | "Estimate" | "Invoice" | "SalesReceipt" | "CreditMemo" | "RefundReceipt" | "Payment"
  | "PurchaseOrder" | "Bill" | "BillPayment" | "VendorCredit" | "Purchase" | "Deposit"
  | "JournalEntry";

export type ContactType = "Customer" | "Vendor";

export interface Realm {
  realmId: string;
  displayName: string;
  writeEnabled: boolean;
  environment: "production" | "sandbox";
}

export interface Document {
  qboId: string;
  docType: DocType;
  docNumber: string | null;
  txnDate: string;
  dueDate: string | null;
  contactId: string | null;
  contactType: ContactType | null;
  contactName: string | null;
  classId: string | null;
  totalMinor: number;
  balanceMinor: number | null;
  status: string | null;
  poNumber: string | null;
  privateNote: string | null;
  customerMemo: string | null;
  /** Set when the document exists only in the outbox: its write state. */
  pendingWrite?: string;
}

export interface Line {
  lineNo: number;
  itemId: string | null;
  description: string | null;
  qty: string | null;
  unitPrice: string | null;
  amountMinor: number;
  classId: string | null;
  taxable: boolean;
}

export interface Link {
  fromQboId: string;
  fromType: string;
  toQboId: string;
  toType: string;
}

export interface DocumentDetail {
  document: Document;
  lines: Line[];
  related: Document[];
  links: Link[];
  unresolved: string[];
  openWrites: Write[];
}

export interface Write {
  id: string;
  entityType: string;
  operation: "create" | "update";
  entityId: string;
  label: string;
  state: "pending" | "in_flight" | "conflicted" | "rejected" | "applied" | "discarded";
  status: string;
  attempts: number;
  lastError: string | null;
  blockedBy: string | null;
  createdAt: string;
  updatedAt: string;
}

export interface DraftLine {
  itemId: string | null;
  description: string | null;
  qty: string;
  rate: string;
  classId: string | null;
  taxable: boolean;
}

export interface Draft {
  docType: DocType;
  /** The customer on a sales document, the vendor on a purchase. */
  contactId: string;
  txnDate: string;
  dueDate: string | null;
  docNumber: string | null;
  poNumber: string | null;
  privateNote: string | null;
  customerMemo: string | null;
  classId: string | null;
  lines: DraftLine[];
}

export interface Customer {
  qboId: string;
  displayName: string;
  companyName: string | null;
  pending: boolean;
}

export type Hit =
  | { kind: "document"; reason: string; document: Document }
  | { kind: "contact"; reason: string; contactType: ContactType; qboId: string; displayName: string; companyName: string | null; balanceMinor: number | null }
  | { kind: "item"; reason: string; qboId: string; name: string; sku: string | null; unitPrice: string | null };

export interface SearchResult {
  hits: Hit[];
  elapsedMicros: number;
}

export interface EntityStatus {
  entityType: string;
  count: number;
  lastCdcCursor: string | null;
  lastFullSweep: string | null;
}

export interface Status {
  environment: "production" | "sandbox";
  entityTypes: EntityStatus[];
  lastSynced: string | null;
  outboxPending: number;
  outboxNeedsAttention: number;
  quarantined: number;
  writeEnabled: boolean;
}

export interface AgingRow {
  contactId: string;
  contactName: string | null;
  currentMinor: number;
  days1to30Minor: number;
  days31to60Minor: number;
  days61to90Minor: number;
  over90Minor: number;
  totalMinor: number;
  documents: number;
}

export interface Aging {
  asOf: string;
  rows: AgingRow[];
}

export interface SalesRow {
  contactId: string;
  contactName: string | null;
  netSalesMinor: number;
  documents: number;
}

export interface Item {
  qboId: string;
  name: string;
  sku: string | null;
  itemType: string | null;
  unitPrice: string | null;
  purchaseCost: string | null;
  qtyOnHand: string | null;
  active: boolean;
  usedOn: number;
}

// Manufacturing. Decimals arrive as exact strings and are shown as such.
export interface MaterialRow {
  materialId: string;
  name: string;
  thicknessIn: string;
  widthIn: string;
  lengthIn: string;
  color: string | null;
  bfPerSheet: string;
  landedPerBf: string | null;
  lastReceived: string | null;
  receipts: number;
}

export interface ReceiptLineRow {
  materialId: string;
  sheets: string;
  bfPerSheet: string;
  pricePerSheet: string;
  surchargePerSheet: string;
  goodsMinor: number;
  freightMinor: number;
  landedPerBf: string;
}

export interface ReceiptRow {
  receiptId: string;
  receivedOn: string;
  vendorId: string | null;
  reference: string | null;
  freightMinor: number;
  lines: ReceiptLineRow[];
}

export type Component =
  | { kind: "material"; materialId: string; netBf: string; yieldPct: string }
  | { kind: "part"; name: string; itemId: string | null; qty: string; unitCost: string }
  | { kind: "labour"; name: string; minutes: string; ratePerHour: string };

export interface SheetSummary {
  itemId: string;
  name: string;
  sku: string | null;
  price: string | null;
  unitCost: string | null;
  materialCost: string | null;
  wasteCost: string | null;
  marginPct: string | null;
  problem: string | null;
  components: number;
  updatedAt: string;
}

export interface CostLine {
  label: string;
  grossBf: string | null;
  wasteBf: string | null;
  cost: string;
}

export interface UnitCost {
  lines: CostLine[];
  material: string;
  parts: string;
  labour: string;
  total: string;
  waste: string;
}

export interface SensitivityCell {
  priceMovePct: string;
  yieldMovePts: string;
  unitCost: string;
  marginPct: string | null;
}

export interface BuildRow {
  buildId: string;
  itemId: string;
  name?: string | null;
  builtOn: string;
  quantity: string;
  standardMinor: number;
  actualMinor: number;
  varianceMinor: number;
  flagged: boolean;
  note: string | null;
  actual: { materialId: string; boardFeet: string }[];
}

export interface StockRow {
  materialId: string;
  name: string;
  onHandBf: string;
  sheetsOnHand: string;
  averagePerBf: string | null;
  latestPerBf: string | null;
  value: number;
  reorderBf: string | null;
  low: boolean;
  wentNegative: boolean;
  shrinkageValue: number;
}

export interface StockMove {
  date: string;
  kind: "received" | "used" | "counted";
  reference: string;
  changeBf: string;
  onHandBf: string;
  averagePerBf: string;
  valueChange: number;
}

export type CostBasis = "latest" | "average";

export interface ProductStockRow {
  itemId: string;
  name: string | null;
  sku: string | null;
  qboOnHand: string | null;
  tracked: boolean;
  since?: string;
  onHand?: string;
  averageCost?: string | null;
  value?: number;
  netSold?: string;
  wentNegative?: boolean;
}

export type ImportKind = "materials" | "buildSheets" | "counts";

export interface ImportResult {
  ok: boolean;
  committed?: boolean;
  rows: Record<string, unknown>[];
  errors: { line: number; message: string }[];
}

export interface SheetDetail {
  item: { qboId: string; name: string; sku: string | null; unitPrice: string | null };
  components: Component[];
  cost: UnitCost | null;
  marginPct: string | null;
  problem: string | null;
  sensitivity: SensitivityCell[];
  builds: BuildRow[];
}

declare global {
  interface Window {
    __TAURI__?: { core: { invoke: (cmd: string, args: unknown) => Promise<unknown> } };
  }
}

export class ApiError extends Error {}

// Tauri when running in the shell, the loopback dev server otherwise. Both end
// at the same Rust `dispatch`, so the UI cannot tell them apart.
async function call<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  const tauri = window.__TAURI__;
  if (tauri) {
    try {
      return (await tauri.core.invoke("api", { command, args })) as T;
    } catch (e) {
      throw new ApiError(String(e));
    }
  }
  const response = await fetch(`/api/${command}`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(args),
  });
  const body = await response.json().catch(() => ({ error: response.statusText }));
  if (!response.ok) throw new ApiError(body.error ?? response.statusText);
  return body as T;
}

export type OpenInvoice = {
  qboId: string;
  docNumber: string | null;
  txnDate: string;
  dueDate: string | null;
  balanceMinor: number;
  /// Already on its way to QuickBooks in a queued payment.
  queuedMinor: number;
};

export type PaymentDraft = {
  customerId: string;
  txnDate: string;
  amount: string;
  reference: string | null;
  privateNote: string | null;
  /// Absent: spread oldest-first.
  applications?: { invoiceId: string; amount: string }[];
};

export type PaymentPlan = {
  totalMinor: number;
  unappliedMinor: number;
  applied: { invoiceId: string; docNumber: string | null; amountMinor: number }[];
};

export const api = {
  realms: () => call<Realm[]>("realms"),
  status: (realm: string) => call<Status>("status", { realm }),
  search: (realm: string, query: string, limit = 25) => call<SearchResult>("search", { realm, query, limit }),
  aging: (realm: string, side: "receivable" | "payable", asOf?: string) => call<Aging>("aging", { realm, side, asOf }),
  salesByCustomer: (realm: string, from: string, to: string) => call<SalesRow[]>("salesByCustomer", { realm, from, to }),
  items: (realm: string) => call<Item[]>("items", { realm }),
  itemDocuments: (realm: string, itemId: string) => call<Document[]>("itemDocuments", { realm, itemId }),
  customers: (realm: string) => call<Customer[]>("customers", { realm }),
  vendors: (realm: string) => call<Customer[]>("vendors", { realm }),
  documentDraft: (realm: string, qboId: string) =>
    call<{ draft: Draft; contactName: string | null; openWrites: number }>("documentDraft", { realm, qboId }),
  openInvoices: (realm: string, customerId: string) => call<OpenInvoice[]>("openInvoices", { realm, customerId }),
  paymentPreview: (realm: string, draft: PaymentDraft) => call<PaymentPlan>("paymentPreview", { realm, draft }),
  receivePayment: (realm: string, draft: PaymentDraft) =>
    call<PaymentPlan & { outboxId: string; entityId: string }>("receivePayment", { realm, draft }),
  saveDocument: (realm: string, draft: Draft, qboId?: string) =>
    call<{ outboxId: string; entityId: string; state: string }>("saveDocument", { realm, draft, qboId }),
  createCustomer: (realm: string, displayName: string, companyName?: string, email?: string) =>
    call<{ outboxId: string; entityId: string; displayName: string }>("createCustomer", { realm, displayName, companyName, email }),
  outbox: (realm: string) => call<Write[]>("outbox", { realm }),
  resolveWrite: (realm: string, id: string, action: "retry" | "discard") => call<Write>("resolveWrite", { realm, id, action }),
  materials: (realm: string) => call<MaterialRow[]>("materials", { realm }),
  saveMaterial: (realm: string, m: { materialId?: string; name: string; thicknessIn: string; widthIn: string; lengthIn: string; color?: string }) =>
    call<{ materialId: string; bfPerSheet: string }>("saveMaterial", { realm, ...m }),
  receipts: (realm: string, materialId?: string) => call<ReceiptRow[]>("receipts", { realm, materialId }),
  recordReceipt: (
    realm: string,
    r: { receivedOn: string; reference?: string; freight: string; lines: { materialId: string; sheets: string; pricePerSheet: string; surchargePerSheet?: string }[] },
  ) => call<{ receiptId: string }>("recordReceipt", { realm, ...r }),
  buildSheets: (realm: string, basis: CostBasis = "latest") => call<SheetSummary[]>("buildSheets", { realm, basis }),
  buildSheet: (realm: string, itemId: string, basis: CostBasis = "latest") => call<SheetDetail>("buildSheet", { realm, itemId, basis }),
  stock: (realm: string) => call<StockRow[]>("stock", { realm }),
  productStock: (realm: string) => call<ProductStockRow[]>("productStock", { realm }),
  productLedger: (realm: string, itemId: string) =>
    call<{ since: string | null; stock: { moves: StockMove[]; onHandBf: string; averagePerBf: string | null; value: number } }>("productLedger", { realm, itemId }),
  recordProductCount: (realm: string, itemId: string, countedOn: string, units: string, note?: string) =>
    call<{ countId: string }>("recordProductCount", { realm, itemId, countedOn, units, note }),
  importCsv: (realm: string, kind: ImportKind, text: string, commit: boolean) => call<ImportResult>("importCsv", { realm, kind, text, commit }),
  stockLedger: (realm: string, materialId: string) => call<{ moves: StockMove[]; wentNegative: boolean }>("stockLedger", { realm, materialId }),
  recordCount: (realm: string, materialId: string, countedOn: string, amount: string, sheets: boolean, note?: string) =>
    call<{ countId: string }>("recordCount", { realm, materialId, countedOn, amount, sheets, note }),
  setReorderPoint: (realm: string, materialId: string, boardFeet: string | null) => call<unknown>("setReorderPoint", { realm, materialId, boardFeet }),
  saveBuildSheet: (realm: string, itemId: string, components: Component[]) => call<{ itemId: string }>("saveBuildSheet", { realm, itemId, components }),
  builds: (realm: string, itemId?: string) => call<BuildRow[]>("builds", { realm, itemId }),
  recordBuild: (realm: string, b: { itemId: string; builtOn: string; quantity: string; actual: { materialId: string; boardFeet: string }[]; note?: string }) =>
    call<{ buildId: string }>("recordBuild", { realm, ...b }),
  classes: (realm: string) => call<Record<string, string>>("classes", { realm }),
  documents: (realm: string, docType: DocType, limit = 50_000) => call<Document[]>("documents", { realm, docType, limit }),
  document: (realm: string, qboId: string) => call<DocumentDetail>("document", { realm, qboId }),
  contactDocuments: (realm: string, contactType: ContactType, contactId: string) =>
    call<Document[]>("contactDocuments", { realm, contactType, contactId, limit: 50_000 }),
};
