// Audit trail (`docs/MCP.md` "audit_trail", `LEDGER-DESIGN.md` §8): every
// posting since a date (defaulting to the last close), flagged — manual and
// imported — entries first, the list a CPA actually reads.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc } from "../../format.js";
import { viewHead, withState } from "../shared.js";

export async function renderAuditTrail(main) {
  const since = main.dataset.auditSince ?? "";

  await withState(
    main,
    "audit trail",
    async () => {
      const [rows, locked] = await Promise.all([
        ledgerProvider.auditTrail(since || undefined),
        ledgerProvider.lockedThrough(),
      ]);
      return { rows, locked };
    },
    ({ rows, locked }) => {
      main.innerHTML = viewHead(
        "Audit trail",
        since ? `Since ${since}` : `Since the last close (${locked.locked_through ?? "the beginning"})`,
        `<label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">Since</span>
          <input type="date" id="auditSince" value="${esc(since)}" style="border:none;background:none;font:inherit;color:inherit">
        </label>${since ? `<button class="btn" id="auditClear">Use last close</button>` : ""}`,
      ) + `
      <div class="pad">
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Date</th><th>Entry</th><th>Source</th><th>Memo</th><th>Actor</th><th>Command</th><th>Posted</th><th>Flags</th></tr></thead>
          <tbody>${rows.length === 0 ? `<tr><td colspan="8" class="dim">Nothing posted in this window.</td></tr>` : rows.map((r) => `
            <tr>
              <td class="mono dim">${esc(r.entry_date)}</td>
              <td class="mono">${esc(r.entry_id)}</td>
              <td><span class="mono dim">${esc(r.source_type)}</span> ${esc(r.source_id ?? "")}<span class="dim"> v${r.source_version}</span></td>
              <td class="trunc dim">${esc(r.memo ?? "")}</td>
              <td>${esc(r.actor ?? "—")}</td>
              <td class="dim mono">${esc(r.command_kind ?? "—")}</td>
              <td class="dim mono">${r.at ? esc(r.at) : "—"}</td>
              <td>${r.is_flagged ? '<span class="tag warn">flagged</span>' : ""}${r.reversal_of ? ` <span class="tag crit">reverses ${esc(r.reversal_of)}</span>` : ""}</td>
            </tr>`).join("")}</tbody>
        </table></div></div>
        <p class="hint">Flagged entries — manual adjustments and reversals — sort first. A plain document posting carries no flag.</p>
      </div>`;

      main.querySelector("#auditSince")?.addEventListener("change", (e) => {
        main.dataset.auditSince = e.target.value;
        renderAuditTrail(main);
      });
      main.querySelector("#auditClear")?.addEventListener("click", () => {
        main.dataset.auditSince = "";
        renderAuditTrail(main);
      });
    },
  );
}
