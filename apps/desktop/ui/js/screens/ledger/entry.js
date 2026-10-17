// Document entry, the pure half: what each entry kind asks for, turning a
// draft into the `document` `create_document` takes (`docs/MCP.md`), and the
// running totals the form shows while it is being typed. No DOM, so
// `test/ledger-entry.test.js` exercises it head-on.
//
// The form only ever sends what a person typed. The number, due date, header
// class and sales tax are the book's to fill (`books.rs::prepare_document`),
// and the totals here are a preview in integer cents: the saved document's
// own `total` and `tax`, as `create_document` returns them, are the ones the
// screen reports once it is saved.

/** The kinds the screen enters, in the order it offers them. `contact` is
 * which master list the counterparty comes from; `lines` whether it has
 * item/account lines (a payment has applications instead); `taxed` whether
 * the book adds NJ sales tax (`books.rs::is_taxed_sale`); `numbered` whether
 * the book allocates the number (`books.rs::is_numbered` — a bill carries
 * the vendor's own); `accountLines` whether a line may name an account
 * instead of an item; `bank` the money account the document names
 * (`deposit_to` in, `pay_from` out) and the accounts offered for it, the
 * first being what the book uses when none is named; `applies` the book a
 * payment settles and the kind it applies to. */
const DEPOSIT_TO = { field: "deposit_to", label: "Deposit to", accounts: [
  { id: "", label: "1150 Undeposited funds" },
  { id: "1100", label: "1100 Checking" },
] };
const PAY_FROM = { field: "pay_from", label: "Pay from", required: true, accounts: [
  { id: "1100", label: "1100 Checking" },
] };

export const ENTRY_KINDS = Object.freeze({
  Invoice: { label: "Invoice", contact: "customer", lines: true, taxed: true, numbered: true, accountLines: false, posts: true },
  SalesReceipt: { label: "Sales receipt", contact: "customer", lines: true, taxed: true, numbered: true, accountLines: false, posts: true, bank: DEPOSIT_TO },
  Estimate: { label: "Estimate", contact: "customer", lines: true, taxed: true, numbered: true, accountLines: false, posts: false },
  CreditMemo: { label: "Credit memo", contact: "customer", lines: true, taxed: true, numbered: true, accountLines: false, posts: true },
  Payment: { label: "Receive payment", noun: "payment", contact: "customer", lines: false, taxed: false, numbered: false, accountLines: false, posts: true, bank: DEPOSIT_TO, applies: { book: "receivable", kind: "Invoice" } },
  PurchaseOrder: { label: "Purchase order", contact: "vendor", lines: true, taxed: false, numbered: true, accountLines: false, posts: false },
  Bill: { label: "Bill", contact: "vendor", lines: true, taxed: false, numbered: false, accountLines: true, posts: true },
  BillPayment: { label: "Pay bills", noun: "bill payment", contact: "vendor", lines: false, taxed: false, numbered: false, accountLines: false, posts: true, bank: PAY_FROM, applies: { book: "payable", kind: "Bill" } },
});

export function isEntryKind(kind) {
  return Object.prototype.hasOwnProperty.call(ENTRY_KINDS, kind);
}

/** `PostingConfig::default().sales_tax_rate`, over 100000. */
const NJ_RATE_PER_100K = 6625;

const MONEY_RE = /^-?\d+(\.\d{1,2})?$/;
const QTY_RE = /^\d+(\.\d{1,4})?$/;

/** "12.5" -> 1250. Throws on anything that isn't a plain decimal with at most
 * `places` places, so a typo can't become a silently rounded amount (D9). */
function scaled(text, places, re, what) {
  const t = String(text ?? "").trim();
  if (!re.test(t)) throw new Error(`${what} must be a number with at most ${places} decimal places, got ${JSON.stringify(t)}`);
  const negative = t.startsWith("-");
  const [whole, frac = ""] = t.replace("-", "").split(".");
  const n = Number(whole) * 10 ** places + Number(frac.padEnd(places, "0"));
  return negative ? -n : n;
}

export function toCents(text) {
  return scaled(text, 2, MONEY_RE, "an amount");
}

export function fromCents(cents) {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(cents);
  return `${sign}${Math.floor(abs / 100)}.${String(abs % 100).padStart(2, "0")}`;
}

/** Quantity × rate, half away from zero to the cent: what a line's amount
 * defaults to when the person types a quantity and a rate. `null` while
 * either is blank. */
export function lineAmount(qty, rate) {
  if (String(qty ?? "").trim() === "" || String(rate ?? "").trim() === "") return null;
  const q = scaled(qty, 4, QTY_RE, "a quantity");
  const r = toCents(rate);
  const product = q * r; // cents × 10^4
  const sign = product < 0 ? -1 : 1;
  const abs = Math.abs(product);
  return fromCents(sign * Math.floor((abs + 5000) / 10000));
}

