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

// Exact decimal arithmetic for the editor's line amounts, on scaled BigInts.
// Mirrors the server's LineExtension rounding (half away from zero, to the
// cent) so the amount a person sees is the amount that will be sent.
function toScaled(raw: string): { value: bigint; scale: number } | null {
  const text = raw.trim().replace(/,/g, "");
  if (!/^-?\d*(\.\d*)?$/.test(text) || text === "" || text === "-" || text === ".") return null;
  const negative = text.startsWith("-");
  const [whole, frac = ""] = text.replace("-", "").split(".");
  const value = BigInt((whole || "0") + frac) * (negative ? -1n : 1n);
  return { value, scale: frac.length };
}

export function lineAmountMinor(qty: string, rate: string): number | null {
  const q = toScaled(qty);
  const r = toScaled(rate);
  if (!q || !r) return null;
  const product = q.value * r.value;
  const scale = q.scale + r.scale;
  if (scale <= 2) return Number(product * 10n ** BigInt(2 - scale));
  const divisor = 10n ** BigInt(scale - 2);
  const negative = product < 0n;
  const abs = negative ? -product : product;
  let cents = abs / divisor;
  if ((abs % divisor) * 2n >= divisor) cents += 1n;
  return Number(negative ? -cents : cents);
}

// A decimal string as stored (a cost rate, a board-foot count), grouped and
// trimmed, never passed through a float.
export function formatQty(raw: string | null | undefined, places = 4): string {
  if (raw === null || raw === undefined || raw === "") return "";
  const negative = raw.startsWith("-");
  const [whole, frac = ""] = raw.replace("-", "").split(".");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  const trimmed = frac.slice(0, places).replace(/0+$/, "");
  return `${negative ? "−" : ""}${grouped}${trimmed ? "." + trimmed : ""}`;
}

// A cost rate in dollars: at least two places, up to four.
export function formatRate(raw: string | null | undefined): string {
  if (raw === null || raw === undefined || raw === "") return "";
  const negative = raw.startsWith("-");
  const [whole, frac = ""] = raw.replace("-", "").split(".");
  const places = frac.slice(0, 4).replace(/0+$/, "").padEnd(2, "0");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return `${negative ? "−" : ""}${grouped}.${places}`;
}

// Landed cost per board-foot: always four places, so a column lines up.
export function formatPerBf(raw: string | null | undefined): string {
  if (raw === null || raw === undefined || raw === "") return "";
  const [whole, frac = ""] = raw.split(".");
  return `${whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",")}.${frac.slice(0, 4).padEnd(4, "0")}`;
}
