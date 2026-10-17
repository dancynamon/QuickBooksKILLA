// Document entry into the own book (D28, `docs/MCP.md` "create_document"):
// invoice, sales receipt, estimate, credit memo, receive payment, purchase
// order, bill and pay bills, typed against
// the book's own customers, vendors and items. The form sends only what a
// person typed; the book fills the number, due date, header class and sales
// tax and posts it, and the screen reports what came back — the number, the
// total, the entry and the `locked_through` the write gate returned.
//
// Keyboard-first: the first field takes focus, Tab walks the line grid,
// Enter in a line's last cell adds a line, ⌘/Ctrl-Enter saves. A draft
// survives switching kinds and leaving the screen until it is saved.
//
// The same form edits a saved document (`#ledger-edit/<id>`, opened from
// Receivables/Payables; `docs/MCP.md` "get_document" / "update_document"):
// the document is read back into a draft, and saving sends a new version —
// the book reverses the old entry and posts the new one, keeping the number.
// A Void button with a reason (`void_document`) sits under it; a voided
// document opens read-only, because it stays voided.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { errorState, loadingState, viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";
import {
  ENTRY_KINDS, buildDocument, buildEditedDocument, draftFromDocument, effectiveAmount,
  fromCents, lineForItem, previewTotals, toCents,
} from "./entry.js";
import { sessionHint } from "./masters.js";
import { contactTermsLabel } from "./masterData.js";
import { termsOf } from "../../data/ledger/terms.js";

const ACTOR = "dan";

/** kind -> draft, kept for the life of the window. */
const drafts = new Map();
/** document_id -> { doc, draft, error } while it is being edited. */
const editing = new Map();

function blankLine() {
  return { itemId: "", account: "", description: "", qty: "", rate: "", amount: "", cls: "", taxable: false };
}

function draftFor(kind) {
  if (!drafts.has(kind)) {
    drafts.set(kind, {
      txnDate: todayIso(), contactId: "", number: "", memo: "",
      bank: ENTRY_KINDS[kind].bank?.accounts[0].id ?? "",
      lines: [blankLine()], applications: [],
    });
  }
  return drafts.get(kind);
}

function safeAmount(line) {
  try {
    return effectiveAmount(line);
  } catch {
    return null;
  }
}

export async function renderNewDocument(main, kind) {
  const draft = draftFor(kind);
  if (sessionHint.contactId) {
    // "Invoice" / "Bill" from the contacts list: start this draft for them.
    draft.contactId = sessionHint.contactId;
    sessionHint.contactId = null;
  }
  await renderForm(main, kind, draft, null);
}

/** `#ledger-edit/<document_id>`: a saved document, read back into the form.
 * `fresh` (arriving by navigation) reads it again rather than resuming an
 * edit left earlier, so the form never saves over a newer version. */
export async function renderEditDocument(main, documentId, { fresh = false } = {}) {
  if (fresh) {
    editing.delete(documentId);
    delete main.dataset.entryNotice;
  }
  let entry = editing.get(documentId);
  if (!entry) {
    loadingState(main, "document");
    let doc;
    try {
      doc = await ledgerProvider.getDocument(documentId);
    } catch (err) {
      errorState(main, err);
      return;
    }
    entry = { doc, draft: null, error: null };
    if (!doc.is_voided) {
      try {
        entry.draft = draftFromDocument(doc);
      } catch (err) {
        entry.error = err.message;
      }
    }
    editing.set(documentId, entry);
  }
  if (!entry.draft) {
    renderReadOnly(main, entry.doc, entry.error);
    return;
  }
  await renderForm(main, entry.doc.kind, entry.draft, entry.doc);
}

/** The applications a saved payment already made, folded back into the open
 * list it is edited against: a target it paid off is open again by what
 * this payment applied, so the edit can change it. */
async function withOwnApplications(open, saved, kind) {
  const rows = open.map((d) => ({ ...d }));
  for (const a of saved?.applications ?? []) {
    const row = rows.find((d) => d.document_id === a.target_document_id);
    if (row) {
      row.open = fromCents(toCents(row.open) + toCents(a.amount));
      continue;
    }
    const target = await ledgerProvider.getDocument(a.target_document_id);
    rows.push({
      document_id: target.document_id, kind: target.kind ?? kind, number: target.number,
      txn_date: target.txn_date, due_date: target.due_date ?? target.txn_date,
      total: target.total, open: a.amount,
    });
  }
  return rows;
}

