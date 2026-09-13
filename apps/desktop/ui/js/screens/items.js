// The item (SKU) page. `prototype/README.md`: "a SKU page that cannot
// answer 'where has this been used' is not worth opening." Reached by
// search (there is no list-all-items tool, same reasoning as `contacts.js`).

import { provider } from "../data/index.js";
import { esc, plain, date } from "../format.js";
import { viewHead, withState, documentStatusTag } from "./shared.js";

function whereUsedRows(rows) {
  if (rows.length === 0) return `<tr><td colspan="6" class="dim">Not on any document yet.</td></tr>`;
  return rows.map((d) => `
    <tr data-clickable data-doc="${esc(d.qbo_id)}">
      <td class="dim">${esc(d.doc_type)}</td>
      <td class="mono">${esc(d.doc_number ?? d.qbo_id)}</td>
      <td class="mono dim">${date(d.txn_date)}</td>
      <td class="trunc">${esc(d.contact_name ?? d.contact_id ?? "—")}</td>
      <td class="r num">${plain(d.total)}</td>
      <td>${documentStatusTag(d)}</td>
    </tr>`).join("");
}

export async function renderItem(main, qboId) {
  await withState(
    main,
    `item ${qboId}`,
    () => provider.itemDetail(qboId),
    ({ item, where_used, units_sold }) => {
      const price = item.unit_price != null ? Number(item.unit_price) : null;
      const cost = item.purchase_cost != null ? Number(item.purchase_cost) : null;
      const margin = price && cost != null && price !== 0 ? (price - cost) / price : null;

      main.innerHTML = viewHead(item.sku ?? item.qbo_id, item.name) + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Price</div><div class="strip-val">${item.unit_price != null ? plain(item.unit_price) : "—"}</div><div class="strip-note">${esc(item.item_type ?? "")}</div></div>
          <div class="strip-cell"><div class="lbl">Cost</div><div class="strip-val">${item.purchase_cost != null ? plain(item.purchase_cost) : "—"}</div><div class="strip-note">last landed</div></div>
          <div class="strip-cell"><div class="lbl">Gross margin</div><div class="strip-val ${margin == null ? "" : margin > 0.5 ? "pos" : margin > 0.3 ? "" : "warn"}">${margin == null ? "—" : Math.round(margin * 100) + "%"}</div></div>
          <div class="strip-cell"><div class="lbl">On hand</div><div class="strip-val">${esc(item.qty_on_hand ?? "—")}</div><div class="strip-note">${item.qty_on_hand == null ? "not stocked" : "units"}</div></div>
          <div class="strip-cell"><div class="lbl">Units sold</div><div class="strip-val">${esc(units_sold)}</div><div class="strip-note">invoices + sales receipts</div></div>
        </div>

        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Type</th><th>No.</th><th>Date</th><th>Party</th><th class="r">Total</th><th>Status</th></tr></thead>
          <tbody>${whereUsedRows(where_used)}</tbody>
        </table></div></div>
      </div>`;

      main.querySelectorAll("tr[data-doc]").forEach((row) => {
        row.addEventListener("click", () => {
          window.location.hash = `#doc/${encodeURIComponent(row.dataset.doc)}`;
        });
      });
    },
  );
}
