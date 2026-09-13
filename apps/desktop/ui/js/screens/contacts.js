// Customer and vendor pages. `docs/MCP.md`'s `contact_detail`: contact info
// and balance, every document still carrying a balance, and the 50 most
// recent regardless of balance. There is no list-all-customers tool — a
// contact is reached by search (`palette.js`) or by following a document's
// party link — so this module only ever renders one contact at a time.

import { provider } from "../data/index.js";
import { esc, plain, date } from "../format.js";
import { viewHead, withState, documentStatusTag, balanceCell } from "./shared.js";

function documentRows(rows, emptyText) {
  if (rows.length === 0) return `<tr><td colspan="6" class="dim">${esc(emptyText)}</td></tr>`;
  return rows.map((d) => `
    <tr data-clickable data-doc="${esc(d.qbo_id)}">
      <td class="dim">${esc(d.doc_type)}</td>
      <td class="mono">${esc(d.doc_number ?? d.qbo_id)}</td>
      <td class="mono dim">${date(d.txn_date)}</td>
      <td class="r num">${plain(d.total)}</td>
      <td class="r num">${balanceCell(d.balance)}</td>
      <td>${documentStatusTag(d)}</td>
    </tr>`).join("");
}

/** `contactType` is `"customer"` or `"vendor"` — the lower-case form
 * `docs/MCP.md` documents for this tool's argument, distinct from the
 * `ContactType` values (`"Customer"`/`"Vendor"`) the result itself carries. */
export async function renderContact(main, contactType, qboId) {
  await withState(
    main,
    `${contactType} ${qboId}`,
    () => provider.contactDetail(contactType, qboId),
    ({ contact, open_documents, recent_documents }) => {
      main.innerHTML = viewHead(
        contact.display_name,
        contact.company_name && contact.company_name !== contact.display_name ? contact.company_name : "",
      ) + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Open balance</div>
            <div class="strip-val ${Number(contact.balance) > 0 ? "warn" : "pos"}">${plain(contact.balance)}</div>
            <div class="strip-note">${open_documents.length} still open</div></div>
          <div class="strip-cell"><div class="lbl">Email</div>
            <div class="strip-val" style="font-size:calc(13px * var(--fs))">${contact.email ? esc(contact.email) : "—"}</div></div>
          <div class="strip-cell"><div class="lbl">Phone</div>
            <div class="strip-val" style="font-size:calc(13px * var(--fs))">${contact.phone ? esc(contact.phone) : "—"}</div></div>
          <div class="strip-cell"><div class="lbl">Status</div>
            <div class="strip-val" style="font-size:calc(13px * var(--fs))">${contact.is_active ? "Active" : "Inactive"}</div></div>
        </div>

        <div class="panel" style="margin-bottom:14px">
          <div class="panel-head"><span class="lbl">Open</span><span class="spacer" style="flex:1"></span><span class="lbl">${open_documents.length}</span></div>
          <div class="grid-scroll"><table>
            <thead><tr><th>Type</th><th>No.</th><th>Date</th><th class="r">Total</th><th class="r">Balance</th><th>Status</th></tr></thead>
            <tbody>${documentRows(open_documents, "Nothing open.")}</tbody>
          </table></div>
        </div>

        <div class="panel">
          <div class="panel-head"><span class="lbl">Recent</span><span class="spacer" style="flex:1"></span><span class="lbl">up to 50</span></div>
          <div class="grid-scroll"><table>
            <thead><tr><th>Type</th><th>No.</th><th>Date</th><th class="r">Total</th><th class="r">Balance</th><th>Status</th></tr></thead>
            <tbody>${documentRows(recent_documents, "No documents on file.")}</tbody>
          </table></div>
        </div>
      </div>`;

      main.querySelectorAll("tr[data-doc]").forEach((row) => {
        row.addEventListener("click", () => {
          window.location.hash = `#doc/${encodeURIComponent(row.dataset.doc)}`;
        });
      });
    },
  );
}
