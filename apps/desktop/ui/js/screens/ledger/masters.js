// Customers, vendors and items in the own book (D28, `docs/MCP.md`
// "list_contacts" / "list_items" / "upsert_contact" / "upsert_item"): the
// master data `create_document` checks every document against. A list with
// search, and one editor above it for a new record or the one being edited.
// Nothing is deleted: a record no longer in use is made inactive, so every
// document that names it still resolves.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";
import { ITEM_TYPES, buildContact, buildItem, contactTermsLabel } from "./masterData.js";
import { termsOf } from "../../data/ledger/terms.js";

const CONTACT_KINDS = {
  customer: { label: "Customers", one: "customer" },
  vendor: { label: "Vendors", one: "vendor" },
};

function noticeHtml(notice) {
  if (!notice) return "";
  return `<div class="${notice.ok ? "form-ok" : "form-error"}">${esc(notice.text)}</div>`;
}

function field(label, name, value, attrs = "") {
  return `<label class="form-field"><span class="lbl">${esc(label)}</span>
    <input class="form-input" name="${name}" value="${esc(value ?? "")}" ${attrs}></label>`;
}

function check(label, name, checked) {
  return `<label class="form-field" style="flex-direction:row;align-items:center;gap:6px">
    <input type="checkbox" name="${name}" ${checked ? "checked" : ""}><span>${esc(label)}</span></label>`;
}

function formValues(form) {
  const out = {};
  for (const el of form.elements) {
    if (!el.name) continue;
    out[el.name] = el.type === "checkbox" ? el.checked : el.value;
  }
  return out;
}

/** Shared by both screens: search box, inactive toggle, notice, editing id. */
function screenState(main, key) {
  return {
    query: main.dataset[`${key}Query`] ?? "",
    inactive: main.dataset[`${key}Inactive`] === "1",
    editing: main.dataset[`${key}Editing`] ?? null,
    notice: main.dataset[`${key}Notice`] ? JSON.parse(main.dataset[`${key}Notice`]) : null,
  };
}

function wireListControls(main, key, rerender) {
  const search = main.querySelector("#masterSearch");
  search?.addEventListener("keydown", (event) => {
    if (event.key === "Enter") {
      event.preventDefault();
      main.dataset[`${key}Query`] = search.value;
      delete main.dataset[`${key}Notice`];
      rerender();
    }
  });
  main.querySelector("#masterInactive")?.addEventListener("change", (event) => {
    main.dataset[`${key}Inactive`] = event.target.checked ? "1" : "0";
    rerender();
  });
  main.querySelector("#masterNew")?.addEventListener("click", () => {
    main.dataset[`${key}Editing`] = "";
    delete main.dataset[`${key}Notice`];
    rerender();
  });
  main.querySelectorAll("[data-edit]").forEach((btn) => {
    btn.addEventListener("click", () => {
      main.dataset[`${key}Editing`] = btn.dataset.edit;
      delete main.dataset[`${key}Notice`];
      rerender();
    });
  });
  main.querySelector("#masterCancel")?.addEventListener("click", () => {
    delete main.dataset[`${key}Editing`];
    rerender();
  });
}

function listHead(state, label) {
  return `<div class="form-actions" style="margin-bottom:10px">
    <input class="form-input" id="masterSearch" placeholder="Search ${esc(label)} — Enter" value="${esc(state.query)}" style="max-width:320px">
    <label class="hint" style="margin:0;display:flex;gap:6px;align-items:center"><input type="checkbox" id="masterInactive" ${state.inactive ? "checked" : ""}> Show inactive</label>
    <span class="spacer" style="flex:1"></span>
    <button class="btn btn-primary" type="button" id="masterNew">New</button>
  </div>`;
}

// ---------------------------------------------------------------------------
// customers and vendors
// ---------------------------------------------------------------------------

