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
  entityTypes: EntityStatus[];
  lastSynced: string | null;
  outboxPending: number;
  outboxNeedsAttention: number;
  quarantined: number;
  writeEnabled: boolean;
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

export const api = {
  realms: () => call<Realm[]>("realms"),
  status: (realm: string) => call<Status>("status", { realm }),
  search: (realm: string, query: string, limit = 25) => call<SearchResult>("search", { realm, query, limit }),
  classes: (realm: string) => call<Record<string, string>>("classes", { realm }),
  documents: (realm: string, docType: DocType, limit = 50_000) => call<Document[]>("documents", { realm, docType, limit }),
  document: (realm: string, qboId: string) => call<DocumentDetail>("document", { realm, qboId }),
  contactDocuments: (realm: string, contactType: ContactType, contactId: string) =>
    call<Document[]>("contactDocuments", { realm, contactType, contactId, limit: 50_000 }),
};