function noticeFor(main, kind) {
  const stored = main.dataset.entryNotice ? JSON.parse(main.dataset.entryNotice) : null;
  // A notice belongs to the kind that raised it, not whatever kind the URL
  // shows next.
  return stored?.kind === kind ? stored : null;
}

async function renderForm(main, kind, draft, saved) {
  const spec = ENTRY_KINDS[kind];
  const notice = noticeFor(main, kind);

  await withState(
    main,
    spec.label.toLowerCase(),
    async () => {
      const [contacts, items] = await Promise.all([
        ledgerProvider.listContacts(spec.contact),
        spec.lines ? ledgerProvider.listItems() : Promise.resolve([]),
      ]);
      let open = [];
      if (!spec.lines && draft.contactId) {
        open = (await ledgerProvider.openDocuments(spec.applies.book, draft.txnDate))
          .filter((d) => d.contact_id === draft.contactId && d.kind === spec.applies.kind);
        if (saved && saved.contact?.id === draft.contactId) open = await withOwnApplications(open, saved, spec.applies.kind);
      }
      return { contacts, items, open };
    },
    ({ contacts, items, open }) => {
      const itemsById = new Map(items.map((i) => [i.item_id, i]));
      const contact = contacts.find((c) => c.contact_id === draft.contactId) ?? null;
      if (!spec.lines) {
        // Keep typed amounts for invoices still open; drop the rest.
        const typed = new Map(draft.applications.map((a) => [a.documentId, a.amount]));
        draft.applications = open.map((d) => ({
          documentId: d.document_id, kind: d.kind, number: d.number, amount: typed.get(d.document_id) ?? "",
        }));
      }

      const classHints = [...new Set([...contacts, ...items].map((x) => x.default_class).filter(Boolean))].sort();

      main.innerHTML = (saved
        ? viewHead(
          `${spec.label} ${saved.number ?? ""}`.trim(),
          `Editing version ${saved.version} · saving keeps the number and ${spec.posts ? "reverses the old entry before posting the new one" : "posts nothing"}`,
          `<a class="btn" href="#ledger-new/${kind}">New ${esc(spec.noun ?? spec.label.toLowerCase())}</a>`,
        )
        : viewHead(
          `New ${spec.noun ?? spec.label.toLowerCase()}`,
          spec.posts ? "Saves and posts to the own book" : "Saves to the own book; posts nothing",
          `<div class="seg" role="group" aria-label="Document type">
            ${Object.entries(ENTRY_KINDS).map(([k, s]) => `<button data-kind="${k}" aria-pressed="${k === kind}">${esc(s.label)}</button>`).join("")}
          </div>`,
        )) + `
      <div class="pad">
        ${notice ? noticeHtml(notice) : ""}
        <form id="entryForm" autocomplete="off">
          <div class="panel" style="margin-bottom:14px"><div class="panel-body">
            <div class="form-grid">
              <label class="form-field"><span class="lbl">${spec.contact === "customer" ? "Customer" : "Vendor"}</span>
                <select class="form-select" name="contactId" required>
                  <option value="">Choose…</option>
                  ${contacts.map((c) => `<option value="${esc(c.contact_id)}" ${c.contact_id === draft.contactId ? "selected" : ""}>${esc(c.display_name)}</option>`).join("")}
                </select>
              </label>
              <label class="form-field"><span class="lbl">Date</span>
                <input class="form-input" type="date" name="txnDate" value="${esc(draft.txnDate)}" required>
              </label>
              ${spec.lines ? `<label class="form-field"><span class="lbl">${spec.numbered ? "Number" : "Vendor's number"}</span>
                <input class="form-input mono" name="number" value="${esc(draft.number)}" placeholder="${spec.numbered ? "next in sequence" : "from the bill"}" ${saved && spec.numbered ? "readonly title=\"A saved document keeps its number\"" : ""}>
              </label>` : ""}
              ${spec.bank ? `<label class="form-field"><span class="lbl">${esc(spec.bank.label)}</span>
                <select class="form-select" name="bank">
                  ${spec.bank.accounts.map((a) => `<option value="${a.id}" ${a.id === draft.bank ? "selected" : ""}>${esc(a.label)}</option>`).join("")}
                </select>
              </label>` : ""}
              <label class="form-field wide"><span class="lbl">Memo</span>
                <input class="form-input" name="memo" value="${esc(draft.memo)}">
              </label>
            </div>
            ${contact ? `<p class="form-note">${contactNote(contact, spec)}</p>` : ""}
          </div></div>

          ${spec.lines ? linesHtml(kind, spec, draft, items, classHints) : applicationsHtml(spec, draft, open, contact)}

          <div class="strip" id="entryTotals">${totalsHtml(kind, spec, draft, contact)}</div>

          <div class="form-actions" style="margin-top:12px">
            <button class="btn btn-primary" type="submit">${saved ? "Save changes" : spec.posts ? "Save and post" : "Save"}</button>
            <span class="hint" style="margin:0">⌘/Ctrl-Enter saves. ${spec.taxed ? "Tax is the book's: NJ 6.625% on taxable lines, none for an exempt customer." : ""}</span>
          </div>
        </form>
        ${saved ? voidFormHtml(saved) : ""}
        <datalist id="classHints">${classHints.map((c) => `<option value="${esc(c)}">`).join("")}</datalist>
      </div>`;

      wire(main, kind, spec, draft, contact, itemsById, saved);
      const first = draft.contactId
        ? main.querySelector(spec.lines ? "[data-line='0'][data-field='itemId']" : "[data-app='0']")
        : main.querySelector("[name='contactId']");
      first?.focus();
    },
  );
}

