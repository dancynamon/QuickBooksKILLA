// Money and date formatting. `docs/MCP.md`: every money field the provider
// returns is already a two-place decimal string ("1234.56", a credit
// "-12.00") — never bare minor units, never a float. Everything here formats
// that string; nothing here does money arithmetic, which stays server-side
// on `Money` (`ledger-core`) for the same reason it stays there in Rust.

/**
 * A decimal-string money value as a currency string: "1234.56" -> "$1,234.56",
 * "-12.00" -> "-$12.00". `null`/`undefined` (an unset balance, a document
 * with no due amount) renders as an em dash rather than "$0.00" — those are
 * different facts, and the query API already keeps them apart (D9).
 * @param {string | null | undefined} decimal
 * @param {{ dashForZero?: boolean }} [opts]
 */
export function money(decimal, opts = {}) {
  if (decimal == null) return "—";
  const n = Number(decimal);
  if (!Number.isFinite(n)) return "—";
  if (n === 0 && opts.dashForZero) return "—";
  const negative = n < 0;
  const abs = Math.abs(n).toLocaleString("en-US", {
    style: "currency",
    currency: "USD",
  });
  return negative ? `-${abs}` : abs;
}

/**
 * The same value without a currency symbol, for a dense grid column where
 * the header already says what it is: "1234.56" -> "1,234.56".
 * @param {string | null | undefined} decimal
 * @param {{ dashForZero?: boolean }} [opts]
 */
export function plain(decimal, opts = {}) {
  if (decimal == null) return "—";
  const n = Number(decimal);
  if (!Number.isFinite(n)) return "—";
  if (n === 0 && opts.dashForZero) return "—";
  return n.toLocaleString("en-US", {
    minimumFractionDigits: 2,
    maximumFractionDigits: 2,
  });
}

/**
 * A `YYYY-MM-DD` date as stored on a `DocumentRow` — rendered as-is, not
 * reparsed through `Date`, because a plain calendar date has no timezone to
 * get wrong by round-tripping it through one.
 * @param {string | null | undefined} isoDate
 */
export function date(isoDate) {
  if (!isoDate) return "—";
  return isoDate;
}

/**
 * The same date, more legible for a document header: "2026-07-14" ->
 * "14 Jul 2026". Falls back to the raw string for anything that doesn't
 * parse as `YYYY-MM-DD`, rather than showing "Invalid Date".
 * @param {string | null | undefined} isoDate
 */
export function longDate(isoDate) {
  if (!isoDate) return "—";
  const m = /^(\d{4})-(\d{2})-(\d{2})$/.exec(isoDate);
  if (!m) return isoDate;
  const months = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun",
    "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
  ];
  const [, y, mo, d] = m;
  const monthName = months[Number(mo) - 1];
  if (!monthName) return isoDate;
  return `${Number(d)} ${monthName} ${y}`;
}

/**
 * An RFC3339 timestamp (`sync_status`'s cursor fields) as "how long ago",
 * for the sync pill in the chrome. `null` (never synced) reads as "never".
 * @param {string | null | undefined} isoTimestamp
 * @param {Date} [now]
 */
export function timeAgo(isoTimestamp, now = new Date()) {
  if (!isoTimestamp) return "never";
  const then = new Date(isoTimestamp);
  if (Number.isNaN(then.getTime())) return "never";
  const seconds = Math.max(0, Math.round((now.getTime() - then.getTime()) / 1000));
  if (seconds < 5) return "just now";
  if (seconds < 60) return `${seconds}s ago`;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.round(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.round(hours / 24);
  return `${days}d ago`;
}

/** Escape text for interpolation into an HTML template string. */
export function esc(value) {
  return String(value ?? "").replace(/[&<>"]/g, (m) => (
    { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[m]
  ));
}
