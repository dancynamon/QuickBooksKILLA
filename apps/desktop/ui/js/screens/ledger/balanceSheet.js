// Balance sheet (`docs/MCP.md` "balance_sheet"): assets, liabilities and
// equity as of a date, current-year net income folded into equity as its
// own row (`report.rs::balance_sheet`) so it balances without a formal
// closing entry.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";

function sectionTable(title, section) {
  return `<div class="panel" style="margin-bottom:12px">
    <div class="panel-head"><span class="lbl">${esc(title)}</span><span class="spacer" style="flex:1"></span><span class="mono" style="font-weight:500">${plain(section.total)}</span></div>
    <div class="grid-scroll"><table>
      <thead><tr><th>Account</th><th class="r">Balance</th></tr></thead>
      <tbody>${section.rows.length === 0 ? `<tr><td colspan="2" class="dim">Nothing here.</td></tr>` : section.rows.map((r) => `
        <tr><td>${r.account_id ? `<span class="mono dim">${esc(r.account_id)}</span> ` : ""}${esc(r.name)}</td><td class="r num">${plain(r.balance)}</td></tr>`).join("")}</tbody>
    </table></div>
  </div>`;
}

export async function renderBalanceSheet(main) {
  const asOf = main.dataset.bsAsOf || todayIso();

  await withState(
    main,
    "balance sheet",
    () => ledgerProvider.balanceSheet(asOf),
    (bs) => {
      const balanced = bs.assets.total === bs.total_liabilities_and_equity;
      main.innerHTML = viewHead(
        "Balance sheet",
        `As of ${asOf}`,
        `<label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">As of</span>
          <input type="date" id="bsAsOf" value="${esc(asOf)}" style="border:none;background:none;font:inherit;color:inherit"></label>`,
      ) + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Assets</div><div class="strip-val">${plain(bs.assets.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Liabilities</div><div class="strip-val">${plain(bs.liabilities.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Equity</div><div class="strip-val">${plain(bs.equity.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Liab. + equity</div><div class="strip-val">${plain(bs.total_liabilities_and_equity)}</div></div>
          <div class="strip-cell"><div class="lbl">Balances</div><div class="strip-val ${balanced ? "pos" : "neg"}">${balanced ? "yes" : "no"}</div></div>
        </div>
        ${sectionTable("Assets", bs.assets)}
        ${sectionTable("Liabilities", bs.liabilities)}
        ${sectionTable("Equity", bs.equity)}
      </div>`;

      main.querySelector("#bsAsOf")?.addEventListener("change", (e) => {
        main.dataset.bsAsOf = e.target.value;
        renderBalanceSheet(main);
      });
    },
  );
}