function noticeHtml(notice) {
  if (!notice.ok) return `<div class="form-error">${esc(notice.text)}</div>`;
  const r = notice.result;
  const verb = { updated: "Saved changes to", voided: "Voided" }[notice.action] ?? "Saved";
  const parts = [
    `${verb} ${esc(notice.label)}${r.number ? ` ${esc(r.number)}` : ""}${r.version > 1 ? ` (version ${r.version})` : ""}`,
    r.reversal_entry_ids?.length ? `reversed ${r.reversal_entry_ids.map(esc).join(", ")}` : null,
    `total ${plain(r.total)}`,
    r.tax != null ? `tax ${plain(r.tax)}` : null,
    r.due_date ? `due ${esc(r.due_date)}` : null,
    r.entry_id ? `posted as ${esc(r.entry_id)}`
      : notice.action === "voided" ? null
        : notice.action === "updated" ? (r.reversal_entry_ids?.length ? "posts nothing now" : "entry unchanged")
          : "not posted",
    `locked through ${esc(r.locked_through ?? "nothing")}`,
  ].filter(Boolean);
  return `<div class="form-ok">${parts.join(" · ")}</div>`;
}

const PRINTABLE = new Set(["Invoice", "Estimate", "SalesReceipt", "CreditMemo", "PurchaseOrder"]);

/** D39: write this document's PDF and say where it went. Sends nothing. */
function pdfButtonHtml(saved) {
  return PRINTABLE.has(saved.kind)
    ? `<div class="form-actions" style="margin-top:18px"><button class="btn" type="button" id="renderPdf">Save PDF</button><span class="hint" id="renderPdfNote" style="margin:0">Writes the ${esc(saved.kind === "PurchaseOrder" ? "purchase order" : saved.kind.toLowerCase())} as a PDF; emailing it is a separate step.</span></div>`
    : "";
}

function wirePdf(main, saved) {
  const btn = main.querySelector("#renderPdf");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    const note = main.querySelector("#renderPdfNote");
    btn.disabled = true;
    try {
      const r = await ledgerProvider.renderDocument(saved.document_id);
      const link = r.pay_link?.status === "attached" ? " · with a pay page beside it" : "";
      note.textContent = `Saved ${r.path} (${r.page_count} page${r.page_count === 1 ? "" : "s"}, balance due ${plain(r.balance_due)})${link}.`;
    } catch (err) {
      note.textContent = err.message;
      note.classList.add("neg");
    } finally {
      btn.disabled = false;
    }
  });
}

function voidFormHtml(saved) {
  return `${pdfButtonHtml(saved)}<form id="voidForm" class="panel" style="margin-top:18px" autocomplete="off"><div class="panel-body">
    <div class="form-grid">
      <label class="form-field wide"><span class="lbl">Void ${esc(saved.number ?? saved.document_id)}</span>
        <input class="form-input" name="reason" placeholder="Why — kept on the document and its reversal" required>
      </label>
    </div>
    <div class="form-actions">
      <button class="btn" type="submit">Void</button>
      <span class="hint" style="margin:0">Nothing is deleted: the book saves a voided version and reverses its entry. Refused while a payment or credit is applied to it, or in a closed period.</span>
    </div>
  </div></form>`;
}