export async function renderContacts(main, kind) {
  if (!CONTACT_KINDS[kind]) kind = "customer";
  const spec = CONTACT_KINDS[kind];
  const key = `contacts${kind}`;
  const state = screenState(main, key);
  const rerender = () => renderContacts(main, kind);

  await withState(
    main,
    spec.label.toLowerCase(),
    () => Promise.all([
      ledgerProvider.listContacts(kind, state.query || undefined, state.inactive || undefined),
      ledgerProvider.listContacts(undefined, undefined, true),
      ledgerProvider.aging(kind === "customer" ? "receivable" : "payable", todayIso()),
    ]),
    ([list, everyone, aging]) => {
      const owed = new Map(aging.rows.map((r) => [r.contact_id, r.total]));
      const editing = state.editing == null ? null : everyone.find((c) => c.contact_id === state.editing) ?? null;
      const showEditor = state.editing != null;

      main.innerHTML = viewHead(
        spec.label,
        `${list.length} ${spec.one}${list.length === 1 ? "" : "s"}${state.query ? ` matching "${state.query}"` : ""}`,
        `<div class="seg" role="group" aria-label="Contact kind">
          ${Object.entries(CONTACT_KINDS).map(([k, s]) => `<button data-kind="${k}" aria-pressed="${k === kind}">${esc(s.label)}</button>`).join("")}
        </div>`,
      ) + `
      <div class="pad">
        ${noticeHtml(state.notice)}
        ${showEditor ? contactEditor(kind, spec, editing) : ""}
        ${listHead(state, spec.label.toLowerCase())}
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Name</th><th>Email</th><th>Terms</th><th>Class</th>${kind === "customer" ? "<th>Tax</th>" : ""}<th class="r">${kind === "customer" ? "Owes" : "Owed"}</th><th></th></tr></thead>
          <tbody>${list.length === 0 ? `<tr><td colspan="7" class="dim">None.</td></tr>` : list.map((c) => `<tr>
            <td>${esc(c.display_name)}${c.company_name ? ` <span class="dim">${esc(c.company_name)}</span>` : ""}${c.is_active ? "" : ' <span class="tag">inactive</span>'}</td>
            <td class="dim">${esc(c.email ?? "")}</td>
            <td class="dim">${esc(contactTermsLabel(c))}</td>
            <td class="dim">${esc(c.default_class ?? "")}</td>
            ${kind === "customer" ? `<td>${c.is_tax_exempt ? '<span class="tag">exempt</span>' : ""}</td>` : ""}
            <td class="r num">${plain(owed.get(c.contact_id) ?? null, { dashForZero: true })}</td>
            <td class="r"><button class="btn" type="button" data-edit="${esc(c.contact_id)}">Edit</button>
              <a class="btn" href="#ledger-new/${kind === "customer" ? "Invoice" : "Bill"}" data-start="${esc(c.contact_id)}">${kind === "customer" ? "Invoice" : "Bill"}</a>
              ${kind === "customer" ? `<button class="btn" type="button" data-statement="${esc(c.contact_id)}" title="Save a statement PDF as of today">Statement</button>` : ""}</td>
          </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;

      main.querySelectorAll("[data-kind]").forEach((btn) => {
        btn.addEventListener("click", () => {
          window.location.hash = `#ledger-contacts/${btn.dataset.kind}`;
        });
      });
      wireListControls(main, key, rerender);
      main.querySelectorAll("[data-statement]").forEach((btn) => {
        btn.addEventListener("click", async () => {
          try {
            const r = await ledgerProvider.renderStatement(btn.dataset.statement, todayIso());
            main.dataset[`${key}Notice`] = JSON.stringify({ ok: true, text: `Saved statement ${r.path} (${r.open_items} open item${r.open_items === 1 ? "" : "s"}). Emailing it is a separate step.` });
          } catch (err) {
            main.dataset[`${key}Notice`] = JSON.stringify({ ok: false, text: err.message });
          }
          rerender();
        });
      });
      main.querySelectorAll("[data-start]").forEach((a) => {
        a.addEventListener("click", () => {
          sessionHint.contactId = a.dataset.start;
        });
      });

      const form = main.querySelector("#contactForm");
      form?.querySelector("[name='display_name']")?.focus();
      form?.querySelector("#termsType")?.addEventListener("change", (event) => {
        const byDay = event.target.value === "day_of_month";
        for (const [id, hide] of [["#termsNet", byDay], ["#termsByDay", !byDay]]) {
          form.querySelector(id).style.display = hide ? "none" : "contents";
        }
      });
      form?.addEventListener("submit", async (event) => {
        event.preventDefault();
        try {
          const contact = buildContact(kind, formValues(form), editing, new Set(everyone.map((c) => c.contact_id)));
          const result = await ledgerProvider.upsertContact(contact);
          delete main.dataset[`${key}Editing`];
          main.dataset[`${key}Notice`] = JSON.stringify({ ok: true, text: `Saved ${result.contact.display_name}.` });
        } catch (err) {
          main.dataset[`${key}Notice`] = JSON.stringify({ ok: false, text: err.message });
        }
        rerender();
      });
    },
  );
}

