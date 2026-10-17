// Open items and aging on the own book (D28, `docs/MCP.md` "open_documents"
// / "aging"): what is still owed to Aquamentor (receivable) or by it
// (payable) as of a date, bucketed by days past due, derived from the saved
// documents and their applications. The strip checks the aging total against
// the control account on the trial balance — 1200 for receivables, 2000 for
// payables — because a sub-ledger that doesn't tie to its control account is
// the first thing the slim cutover import (D28) has to prove it isn't.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";
import { toCents } from "./entry.js";

const BOOKS = {
  receivable: { label: "Receivables", control: "1200", contact: "Customer" },
  payable: { label: "Payables", control: "2000", contact: "Vendor" },
};

const BUCKETS = [
  ["current", "Current"],
  ["days_1_30", "1–30"],
  ["days_31_60", "31–60"],
  ["days_61_90", "61–90"],
  ["over_90", "Over 90"],
];

/** The control account's balance as the aging states it: a payable's
 * credit balance comes back negative on the trial balance, positive on the
 * aging. Returns the difference in cents, 0 when it ties. */
export function controlDifference(book, agingTotal, controlBalance) {
  const control = toCents(controlBalance ?? "0.00");
  const sub = toCents(agingTotal);
  return book === "payable" ? sub + control : sub - control;
}

export async function renderOpenItems(main, book) {
  const spec = BOOKS[book] ?? BOOKS.receivable;
  if (!BOOKS[book]) book = "receivable";
  const asOf = main.dataset.openAsOf || todayIso();

  await withState(
    main,
    spec.label.toLowerCase(),
    () => Promise.all([
      ledgerProvider.aging(book, asOf),
      ledgerProvider.openDocuments(book, asOf),
      ledgerProvider.trialBalance(asOf),
    ]),
    ([aging, open, tb]) => {
      const controlRow = tb.rows.find((r) => r.account_id === spec.control || r.number === spec.control);
      const diff = controlDifference(book, aging.totals.total, controlRow?.balance);
      const ties = diff === 0;

      main.innerHTML = viewHead(
        spec.label,
        `As of ${asOf} · ${open.length} open`,
        `<div class="seg" role="group" aria-label="Book">
          ${Object.entries(BOOKS).map(([k, b]) => `<button data-book="${k}" aria-pressed="${k === book}">${esc(b.label)}</button>`).join("")}
        </div>
        <label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">As of</span>
          <input type="date" id="openAsOf" value="${esc(asOf)}" style="border:none;background:none;font:inherit;color:inherit">
        </label>
        <a class="btn btn-primary" href="#ledger-new/${book === "receivable" ? "Invoice" : "Bill"}">New ${book === "receivable" ? "invoice" : "bill"}</a>`,
      ) + `
      <div class="pad">
        <div class="strip">
          ${BUCKETS.map(([key, label]) => `<div class="strip-cell"><div class="lbl">${label}</div><div class="strip-val">${plain(aging.totals[key])}</div></div>`).join("")}
          <div class="strip-cell"><div class="lbl">Total</div><div class="strip-val">${plain(aging.totals.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Ties to ${spec.control}</div><div class="strip-val ${ties ? "pos" : "neg"}">${ties ? "yes" : `off by ${plain(String(diff / 100))}`}</div></div>
        </div>

        <div class="grid-wrap" style="margin-bottom:14px"><div class="grid-scroll"><table>
          <thead><tr><th>${spec.contact}</th>${BUCKETS.map(([, label]) => `<th class="r">${label}</th>`).join("")}<th class="r">Total</th></tr></thead>
          <tbody>${aging.rows.length === 0 ? `<tr><td colspan="7" class="dim">Nothing open.</td></tr>` : aging.rows.map((r) => `<tr>
            <td>${esc(r.contact_name)}</td>
            ${BUCKETS.map(([key]) => `<td class="r num ${key === "over_90" && Number(r[key]) ? "neg" : "dim"}">${plain(r[key], { dashForZero: true })}</td>`).join("")}
            <td class="r num" style="font-weight:500">${plain(r.total)}</td>
          </tr>`).join("")}</tbody>
        </table></div></div>

        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Type</th><th>No.</th><th>${spec.contact}</th><th>Date</th><th>Due</th><th class="r">Total</th><th class="r">Applied</th><th class="r">Open</th></tr></thead>
          <tbody>${open.map((d) => `<tr>
            <td class="dim">${esc(d.kind)}</td>
            <td class="mono">${esc(d.number ?? "—")}</td>
            <td>${esc(d.contact_name)}</td>
            <td class="dim">${esc(d.txn_date)}</td>
            <td class="${d.due_date < asOf ? "neg" : "dim"}">${esc(d.due_date)}</td>
            <td class="r num dim">${plain(d.total)}</td>
            <td class="r num dim">${plain(d.applied, { dashForZero: true })}</td>
            <td class="r num" style="font-weight:500">${plain(d.open)}</td>
          </tr>`).join("")}</tbody>
        </table></div></div>
        <p class="hint">Derived from the saved documents and the payments applied to them; nothing here is stored. ${ties ? "" : `The aging and ${spec.control} disagree: an entry is touching ${spec.control} without a document behind it, or a document posted somewhere else.`}</p>
      </div>`;

      main.querySelectorAll("[data-book]").forEach((btn) => {
        btn.addEventListener("click", () => {
          window.location.hash = `#ledger-open/${btn.dataset.book}`;
        });
      });
      main.querySelector("#openAsOf")?.addEventListener("change", (event) => {
        main.dataset.openAsOf = event.target.value;
        renderOpenItems(main, book);
      });
    },
  );
}