/** The amount a line contributes: what was typed in the amount column,
 * else quantity × rate, else nothing. */
export function effectiveAmount(line) {
  if (String(line.amount ?? "").trim() !== "") return fromCents(toCents(line.amount));
  return lineAmount(line.qty, line.rate);
}

/** `books.rs::sales_tax`'s rounding: half to even on the cent. */
function taxOn(baseCents) {
  const num = baseCents * NJ_RATE_PER_100K;
  const sign = num < 0 ? -1 : 1;
  const abs = Math.abs(num);
  let q = Math.floor(abs / 100000);
  const r = abs % 100000;
  if (r * 2 > 100000 || (r * 2 === 100000 && q % 2 === 1)) q += 1;
  return sign * q;
}

/**
 * The form's running totals. Lines with no amount yet are left out rather
 * than failing the preview; `buildDocument` is where a blank line is an
 * error.
 * @param {string} kind
 * @param {{ amount?: string, qty?: string, rate?: string, taxable?: boolean }[]} lines
 * @param {{ is_tax_exempt?: boolean } | null} contact
 * @param {{ amount?: string }[]} [applications]
 * @returns {{ subtotal: string, tax: string, total: string, exempt: boolean }}
 */
export function previewTotals(kind, lines, contact, applications = []) {
  const spec = ENTRY_KINDS[kind];
  if (!spec.lines) {
    const cents = applications.reduce((sum, a) => {
      const t = String(a.amount ?? "").trim();
      return sum + (MONEY_RE.test(t) ? toCents(t) : 0);
    }, 0);
    return { subtotal: fromCents(cents), tax: "0.00", total: fromCents(cents), exempt: false };
  }
  let subtotal = 0;
  let taxable = 0;
  for (const line of lines) {
    let amount;
    try {
      amount = effectiveAmount(line);
    } catch {
      continue;
    }
    if (amount == null) continue;
    const cents = toCents(amount);
    subtotal += cents;
    if (spec.taxed && line.taxable) taxable += cents;
  }
  const exempt = Boolean(contact?.is_tax_exempt);
  const tax = spec.taxed && !exempt ? taxOn(taxable) : 0;
  return { subtotal: fromCents(subtotal), tax: fromCents(tax), total: fromCents(subtotal + tax), exempt };
}

const ISO_DATE_RE = /^\d{4}-\d{2}-\d{2}$/;

/**
 * A draft as the form holds it, as the `document` `create_document` takes.
 * Throws a plain `Error` naming what is missing; the screen shows the
 * message and sends nothing.
 * @param {string} kind
 * @param {{
 *   txnDate: string, contactId: string, number?: string, memo?: string,
 *   lines?: { itemId?: string, account?: string, description?: string, qty?: string, rate?: string, amount?: string, cls?: string, taxable?: boolean }[],
 *   applications?: { documentId: string, kind?: string, amount?: string }[],
 *   bank?: string,
 * }} draft
 */
export function buildDocument(kind, draft) {
  const spec = ENTRY_KINDS[kind];
  if (!spec) throw new Error(`not an entry kind: ${JSON.stringify(kind)}`);
  if (!ISO_DATE_RE.test(draft.txnDate ?? "")) throw new Error("a date is required");
  if (!draft.contactId) throw new Error(`choose a ${spec.contact}`);

  const document = {
    kind,
    txn_date: draft.txnDate,
    contact: { kind: spec.contact === "customer" ? "Customer" : "Vendor", id: draft.contactId },
  };
  const number = (draft.number ?? "").trim();
  if (number) document.number = number;
  const memo = (draft.memo ?? "").trim();
  if (memo) document.memo = memo;

  if (spec.lines) {
    const typed = (draft.lines ?? []).filter((l) => !isBlankLine(l));
    if (typed.length === 0) throw new Error("add at least one line");
    document.lines = typed.map((l, i) => {
      const where = `line ${i + 1}`;
      const itemId = (l.itemId ?? "").trim();
      const account = (l.account ?? "").trim();
      if (!itemId && !(spec.accountLines && account)) {
        throw new Error(`${where}: choose an item${spec.accountLines ? " or an account" : ""}`);
      }
      let amount;
      try {
        amount = effectiveAmount(l);
      } catch (err) {
        throw new Error(`${where}: ${err.message}`);
      }
      if (amount == null) throw new Error(`${where}: an amount, or a quantity and a rate, is required`);
      // A line read back from a saved document keeps its own kind (a
      // shipping or discount line stays one); a typed line is an item or an
      // account line.
      const line = { kind: l.kind ?? (itemId ? "Item" : "Account"), amount, is_taxable: spec.taxed && Boolean(l.taxable) };
      if (itemId) line.item_id = itemId;
      else line.account = account;
      const qty = String(l.qty ?? "").trim();
      if (qty) line.qty = qty;
      const description = (l.description ?? "").trim();
      if (description) line.description = description;
      const cls = (l.cls ?? "").trim();
      if (cls) line.class = cls;
      return line;
    });
  } else {
    const applications = [];
    for (const a of draft.applications ?? []) {
      const t = String(a.amount ?? "").trim();
      if (t === "") continue;
      let cents;
      try {
        cents = toCents(t);
      } catch (err) {
        throw new Error(`${a.number ?? a.documentId}: ${err.message}`);
      }
      if (cents === 0) continue;
      if (cents < 0) throw new Error(`${a.number ?? a.documentId}: a payment applies a positive amount`);
      applications.push({ target_document_id: a.documentId, target_kind: a.kind ?? spec.applies.kind, amount: fromCents(cents) });
    }
    if (applications.length === 0) throw new Error(`apply the payment to at least one open ${spec.applies.kind === "Bill" ? "bill" : "invoice"}`);
    document.applications = applications;
  }
  if (spec.bank) {
    const account = draft.bank ?? spec.bank.accounts[0].id;
    if (account) document[spec.bank.field] = account;
    else if (spec.bank.required) throw new Error(`choose an account to ${spec.bank.label.toLowerCase()}`);
  }
  return document;
}

