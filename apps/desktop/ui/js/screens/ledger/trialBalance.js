// Trial balance (`docs/MCP.md` "trial_balance", `LEDGER-DESIGN.md` §7): every
// account's debit and credit total and signed balance as of a date, the
// contra flag shown rather than hidden, and wrong-side accounts highlighted
// — `prototype/bunzbooks.html`'s `trial()` screen is the look this follows.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";

export async function renderTrialBalance(main) {
  const asOf = main.dataset.tbAsOf || todayIso();

  await withState(
    main,
    "trial balance",
    () => ledgerProvider.trialBalance(asOf),
    (tb) => {
      const balanced = tb.total_debits === tb.total_credits;
      const wrongSet = new Set(tb.wrong_side);

      main.innerHTML = viewHead(
        "Trial balance",
        `As of ${asOf}`,
        `<label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">As of</span>
          <input type="date" id="tbAsOf" value="${esc(asOf)}" style="border:none;background:none;font:inherit;color:inherit">
        </label>`,
      ) + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Total debits</div><div class="strip-val">${plain(tb.total_debits)}</div></div>
          <div class="strip-cell"><div class="lbl">Total credits</div><div class="strip-val">${plain(tb.total_credits)}</div></div>
          <div class="strip-cell"><div class="lbl">Difference</div><div class="strip-val ${balanced ? "pos" : "neg"}">${balanced ? "0.00" : plain(String(Number(tb.total_debits) - Number(tb.total_credits)))}</div></div>
          <div class="strip-cell"><div class="lbl">Wrong side</div><div class="strip-val ${tb.wrong_side.length ? "neg" : "pos"}">${tb.wrong_side.length}</div></div>
        </div>
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>No.</th><th>Account</th><th>Type</th><th class="r">Debits</th><th class="r">Credits</th><th class="r">Balance</th><th>Flags</th></tr></thead>
          <tbody>${tb.rows.map((r) => {
            const wrong = wrongSet.has(r.account_id);
            return `<tr style="${wrong ? "background:var(--crit-soft)" : ""}">
              <td class="mono dim">${esc(r.number)}</td>
              <td>${esc(r.name)}${r.needs_mapping ? ' <span class="tag warn">needs mapping</span>' : ""}</td>
              <td class="dim">${esc(r.classification)}</td>
              <td class="r num dim">${Number(r.debit) ? plain(r.debit) : "—"}</td>
              <td class="r num dim">${Number(r.credit) ? plain(r.credit) : "—"}</td>
              <td class="r num" style="font-weight:500;color:${wrong ? "var(--crit)" : "var(--ink)"}">${plain(r.balance)}</td>
              <td>${r.is_contra ? '<span class="tag">contra</span>' : ""}${wrong ? ' <span class="tag crit">wrong side</span>' : ""}</td>
            </tr>`;
          }).join("")}
          <tr style="border-top:1px solid var(--line)">
            <td></td><td style="font-weight:600">Total</td><td></td>
            <td class="r num" style="font-weight:600">${plain(tb.total_debits)}</td>
            <td class="r num" style="font-weight:600">${plain(tb.total_credits)}</td>
            <td class="r num" style="font-weight:600;color:${balanced ? "var(--ok)" : "var(--crit)"}">${balanced ? "in balance" : "out of balance"}</td>
            <td></td>
          </tr></tbody>
        </table></div></div>
        <p class="hint">A wrong-side account (highlighted) sits on the side opposite its normal balance — worth reading before it reaches a statement. Contra accounts are flagged but never counted as wrong-side, since a contra balance sitting "backwards" is what it's there to do.</p>
      </div>`;

      main.querySelector("#tbAsOf")?.addEventListener("change", (event) => {
        main.dataset.tbAsOf = event.target.value;
        renderTrialBalance(main);
      });
    },
  );
}
