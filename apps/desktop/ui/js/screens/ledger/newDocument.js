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

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";
import {
  ENTRY_KINDS, buildDocument, effectiveAmount, lineForItem, previewTotals,
} from "./entry.js";
import { sessionHint } from "./masters.js";

const ACTOR = "dan";

/** kind -> draft, kept for the life of the window. */
const drafts = new Map();

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
  const spec = ENTRY_KINDS[kind];
  const draft = draftFor(kind);
  if (sessionHint.contactId) {
    // "Invoice" / "Bill" from the contacts list: start this draft for them.
    draft.contactId = sessionHint.contactId;
    sessionHint.contactId = null;
  }
  const stored = main.dataset.entryNotice ? JSON.parse(main.dataset.entryNotice) : null;
  // A notice belongs to the kind that raised it, not whatever kind the URL
  // shows next.
  const notice = stored?.kind === kind ? stored : null;

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

      main.innerHTML = viewHead(
        `New ${spec.label.toLowerCase()}`,
        spec.posts ? "Saves and posts to the own book" : "Saves to the own book; posts nothing",
        `<div class="seg" role="group" aria-label="Document type">
          ${Object.entries(ENTRY_KINDS).map(([k, s]) => `<button data-kind="${k}" aria-pressed="${k === kind}">${esc(s.label)}</button>`).join("")}
        </div>`,
      ) + `
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
                <input class="form-input mono" name="number" value="${esc(draft.number)}" placeholder="${spec.numbered ? "next in sequence" : "from the bill"}">
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
            <button class="btn btn-primary" type="submit">${spec.posts ? "Save and post" : "Save"}</button>
            <span class="hint" style="margin:0">⌘/Ctrl-Enter saves. ${spec.taxed ? "Tax is the book's: NJ 6.625% on taxable lines, none for an exempt customer." : ""}</span>
          </div>
        </form>
        <datalist id="classHints">${classHints.map((c) => `<option value="${esc(c)}">`).join("")}</datalist>
      </div>`;

      wire(main, kind, spec, draft, contact, itemsById);
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
  const parts = [
    `Saved ${esc(notice.label)}${r.number ? ` ${esc(r.number)}` : ""}`,
    `total ${plain(r.total)}`,
    r.tax != null ? `tax ${plain(r.tax)}` : null,
    r.due_date ? `due ${esc(r.due_date)}` : null,
    r.entry_id ? `posted as ${esc(r.entry_id)}` : "not posted",
    `locked through ${esc(r.locked_through ?? "nothing")}`,
  ].filter(Boolean);
  return `<div class="form-ok">${parts.join(" · ")}</div>`;
}

function contactNote(contact, spec) {
  const bits = [];
  bits.push(contact.terms_days ? `Net ${contact.terms_days}` : "Due on receipt");
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

function wire(main, kind, spec, draft, contact, itemsById) {
  const form = main.querySelector("#entryForm");
  const totals = main.querySelector("#entryTotals");
  const repaintTotals = () => {
    totals.innerHTML = totalsHtml(kind, spec, draft, contact);
  };
  const rerender = () => {
    delete main.dataset.entryNotice;
    renderNewDocument(main, kind);
  };

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
      document = buildDocument(kind, draft);
    } catch (err) {
      main.dataset.entryNotice = JSON.stringify({ kind, ok: false, text: err.message });
      renderNewDocument(main, kind);
      return;
    }
    try {
      const result = await ledgerProvider.createDocument(document, ACTOR);
      drafts.delete(kind);
      main.dataset.entryNotice = JSON.stringify({ kind, ok: true, label: spec.noun ?? spec.label.toLowerCase(), result });
    } catch (err) {
      main.dataset.entryNotice = JSON.stringify({ kind, ok: false, text: err.message });
    }
    renderNewDocument(main, kind);
  });
}
