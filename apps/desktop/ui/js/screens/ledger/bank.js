// Bank reconciliation (`docs/MCP.md` "bank_status"/"bank_confirm_proposal",
// `docs/BANK.md`): one statement's status — matched, unmatched, proposals —
// and the close rule (`opening + matched = closing`) with the live
// difference, worked down to zero as proposals get confirmed.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { matchKindTag, reconciliationDifference } from "./shared.js";

const DEFAULT_STATEMENT = "stmt-2026-08";

export async function renderBank(main) {
  const statementId = main.dataset.bankStatement || DEFAULT_STATEMENT;
  const notice = main.dataset.bankNotice ? JSON.parse(main.dataset.bankNotice) : null;

  await withState(
    main,
    "bank reconciliation",
    () => ledgerProvider.bankStatus(statementId),
    ({ statement, lines, summary }) => {
      const difference = reconciliationDifference(statement, lines);
      const closes = Number(difference) === 0 && summary.proposed === 0 && summary.unmatched === 0;

      main.innerHTML = viewHead(
        "Bank reconciliation",
        `${esc(statement.statement_id)} · account ${esc(statement.account_id)} · ${esc(statement.period_start)} to ${esc(statement.period_end)}`,
        `<label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">Statement</span>
          <input type="text" id="bankStatement" value="${esc(statementId)}" style="width:130px;border:none;background:none;font:inherit;color:inherit"></label>`,
      ) + `
      <div class="pad">
        ${notice ? `<div class="${notice.ok ? "form-ok" : "form-error"}">${esc(notice.text)}${notice.lockedThrough !== undefined ? ` · locked through ${esc(notice.lockedThrough ?? "nothing")}` : ""}</div>` : ""}

        <div class="strip">
          <div class="strip-cell"><div class="lbl">Opening</div><div class="strip-val">${plain(statement.opening_balance)}</div></div>
          <div class="strip-cell"><div class="lbl">Matched</div><div class="strip-val">${summary.matched}</div></div>
          <div class="strip-cell"><div class="lbl">Proposed</div><div class="strip-val ${summary.proposed ? "warn" : "pos"}">${summary.proposed}</div></div>
          <div class="strip-cell"><div class="lbl">Unmatched</div><div class="strip-val ${summary.unmatched ? "neg" : "pos"}">${summary.unmatched}</div></div>
          <div class="strip-cell"><div class="lbl">Closing (stated)</div><div class="strip-val">${plain(statement.closing_balance)}</div></div>
          <div class="strip-cell"><div class="lbl">Difference</div><div class="strip-val ${closes ? "pos" : "neg"}">${plain(difference)}</div></div>
        </div>
        <p class="hint" style="margin-top:-8px;margin-bottom:12px">opening + matched = closing → ${plain(statement.opening_balance)} + matched = ${plain(statement.closing_balance)}. ${closes ? "The statement closes." : `Off by ${plain(difference)} while lines remain proposed or unmatched.`}</p>

        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Date</th><th>Description</th><th class="r">Amount</th><th>Status</th><th>Confirm to</th></tr></thead>
          <tbody>${lines.map((l) => `
            <tr data-line="${esc(l.line_id)}">
              <td class="mono dim">${esc(l.posted_on)}</td>
              <td class="trunc">${esc(l.description)}</td>
              <td class="r num">${plain(l.amount)}</td>
              <td>${matchKindTag(l.match_kind)}${l.matched_entry_id ? ` <span class="dim mono" style="font-size:calc(11px * var(--fs))">${esc(l.matched_entry_id)}</span>` : ""}</td>
              <td>${l.match_kind === "proposed" ? `
                <form class="confirmForm" data-line="${esc(l.line_id)}" style="display:flex;gap:6px;align-items:center">
                  <input class="form-input" name="account" placeholder="account" style="width:78px" required>
                  <input class="form-input" name="class" placeholder="class">
                  <input class="form-input" name="actor" placeholder="actor" value="dan" style="width:70px" required>
                  <button class="btn btn-primary" type="submit">Confirm</button>
                </form>` : l.match_kind == null ? '<span class="dim">run bank match first</span>' : ""}</td>
            </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;

      main.querySelector("#bankStatement")?.addEventListener("change", (e) => {
        main.dataset.bankStatement = e.target.value.trim();
        delete main.dataset.bankNotice;
        renderBank(main);
      });

      main.querySelectorAll(".confirmForm").forEach((form) => {
        form.addEventListener("submit", async (event) => {
          event.preventDefault();
          const data = new FormData(form);
          try {
            const result = await ledgerProvider.bankConfirmProposal(
              form.dataset.line, String(data.get("account")).trim(),
              String(data.get("class") || "").trim() || undefined, String(data.get("actor")),
            );
            main.dataset.bankNotice = JSON.stringify({ ok: true, text: `Confirmed — posted ${result.entry_id}.`, lockedThrough: result.locked_through });
          } catch (err) {
            main.dataset.bankNotice = JSON.stringify({ ok: false, text: err.message });
          }
          renderBank(main);
        });
      });
    },
  );
}
