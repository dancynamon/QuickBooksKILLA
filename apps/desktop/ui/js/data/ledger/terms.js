// A contact's payment terms (DECISIONS.md D41), the JS twin of
// `books::Terms`: net days, or QBO's date-driven term — due on a day of the
// month, the first such day on or after the document date, a month later
// when that day is `next_month_within` days or fewer away, and the month's
// last day when it has fewer days. Pure, so the fixture fills due dates the
// way the book does and the screens say them the same way.

/** `contact.terms` when the book sent it, else read off `due_day` (the
 * stored shape, as `books::Contact` has it) or `terms_days`. */
export function termsOf(contact) {
  if (contact?.terms && typeof contact.terms === "object") return contact.terms;
  if (contact?.due_day) {
    return { type: "day_of_month", day: contact.due_day.day, next_month_within: contact.due_day.next_month_within ?? 0 };
  }
  return { type: "net", days: contact?.terms_days ?? 0 };
}

function daysIn(year, month) {
  return new Date(Date.UTC(year, month + 1, 0)).getUTCDate();
}

function dayInMonth(year, month, day) {
  const y = year + Math.floor(month / 12);
  const m = ((month % 12) + 12) % 12;
  return new Date(Date.UTC(y, m, Math.min(Math.max(day, 1), daysIn(y, m))));
}

const iso = (date) => date.toISOString().slice(0, 10);
const DAY_MS = 86_400_000;

/** The due date of a document dated `txnDate` ("YYYY-MM-DD"). */
export function termsDueDate(terms, txnDate) {
  const txn = new Date(`${txnDate}T00:00:00Z`);
  if (terms.type !== "day_of_month") {
    return iso(new Date(txn.getTime() + Math.max(terms.days ?? 0, 0) * DAY_MS));
  }
  let year = txn.getUTCFullYear();
  let month = txn.getUTCMonth();
  let due = dayInMonth(year, month, terms.day);
  if (due < txn) due = dayInMonth(year, ++month, terms.day);
  if ((due - txn) / DAY_MS <= (terms.next_month_within ?? 0)) due = dayInMonth(year, ++month, terms.day);
  return iso(due);
}

function ordinal(n) {
  const suffix = n % 100 >= 11 && n % 100 <= 13 ? "th" : ({ 1: "st", 2: "nd", 3: "rd" })[n % 10] ?? "th";
  return `${n}${suffix}`;
}

/** "Net 30", "Due on receipt", "Due the 1st", "Due the last of the month". */
export function termsLabel(terms) {
  if (terms.type === "day_of_month") {
    return terms.day >= 31 ? "Due the last of the month" : `Due the ${ordinal(terms.day)}`;
  }
  return terms.days > 0 ? `Net ${terms.days}` : "Due on receipt";
}
