// Profit and loss (`docs/MCP.md` "profit_and_loss"): income, COGS and
// expense between two dates, by account and by class, with gross margin and
// net income. Two views over the same result rather than two calls — the
// tool already returns both breakdowns.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";

function defaultFrom() {
  return `${todayIso().slice(0, 4)}-01-01`;
}

function sectionRowsByAccount(section) {
  if (section.by_account.length === 0) return `<tr><td colspan="2" class="dim">Nothing posted.</td></tr>`;
  return section.by_account.map((r) => `
    <tr><td class="mono dim">${esc(r.account_id)}</td><td class="r num">${plain(r.amount)}</td></tr>`).join("");
}

function sectionRowsByClass(section) {
  if (section.by_class.length === 0) return `<tr><td colspan="2" class="dim">Nothing posted.</td></tr>`;
  return section.by_class.map((r) => `
    <tr><td>${r.class_id ? esc(r.class_id) : '<span class="dim">no class</span>'}</td><td class="r num">${plain(r.amount)}</td></tr>`).join("");
}

function sectionPanel(title, section, by) {
  const rows = by === "class" ? sectionRowsByClass(section) : sectionRowsByAccount(section);
  const head = by === "class" ? "Class" : "Account";
  return `<div class="panel" style="margin-bottom:12px">
    <div class="panel-head"><span class="lbl">${esc(title)}</span><span class="spacer" style="flex:1"></span><span class="mono" style="font-weight:500">${plain(section.total)}</span></div>
    <div class="grid-scroll"><table>
      <thead><tr><th>${head}</th><th class="r">Amount</th></tr></thead>
      <tbody>${rows}</tbody>
    </table></div>
  </div>`;
}

export async function renderProfitAndLoss(main) {
  const from = main.dataset.pnlFrom || defaultFrom();
  const to = main.dataset.pnlTo || todayIso();
  const by = main.dataset.pnlBy || "account";

  await withState(
    main,
    "profit and loss",
    () => ledgerProvider.profitAndLoss(from, to),
    (pnl) => {
      main.innerHTML = viewHead(
        "Profit and loss",
        `${from} to ${to} · by ${by}`,
        `<div class="seg" role="group" aria-label="Break out by">
          <button data-by="account" aria-pressed="${by === "account"}">Account</button>
          <button data-by="class" aria-pressed="${by === "class"}">Class</button>
        </div>
        <label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">From</span>
          <input type="date" id="pnlFrom" value="${esc(from)}" style="border:none;background:none;font:inherit;color:inherit"></label>
        <label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">To</span>
          <input type="date" id="pnlTo" value="${esc(to)}" style="border:none;background:none;font:inherit;color:inherit"></label>`,
      ) + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Income</div><div class="strip-val">${plain(pnl.income.total)}</div></div>
          <div class="strip-cell"><div class="lbl">COGS</div><div class="strip-val">${plain(pnl.cogs.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Gross margin</div><div class="strip-val ${Number(pnl.gross_margin) >= 0 ? "pos" : "neg"}">${plain(pnl.gross_margin)}</div></div>
          <div class="strip-cell"><div class="lbl">Expense</div><div class="strip-val">${plain(pnl.expense.total)}</div></div>
          <div class="strip-cell"><div class="lbl">Net income</div><div class="strip-val ${Number(pnl.net_income) >= 0 ? "pos" : "neg"}">${plain(pnl.net_income)}</div></div>
        </div>
        ${sectionPanel("Income", pnl.income, by)}
        ${sectionPanel("Cost of goods sold", pnl.cogs, by)}
        ${sectionPanel("Expense", pnl.expense, by)}
      </div>`;

      main.querySelectorAll("[data-by]").forEach((btn) => {
        btn.addEventListener("click", () => {
          main.dataset.pnlBy = btn.dataset.by;
          renderProfitAndLoss(main);
        });
      });
      main.querySelector("#pnlFrom")?.addEventListener("change", (e) => {
        main.dataset.pnlFrom = e.target.value;
        renderProfitAndLoss(main);
      });
      main.querySelector("#pnlTo")?.addEventListener("change", (e) => {
        main.dataset.pnlTo = e.target.value;
        renderProfitAndLoss(main);
      });
    },
  );
}