/** Carries "start a document for this contact" from the contacts list to
 * the entry screen, which takes it once. */
export const sessionHint = { contactId: null };

/** Net N, or due on day D with QBO's roll-over window (D41). */
function termsFields(c) {
  const terms = termsOf(c);
  const byDay = terms.type === "day_of_month";
  return `<label class="form-field"><span class="lbl">Terms</span>
      <select class="form-select" name="terms_type" id="termsType">
        <option value="net" ${byDay ? "" : "selected"}>Net days</option>
        <option value="day_of_month" ${byDay ? "selected" : ""}>Day of the month</option>
      </select></label>
    <span id="termsNet" style="display:${byDay ? "none" : "contents"}">
      ${field("Net days", "terms_days", c?.terms_days, 'inputmode="numeric" placeholder="due on receipt"')}
    </span>
    <span id="termsByDay" style="display:${byDay ? "contents" : "none"}">
      ${field("Due on day", "due_day", byDay ? terms.day : "", 'inputmode="numeric" placeholder="1 to 31 (31 is the last)"')}
      ${field("Next month if within (days)", "next_month_within", byDay ? terms.next_month_within : "", 'inputmode="numeric" placeholder="0"')}
    </span>`;
}

function contactEditor(kind, spec, c) {
  return `<div class="panel" style="margin-bottom:14px">
    <div class="panel-head"><span class="lbl">${c ? `Edit ${esc(c.display_name)}` : `New ${spec.one}`}</span>
      ${c ? `<span class="mono dim">${esc(c.contact_id)}</span>` : ""}</div>
    <div class="panel-body"><form id="contactForm" autocomplete="off">
      <div class="form-grid">
        ${field("Name", "display_name", c?.display_name, "required")}
        ${field("Company", "company_name", c?.company_name)}
        ${field("Email", "email", c?.email, 'type="email"')}
        ${field("Phone", "phone", c?.phone)}
        ${termsFields(c)}
        ${field("Default class", "default_class", c?.default_class)}
        <label class="form-field wide"><span class="lbl">Bill to</span><textarea class="form-textarea" name="bill_address" rows="2">${esc(c?.bill_address ?? "")}</textarea></label>
        <label class="form-field wide"><span class="lbl">Ship to</span><textarea class="form-textarea" name="ship_address" rows="2">${esc(c?.ship_address ?? "")}</textarea></label>
        ${kind === "customer" ? check("Tax exempt", "is_tax_exempt", c?.is_tax_exempt) : ""}
        ${check("Active", "is_active", c ? c.is_active : true)}
      </div>
      <div class="form-actions">
        <button class="btn btn-primary" type="submit">Save</button>
        <button class="btn" type="button" id="masterCancel">Cancel</button>
        <span class="hint" style="margin:0">Made inactive rather than deleted, so every document that names it still resolves.</span>
      </div>
    </form></div>
  </div>`;
}

// ---------------------------------------------------------------------------
// items
// ---------------------------------------------------------------------------

