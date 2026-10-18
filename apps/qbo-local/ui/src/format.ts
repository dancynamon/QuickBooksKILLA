// Exact money formatting from integer minor units. No float ever touches an
// amount: 1234567 becomes "12,345.67" by integer arithmetic alone.
export function formatMoney(minor: number | null | undefined): string {
  if (minor === null || minor === undefined) return "";
  const negative = minor < 0;
  const abs = Math.abs(minor);
  const whole = Math.trunc(abs / 100).toString().replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const cents = (abs % 100).toString().padStart(2, "0");
  return `${negative ? "−" : ""}${whole}.${cents}`;
}

// Decimal strings from the replica (qty, unit price) are shown as stored,
// trimmed of trailing zeros past two places.
export function formatDecimal(raw: string | null): string {
  if (raw === null) return "";
  if (!raw.includes(".")) return raw;
  const [whole, frac] = raw.split(".");
  const trimmed = frac.replace(/0+$/, "");
  return trimmed.length === 0 ? whole : `${whole}.${trimmed.length < 2 ? trimmed.padEnd(2, "0") : trimmed}`;
}

export function formatDate(iso: string | null): string {
  if (!iso) return "";
  const [y, m, d] = iso.slice(0, 10).split("-");
  return `${m}/${d}/${y}`;
}

export function ago(iso: string | null, now = Date.now()): string {
  if (!iso) return "never";
  const seconds = Math.max(0, Math.round((now - Date.parse(iso)) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

export const DOC_LABELS: Record<string, string> = {
  Estimate: "Estimate",
  Invoice: "Invoice",
  SalesReceipt: "Sales receipt",
  CreditMemo: "Credit memo",
  RefundReceipt: "Refund receipt",
  Payment: "Payment",
  PurchaseOrder: "Purchase order",
  Bill: "Bill",
  BillPayment: "Bill payment",
  VendorCredit: "Vendor credit",
  Purchase: "Expense",
  Deposit: "Deposit",
  JournalEntry: "Journal entry",
};

export const DOC_PLURALS: Record<string, string> = {
  ...Object.fromEntries(Object.entries(DOC_LABELS).map(([k, v]) => [k, `${v}s`])),
  JournalEntry: "Journal entries",
  Purchase: "Expenses",
};
