// "Today" — the landing screen. `prototype/README.md`'s brief for it:
// everything a QBO trip to the browser used to answer, in one place, off
// local disk. Built from the same eleven tools every other screen uses —
// aging totals, the open-document queues, and the sync chrome's own numbers
// — nothing here is a twelfth tool of its own.

import { provider } from "../data/index.js";
import { esc, plain, money } from "../format.js";
import { viewHead, withState, documentStatusTag } from "./shared.js";

function todayIso() {
  return new Date().toISOString().slice(0, 10);
}

function queueRows(rows, onDoc) {
  if (rows.length === 0) return `<tr><td colspan="4" class="dim">Nothing open.</td></tr>`;
  return rows.slice(0, 7).map((d) => `
    <tr data-clickable data-doc="${esc(d.qbo_id)}">
      <td class="mono">${esc(d.doc_number ?? d.qbo_id)}</td>
      <td class="trunc">${esc(d.contact_name ?? d.contact_id ?? "—")}</td>
      <td class="r num">${plain(d.total)}</td>
      <td>${documentStatusTag(d)}</td>
    </tr>`).join("");
}

export async function renderDashboard(main) {
  await withState(
    main,
    "today",
    async () => {
      const asOf = todayIso();
      const [ar, ap, openInvoices, openBills, openEstimates, openPOs, sync] = await Promise.all([
        provider.arAging(asOf),
        provider.apAging(asOf),
        provider.openDocuments("Invoice", 0, 7),
        provider.openDocuments("Bill", 0, 7),
        provider.openDocuments("Estimate", 0, 200),
        provider.openDocuments("PurchaseOrder", 0, 7),
        provider.syncStatus(),
      ]);
      return { ar, ap, openInvoices, openBills, openEstimates, openPOs, sync };
    },
    ({ ar, ap, openInvoices, openBills, openEstimates, openPOs, sync }) => {
      const arTotal = Object.values(ar.totals).reduce((a, v) => a + Number(v), 0);
      const apTotal = Object.values(ap.totals).reduce((a, v) => a + Number(v), 0);
      const overdueAr = Number(ar.totals.d31_60) + Number(ar.totals.d61_90) + Number(ar.totals.over_90);

      main.innerHTML = viewHead("Today", "Everything below came off local disk.") + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">A/R outstanding</div><div class="strip-val">${money(String(arTotal.toFixed(2)))}</div><div class="strip-note">${ar.rows.length} customers owe</div></div>
          <div class="strip-cell"><div class="lbl">Past due 30+</div><div class="strip-val ${overdueAr ? "neg" : "pos"}">${money(String(overdueAr.toFixed(2)))}</div></div>
          <div class="strip-cell"><div class="lbl">A/P outstanding</div><div class="strip-val">${money(String(apTotal.toFixed(2)))}</div><div class="strip-note">${ap.rows.length} vendors owed</div></div>
          <div class="strip-cell"><div class="lbl">Open estimates</div><div class="strip-val">${openEstimates.length}</div><div class="strip-note">pending or accepted</div></div>
          <div class="strip-cell"><div class="lbl">Quarantined</div><div class="strip-val ${sync.quarantined_total ? "neg" : "pos"}">${sync.quarantined_total}</div><div class="strip-note">failed to parse</div></div>
        </div>

        <div class="split">
          <div>
            <div class="panel" style="margin-bottom:14px">
              <div class="panel-head"><span class="lbl">Open invoices</span><span class="spacer" style="flex:1"></span>
                <a class="btn" href="#register/Invoice">See all</a></div>
              <div class="grid-scroll"><table>
                <thead><tr><th>No.</th><th>Customer</th><th class="r">Total</th><th>Status</th></tr></thead>
                <tbody>${queueRows(openInvoices, true)}</tbody>
              </table></div>
            </div>
            <div class="panel">
              <div class="panel-head"><span class="lbl">Open purchase orders</span><span class="spacer" style="flex:1"></span>
                <a class="btn" href="#open/PurchaseOrder">See all</a></div>
              <div class="grid-scroll"><table>
                <thead><tr><th>No.</th><th>Vendor</th><th class="r">Total</th><th>Status</th></tr></thead>
                <tbody>${queueRows(openPOs)}</tbody>
              </table></div>
            </div>
          </div>
          <div>
            <div class="panel" style="margin-bottom:14px">
              <div class="panel-head"><span class="lbl">Open bills</span><span class="spacer" style="flex:1"></span>
                <a class="btn" href="#open/Bill">See all</a></div>
              <div class="grid-scroll"><table>
                <thead><tr><th>No.</th><th>Vendor</th><th class="r">Total</th><th>Status</th></tr></thead>
                <tbody>${queueRows(openBills)}</tbody>
              </table></div>
            </div>
            <div class="panel">
              <div class="panel-head"><span class="lbl">Sync</span><span class="spacer" style="flex:1"></span>
                <a class="btn" href="#sync">Details</a></div>
              <div class="panel-body">
                <dl class="kv">
                  <dt>Writes</dt><dd>${sync.write_enabled ? "enabled" : "disabled"}</dd>
                  <dt>Entities</dt><dd>${sync.entities.length} tracked</dd>
                  <dt>Quarantined</dt><dd>${sync.quarantined_total}</dd>
                </dl>
              </div>
            </div>
          </div>
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