export async function renderItems(main) {
  const key = "items";
  const state = screenState(main, key);
  const rerender = () => renderItems(main);

  await withState(
    main,
    "items",
    () => Promise.all([
      ledgerProvider.listItems(state.query || undefined, state.inactive || undefined),
      ledgerProvider.listItems(undefined, true),
      ledgerProvider.trialBalance(todayIso()),
    ]),
    ([list, every, tb]) => {
      const editing = state.editing == null ? null : every.find((i) => i.item_id === state.editing) ?? null;
      const accounts = tb.rows.map((r) => ({ id: r.account_id, label: `${r.number} ${r.name}`, classification: r.classification }));

      main.innerHTML = viewHead(
        "Products and services",
        `${list.length} item${list.length === 1 ? "" : "s"}${state.query ? ` matching "${state.query}"` : ""}`,
      ) + `
      <div class="pad">
        ${noticeHtml(state.notice)}
        ${state.editing != null ? itemEditor(editing, accounts) : ""}
        ${listHead(state, "items")}
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>SKU</th><th>Name</th><th>Type</th><th>Income</th><th>Class</th><th class="r">Price</th><th class="r">Cost</th><th>Tax</th><th></th></tr></thead>
          <tbody>${list.length === 0 ? `<tr><td colspan="9" class="dim">None.</td></tr>` : list.map((i) => `<tr>
            <td class="mono dim">${esc(i.sku ?? "")}</td>
            <td>${esc(i.name)}${i.is_active ? "" : ' <span class="tag">inactive</span>'}</td>
            <td class="dim">${esc(ITEM_TYPES.find((t) => t.id === i.item_type)?.label ?? i.item_type)}</td>
            <td class="mono dim">${esc(i.income_account)}</td>
            <td class="dim">${esc(i.default_class ?? "")}</td>
            <td class="r num">${plain(i.sales_price)}</td>
            <td class="r num dim">${plain(i.purchase_cost ?? i.unit_cost)}</td>
            <td>${i.is_taxable ? '<span class="tag">taxable</span>' : ""}</td>
            <td class="r"><button class="btn" type="button" data-edit="${esc(i.item_id)}">Edit</button></td>
          </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;

      wireListControls(main, key, rerender);

      const form = main.querySelector("#itemForm");
      form?.querySelector("[name='name']")?.focus();
      const assetField = main.querySelector("#assetField");
      form?.elements.item_type?.addEventListener("change", (event) => {
        assetField.hidden = event.target.value !== "inventory";
      });
      form?.addEventListener("submit", async (event) => {
        event.preventDefault();
        try {
          const item = buildItem(formValues(form), editing, new Set(every.map((i) => i.item_id)));
          const result = await ledgerProvider.upsertItem(item);
          delete main.dataset[`${key}Editing`];
          main.dataset[`${key}Notice`] = JSON.stringify({ ok: true, text: `Saved ${result.item.name}.` });
        } catch (err) {
          main.dataset[`${key}Notice`] = JSON.stringify({ ok: false, text: err.message });
        }
        rerender();
      });
    },
  );
}

function accountSelect(label, name, value, accounts, kinds, extra = "") {
  const options = accounts.filter((a) => kinds.includes(a.classification));
  return `<label class="form-field" ${extra}><span class="lbl">${esc(label)}</span>
    <select class="form-select" name="${name}">
      <option value="">Choose…</option>
      ${options.map((a) => `<option value="${esc(a.id)}" ${a.id === value ? "selected" : ""}>${esc(a.label)}</option>`).join("")}
    </select></label>`;
}

function itemEditor(i, accounts) {
  const type = i?.item_type ?? "non_inventory";
  return `<div class="panel" style="margin-bottom:14px">
    <div class="panel-head"><span class="lbl">${i ? `Edit ${esc(i.name)}` : "New item"}</span>
      ${i ? `<span class="mono dim">${esc(i.item_id)}</span>` : ""}</div>
    <div class="panel-body"><form id="itemForm" autocomplete="off">
      <div class="form-grid">
        ${field("Name", "name", i?.name, "required")}
        ${field("SKU", "sku", i?.sku)}
        <label class="form-field"><span class="lbl">Type</span>
          <select class="form-select" name="item_type">
            ${ITEM_TYPES.map((t) => `<option value="${t.id}" ${t.id === type ? "selected" : ""}>${esc(t.label)}</option>`).join("")}
          </select></label>
        ${accountSelect("Income account", "income_account", i?.income_account ?? "4100", accounts, ["Income"])}
        ${accountSelect("Expense account", "expense_account", i?.expense_account ?? "5000", accounts, ["COGS", "Expense"])}
        ${accountSelect("Asset account", "asset_account", i?.asset_account, accounts, ["Asset"], `id="assetField" ${type === "inventory" ? "" : "hidden"}`)}
        ${field("Default class", "default_class", i?.default_class)}
        ${field("Sales price", "sales_price", i?.sales_price, 'inputmode="decimal"')}
        ${field("Purchase cost", "purchase_cost", i?.purchase_cost, 'inputmode="decimal"')}
        <label class="form-field wide"><span class="lbl">Description</span><input class="form-input" name="description" value="${esc(i?.description ?? "")}"></label>
        ${check("Taxable", "is_taxable", i ? i.is_taxable : true)}
        ${check("Active", "is_active", i ? i.is_active : true)}
      </div>
      <div class="form-actions">
        <button class="btn btn-primary" type="submit">Save</button>
        <button class="btn" type="button" id="masterCancel">Cancel</button>
        ${i?.unit_cost ? `<span class="hint" style="margin:0">Moving-average cost ${plain(i.unit_cost)} is the book's (W12).</span>` : ""}
      </div>
    </form></div>
  </div>`;
}