/** A document this form does not edit — voided, or a kind or line it
 * cannot show — as a plain summary, with Void when it is not voided yet. */
function renderReadOnly(main, doc, why) {
  const notice = noticeFor(main, doc.kind);
  main.innerHTML = viewHead(
    `${doc.kind} ${doc.number ?? ""}`.trim(),
    doc.is_voided ? "Voided · kept, never deleted" : why ?? "",
    `<a class="btn" href="#ledger-open/receivable">Receivables</a><a class="btn" href="#ledger-open/payable">Payables</a>`,
  ) + `
  <div class="pad">
    ${notice ? noticeHtml(notice) : ""}
    ${doc.is_voided ? `<div class="banner banner-warn">${esc(doc.memo ?? "Voided")}</div>` : ""}
    <div class="strip">
      <div class="strip-cell"><div class="lbl">Date</div><div class="strip-val">${esc(doc.txn_date)}</div></div>
      <div class="strip-cell"><div class="lbl">Contact</div><div class="strip-val">${esc(doc.contact?.id ?? "—")}</div></div>
      <div class="strip-cell"><div class="lbl">Total</div><div class="strip-val">${plain(doc.total)}</div></div>
      <div class="strip-cell"><div class="lbl">Version</div><div class="strip-val">${esc(doc.version)}</div></div>
    </div>
    <div class="grid-wrap"><div class="grid-scroll"><table>
      <thead><tr><th>#</th><th>Kind</th><th>Item / account</th><th>Description</th><th class="r">Amount</th></tr></thead>
      <tbody>${(doc.lines ?? []).map((l) => `<tr>
        <td class="mono dim">${esc(l.line_no)}</td><td class="dim">${esc(l.kind)}</td>
        <td class="mono">${esc(l.item_id ?? l.account ?? "")}</td><td>${esc(l.description ?? "")}</td>
        <td class="r num">${plain(l.amount)}</td>
      </tr>`).join("") || `<tr><td colspan="5" class="dim">No lines.</td></tr>`}</tbody>
    </table></div></div>
    ${doc.is_voided ? pdfButtonHtml(doc) : voidFormHtml(doc)}
  </div>`;
  wireVoid(main, doc);
  wirePdf(main, doc);
}

function wireVoid(main, saved) {
  const form = main.querySelector("#voidForm");
  if (!form) return;
  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    const reason = form.elements.reason.value.trim();
    if (!reason) return;
    const label = ENTRY_KINDS[saved.kind]?.noun ?? ENTRY_KINDS[saved.kind]?.label.toLowerCase() ?? saved.kind;
    try {
      const result = await ledgerProvider.voidDocument(saved.document_id, reason, ACTOR);
      editing.delete(saved.document_id);
      main.dataset.entryNotice = JSON.stringify({ kind: saved.kind, ok: true, action: "voided", label, result });
    } catch (err) {
      main.dataset.entryNotice = JSON.stringify({ kind: saved.kind, ok: false, text: err.message });
    }
    renderEditDocument(main, saved.document_id);
  });
}

function contactNote(contact, spec) {
  const bits = [];
  const terms = termsOf(contact);
  bits.push(esc(contactTermsLabel(contact))
    + (terms.type === "day_of_month" && terms.next_month_within > 0 ? ` (next month if dated within ${terms.next_month_within} days)` : ""));
  if (contact.default_class) bits.push(`class ${esc(contact.default_class)} unless a line says otherwise`);
  if (spec.taxed && contact.is_tax_exempt) bits.push("tax exempt");
  if (contact.email) bits.push(esc(contact.email));
  return bits.join(" · ");
}

