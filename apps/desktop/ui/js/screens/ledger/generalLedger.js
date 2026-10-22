// General ledger (`docs/MCP.md` "general_ledger", `LEDGER-DESIGN.md` §8):
// every posted line between two dates, grouped by account, with a running
// balance carried in from before the range. Clicking a line calls
// `entries_for_document` for that line's source document and expands its
// full entry (every line, not just this account's) plus the document id
// behind it.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";

function defaultFrom() {
  const d = new Date();
  return `${d.getUTCFullYear()}-${String(d.getUTCMonth() + 1).padStart(2, "0")}-01`;
}

function entryDetailPanel(rows) {
  return `<div class="panel-body" style="padding:8px 12px">
    <div class="grid-scroll"><table>
      <thead><tr><th>#</th><th>Account</th><th>Class</th><th class="r">Debit</th><th class="r">Credit</th></tr></thead>
      <tbody>${rows.map((l) => `
        <tr><td class="mono dim">${l.line_no}</td><td class="mono">${esc(l.account)}</td><td class="dim">${l.class ? esc(l.class) : "—"}</td>
          <td class="r num">${Number(l.debit) ? plain(l.debit) : "—"}</td><td class="r num">${Number(l.credit) ? plain(l.credit) : "—"}</td></tr>`).join("")}</tbody>
    </table></div>
  </div>`;
}

export async function renderGeneralLedger(main) {
  const from = main.dataset.glFrom || defaultFrom();
  const to = main.dataset.glTo || todayIso();
  const account = main.dataset.glAccount || "";

  await withState(
    main,
    "general ledger",
    () => ledgerProvider.generalLedger(from, to, account || undefined),
    (detail) => {
      main.innerHTML = viewHead(
        "General ledger",
        `${from} to ${to}${account ? ` · account ${account}` : ""}`,
        `<label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">From</span>
          <input type="date" id="glFrom" value="${esc(from)}" style="border:none;background:none;font:inherit;color:inherit"></label>
        <label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">To</span>
          <input type="date" id="glTo" value="${esc(to)}" style="border:none;background:none;font:inherit;color:inherit"></label>
        <label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">Account</span>
          <input type="text" id="glAccount" value="${esc(account)}" placeholder="all" style="width:64px;border:none;background:none;font:inherit;color:inherit"></label>`,
      ) + `
      <div class="pad">
        ${detail.sections.map((section) => `
          <div class="panel" style="margin-bottom:12px" data-section="${esc(section.account_id)}">
            <div class="panel-head">
              <span class="lbl">${esc(section.number)} — ${esc(section.name)}</span>
              <span class="spacer" style="flex:1"></span>
              <span class="dim" style="font-size:calc(11px * var(--fs))">opening ${plain(section.opening_balance)}</span>
            </div>
            <div class="grid-scroll"><table>
              <thead><tr><th>Date</th><th>Source</th><th>Memo</th><th class="r">Debit</th><th class="r">Credit</th><th class="r">Running</th></tr></thead>
              <tbody>${section.lines.length === 0 ? `<tr><td colspan="6" class="dim">Nothing posted in range.</td></tr>` : section.lines.map((l) => `
                <tr data-clickable data-entry="${esc(l.entry_id)}" data-source="${esc(l.source_id ?? "")}">
                  <td class="mono dim">${esc(l.entry_date)}</td>
                  <td><span class="mono dim">${esc(l.source_type)}</span> ${esc(l.source_id ?? "")}${l.is_flagged ? ' <span class="tag warn">flagged</span>' : ""}</td>
                  <td class="trunc dim">${esc(l.memo ?? "")}${l.class ? ` · ${esc(l.class)}` : ""}</td>
                  <td class="r num">${Number(l.debit) ? plain(l.debit) : "—"}</td>
                  <td class="r num">${Number(l.credit) ? plain(l.credit) : "—"}</td>
                  <td class="r num" style="font-weight:500">${plain(l.running_balance)}</td>
                </tr>
                <tr class="gl-detail" data-detail-for="${esc(l.entry_id)}" hidden><td colspan="6" style="padding:0"></td></tr>`).join("")}</tbody>
              ${section.lines.length ? `<tr style="border-top:1px solid var(--line)"><td colspan="5" style="font-weight:600">Closing balance</td><td class="r num" style="font-weight:600">${plain(section.closing_balance)}</td></tr>` : ""}
            </table></div>
          </div>`).join("")}
        <p class="hint">Click a line to see every line of the entry behind it, and the document it came from.</p>
      </div>`;

      main.querySelectorAll("tr[data-entry]").forEach((row) => {
        row.addEventListener("click", async () => {
          const detailRow = main.querySelector(`tr[data-detail-for="${CSS.escape(row.dataset.entry)}"]`);
          if (!detailRow) return;
          if (!detailRow.hidden) {
            detailRow.hidden = true;
            return;
          }
          main.querySelectorAll(".gl-detail").forEach((r) => { r.hidden = true; });
          const cell = detailRow.querySelector("td");
          cell.innerHTML = `<div class="state-note">Loading entry…</div>`;
          detailRow.hidden = false;
          try {
            const entries = await ledgerProvider.entriesForDocument(row.dataset.source);
            const match = entries.find((e) => e.entry_id === row.dataset.entry) ?? entries[0];
            if (!match) {
              cell.innerHTML = `<div class="state-note">No entry found for document ${esc(row.dataset.source)}.</div>`;
              return;
            }
            cell.innerHTML = `<div class="panel" style="margin:6px 0;border-style:dashed">
              <div class="panel-head"><span class="lbl">Document ${esc(row.dataset.source)}</span>
                <span class="spacer" style="flex:1"></span>
                <span class="dim mono" style="font-size:calc(11px * var(--fs))">${esc(match.entry_id)}</span></div>
              ${entryDetailPanel(match.entry.lines)}
            </div>`;
          } catch (err) {
            cell.innerHTML = `<div class="state-note is-error">${esc(err.message)}</div>`;
          }
        });
      });

      main.querySelector("#glFrom")?.addEventListener("change", (e) => { main.dataset.glFrom = e.target.value; renderGeneralLedger(main); });
      main.querySelector("#glTo")?.addEventListener("change", (e) => { main.dataset.glTo = e.target.value; renderGeneralLedger(main); });
      main.querySelector("#glAccount")?.addEventListener("change", (e) => { main.dataset.glAccount = e.target.value.trim(); renderGeneralLedger(main); });
    },
  );
}