export function isBlankLine(l) {
  return ["itemId", "account", "description", "qty", "rate", "amount"].every((k) => String(l[k] ?? "").trim() === "");
}

/** A fresh line for an item: rate from its sales price (or purchase cost on
 * a purchase), taxable from the item. */
export function lineForItem(kind, item) {
  const purchase = ENTRY_KINDS[kind].contact === "vendor";
  const rate = purchase ? (item.purchase_cost ?? item.unit_cost) : item.sales_price;
  return {
    itemId: item.item_id,
    description: item.description ?? item.name,
    qty: "1",
    rate: rate ?? "",
    amount: "",
    cls: "",
    taxable: !purchase && Boolean(item.is_taxable),
  };
}

/** The number a quantity was entered as, else `null`. */
function qtyOrNull(qty) {
  const t = String(qty ?? "").trim();
  return QTY_RE.test(t) && Number(t) !== 0 ? t : null;
}

/**
 * A saved document (`get_document`) as the form's draft, so editing is the
 * same form as entering. A line's rate is recovered when quantity × rate
 * gives back the saved amount exactly, so changing the quantity reprices
 * the line; otherwise the amount stays as typed. Throws for a document with
 * lines this form cannot show faithfully (journal lines).
 * @param {object} doc `get_document`'s result
 */
export function draftFromDocument(doc) {
  const spec = ENTRY_KINDS[doc.kind];
  if (!spec) throw new Error(`this form does not edit a ${doc.kind}`);
  if ((doc.lines ?? []).some((l) => l.kind === "Journal" || l.kind === "Description")) {
    throw new Error(`${doc.number ?? doc.document_id} has lines this form does not edit`);
  }
  const lines = (doc.lines ?? []).map((l) => {
    const line = {
      itemId: l.item_id ?? "", account: l.account ?? "", description: l.description ?? "",
      qty: l.qty ?? "", rate: "", amount: l.amount, cls: l.class ?? "", taxable: Boolean(l.is_taxable),
    };
    if (l.kind !== "Item" && l.kind !== "Account") line.kind = l.kind;
    const qty = qtyOrNull(l.qty);
    if (qty) {
      const cents = toCents(l.amount);
      const rate = fromCents(Math.round(cents / Number(qty)));
      if (lineAmount(qty, rate) === fromCents(cents)) {
        line.rate = rate;
        line.amount = "";
      }
    }
    return line;
  });
  const bankAccount = spec.bank ? doc[spec.bank.field] ?? "" : "";
  return {
    documentId: doc.document_id,
    txnDate: doc.txn_date,
    contactId: doc.contact?.id ?? "",
    number: doc.number ?? "",
    memo: doc.memo ?? "",
    bank: bankAccount,
    lines: lines.length ? lines : [{ itemId: "", account: "", description: "", qty: "", rate: "", amount: "", cls: "", taxable: false }],
    applications: (doc.applications ?? []).map((a) => ({ documentId: a.target_document_id, kind: a.target_kind, amount: a.amount })),
  };
}

/** A draft being edited, as the `document` `update_document` takes: what
 * `buildDocument` sends, plus the saved `document_id` and the number it
 * keeps. */
export function buildEditedDocument(kind, draft) {
  if (!draft.documentId) throw new Error("not a saved document");
  const document = buildDocument(kind, draft);
  return { document_id: draft.documentId, ...document };
}