function linesHtml(kind, spec, draft, items, classHints) {
  const row = (l, i) => {
    const amount = safeAmount(l);
    return `<tr>
      <td class="mono dim">${i + 1}</td>
      <td><select class="form-select" data-line="${i}" data-field="itemId">
        <option value="">${spec.accountLines ? "Account line" : "Choose…"}</option>
        ${items.map((it) => `<option value="${esc(it.item_id)}" ${it.item_id === l.itemId ? "selected" : ""}>${esc(it.sku ? `${it.sku} — ${it.name}` : it.name)}</option>`).join("")}
      </select>
      ${spec.accountLines ? `<input class="form-input mono" data-line="${i}" data-field="account" value="${esc(l.account)}" placeholder="account no." ${l.itemId ? "hidden" : ""} style="margin-top:3px">` : ""}</td>
      <td><input class="form-input" data-line="${i}" data-field="description" value="${esc(l.description)}"></td>
      <td><input class="form-input r num" data-line="${i}" data-field="qty" value="${esc(l.qty)}" inputmode="decimal" size="5"></td>
      <td><input class="form-input r num" data-line="${i}" data-field="rate" value="${esc(l.rate)}" inputmode="decimal" size="8"></td>
      <td><input class="form-input r num" data-line="${i}" data-field="amount" value="${esc(l.amount)}" inputmode="decimal" size="9" placeholder="${amount ?? ""}"></td>
      <td><input class="form-input" data-line="${i}" data-field="cls" value="${esc(l.cls)}" list="classHints" size="8" placeholder="${esc(defaultClassFor(l, items) ?? "")}"></td>
      ${spec.taxed ? `<td style="text-align:center"><input type="checkbox" data-line="${i}" data-field="taxable" ${l.taxable ? "checked" : ""} aria-label="Taxable"></td>` : ""}
      <td><button class="btn" type="button" data-remove="${i}" aria-label="Remove line ${i + 1}" ${draft.lines.length === 1 ? "disabled" : ""}>×</button></td>
    </tr>`;
  };
  return `<div class="grid-wrap"><div class="grid-scroll"><table>
    <thead><tr><th>#</th><th>Item</th><th>Description</th><th class="r">Qty</th><th class="r">Rate</th><th class="r">Amount</th><th>Class</th>${spec.taxed ? "<th>Tax</th>" : ""}<th></th></tr></thead>
    <tbody>${draft.lines.map(row).join("")}</tbody>
  </table></div></div>
  <div class="form-actions" style="margin:8px 0 12px"><button class="btn" type="button" id="addLine">Add line</button>
    <span class="hint" style="margin:0">Leave amount blank to use quantity × rate. A blank class takes the item's, then the ${spec.contact}'s.</span></div>`;
}

function defaultClassFor(line, items) {
  const item = items.find((i) => i.item_id === line.itemId);
  return item?.default_class ?? null;
}

function applicationsHtml(spec, draft, open, contact) {
  const customer = spec.contact === "customer";
  if (!contact) return `<div class="state-note">Choose a ${spec.contact} to see ${customer ? "what they owe" : "what is owed to them"}.</div>`;
  if (open.length === 0) return `<div class="state-note">${esc(contact.display_name)} has nothing open as of ${esc(draft.txnDate)}.</div>`;
  return `<div class="grid-wrap"><div class="grid-scroll"><table>
    <thead><tr><th>${esc(spec.applies.kind)}</th><th>Date</th><th>Due</th><th class="r">Total</th><th class="r">Open</th><th class="r">Apply</th></tr></thead>
    <tbody>${open.map((d, i) => `<tr>
      <td class="mono">${esc(d.number ?? d.document_id)}</td>
      <td class="dim">${esc(d.txn_date)}</td>
      <td class="dim">${esc(d.due_date)}</td>
      <td class="r num dim">${plain(d.total)}</td>
      <td class="r num">${plain(d.open)}</td>
      <td class="r"><input class="form-input r num" data-app="${i}" value="${esc(draft.applications[i]?.amount ?? "")}" placeholder="0.00" inputmode="decimal" size="9">
        <button class="btn" type="button" data-pay-full="${i}" data-open="${esc(d.open)}">Full</button></td>
    </tr>`).join("")}</tbody>
  </table></div></div>`;
}

function totalsHtml(kind, spec, draft, contact) {
  const t = previewTotals(kind, draft.lines, contact, draft.applications);
  const cell = (label, value) => `<div class="strip-cell"><div class="lbl">${label}</div><div class="strip-val">${plain(value)}</div></div>`;
  if (!spec.lines) return cell("Payment", t.total);
  return cell("Subtotal", t.subtotal)
    + (spec.taxed ? cell(t.exempt ? "Tax (exempt)" : "Tax, estimated", t.tax) : "")
    + cell("Total", t.total);
}

function wire(main, kind, spec, draft, contact, itemsById, saved) {
  const form = main.querySelector("#entryForm");
  const totals = main.querySelector("#entryTotals");
  const repaintTotals = () => {
    totals.innerHTML = totalsHtml(kind, spec, draft, contact);
  };
  const again = () => (saved ? renderEditDocument(main, saved.document_id) : renderNewDocument(main, kind));
  const rerender = () => {
    delete main.dataset.entryNotice;
    again();
  };
  wireVoid(main, saved);
  if (saved) wirePdf(main, saved);

  main.querySelectorAll("[data-kind]").forEach((btn) => {
    btn.addEventListener("click", () => {
      delete main.dataset.entryNotice;
      window.location.hash = `#ledger-new/${btn.dataset.kind}`;
    });
  });

  for (const name of ["txnDate", "number", "memo"]) {
    form.elements[name]?.addEventListener("input", (event) => {
      draft[name] = event.target.value;
    });
  }
  form.elements.contactId.addEventListener("change", (event) => {
    draft.contactId = event.target.value;
    rerender();
  });
  if (!spec.lines) form.elements.txnDate.addEventListener("change", rerender);
  form.elements.bank?.addEventListener("change", (event) => {
    draft.bank = event.target.value;
  });

  main.querySelectorAll("[data-line]").forEach((el) => {
    const i = Number(el.dataset.line);
    const field = el.dataset.field;
    el.addEventListener(el.tagName === "SELECT" || el.type === "checkbox" ? "change" : "input", () => {
      const line = draft.lines[i];
      if (field === "itemId") {
        const item = itemsById.get(el.value);
        draft.lines[i] = item ? { ...lineForItem(kind, item), cls: line.cls } : { ...blankLine(), account: line.account };
        rerender();
        return;
      }
      line[field] = el.type === "checkbox" ? el.checked : el.value;
      if (field === "qty" || field === "rate" || field === "amount") {
        const amountInput = main.querySelector(`[data-line='${i}'][data-field='amount']`);
        amountInput.placeholder = safeAmount({ ...line, amount: "" }) ?? "";
      }
      repaintTotals();
    });
    if (el.dataset.field === (spec.taxed ? "taxable" : "cls")) {
      el.addEventListener("keydown", (event) => {
        if (event.key === "Enter" && !event.metaKey && !event.ctrlKey && i === draft.lines.length - 1) {
          event.preventDefault();
          draft.lines.push(blankLine());
          rerender();
        }
      });
    }
  });

  main.querySelector("#addLine")?.addEventListener("click", () => {
    draft.lines.push(blankLine());
    rerender();
  });
  main.querySelectorAll("[data-remove]").forEach((btn) => {
    btn.addEventListener("click", () => {
      draft.lines.splice(Number(btn.dataset.remove), 1);
      rerender();
    });
  });

  main.querySelectorAll("[data-app]").forEach((el) => {
    el.addEventListener("input", () => {
      draft.applications[Number(el.dataset.app)].amount = el.value;
      repaintTotals();
    });
  });
  main.querySelectorAll("[data-pay-full]").forEach((btn) => {
    btn.addEventListener("click", () => {
      const i = Number(btn.dataset.payFull);
      draft.applications[i].amount = btn.dataset.open;
      main.querySelector(`[data-app='${i}']`).value = btn.dataset.open;
      repaintTotals();
    });
  });

  form.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && (event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      form.requestSubmit();
    }
  });

  form.addEventListener("submit", async (event) => {
    event.preventDefault();
    let document;
    try {
      document = saved ? buildEditedDocument(kind, draft) : buildDocument(kind, draft);
    } catch (err) {
      main.dataset.entryNotice = JSON.stringify({ kind, ok: false, text: err.message });
      again();
      return;
    }
    const label = spec.noun ?? spec.label.toLowerCase();
    try {
      if (saved) {
        const result = await ledgerProvider.updateDocument(document, ACTOR);
        editing.delete(saved.document_id);
        main.dataset.entryNotice = JSON.stringify({ kind, ok: true, action: "updated", label, result });
      } else {
        const result = await ledgerProvider.createDocument(document, ACTOR);
        drafts.delete(kind);
        main.dataset.entryNotice = JSON.stringify({ kind, ok: true, label, result });
      }
    } catch (err) {
      main.dataset.entryNotice = JSON.stringify({ kind, ok: false, text: err.message });
    }
    again();
  });
}
