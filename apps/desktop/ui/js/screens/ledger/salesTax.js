// Sales tax (`docs/MCP.md` "sales_tax_lines", `LEDGER-DESIGN.md` §9): lines
// A through D, the line-level cross-check E and its variance against C, and
// the ST-50 filing mapping printed as a second column — a copy of the
// worksheet, not a translation of it.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso, quarterOf, st50Rows } from "./shared.js";

const LINE_LABELS = [
  ["a_total_income", "A", "Total income"],
  ["b_tax_collected", "B", "Sales tax collected"],
  ["c_taxable_sales", "C", "Taxable sales (B / rate)"],
  ["d_nontaxable_sales", "D", "Non-taxable sales (A - C)"],
];

export async function renderSalesTax(main) {
  const def = quarterOf(todayIso());
  const year = Number(main.dataset.taxYear || def.year);
  const quarter = Number(main.dataset.taxQuarter || def.quarter);

  await withState(
    main,
    "sales tax",
    () => ledgerProvider.salesTaxLines(year, quarter),
    (result) => {
      const varianceOk = result.variance == null || Math.abs(Number(result.variance)) < 0.01;
      main.innerHTML = viewHead(
        "Sales tax",
        `Q${quarter} ${year}`,
        `<label class="btn" style="gap:6px"><span class="lbl" style="color:inherit">Year</span>
          <input type="number" id="taxYear" value="${year}" min="2000" max="2100" style="width:64px;border:none;background:none;font:inherit;color:inherit"></label>
         <div class="seg" role="group" aria-label="Quarter">
          ${[1, 2, 3, 4].map((q) => `<button data-quarter="${q}" aria-pressed="${q === quarter}">Q${q}</button>`).join("")}
         </div>`,
      ) + `
      <div class="pad">
        <div class="grid-wrap" style="margin-bottom:14px"><div class="grid-scroll"><table>
          <thead><tr><th>Line</th><th>What</th><th class="r">Amount</th></tr></thead>
          <tbody>${LINE_LABELS.map(([key, letter, label]) => `
            <tr><td class="mono" style="font-weight:600">${letter}</td><td>${esc(label)}</td><td class="r num">${plain(result[key])}</td></tr>`).join("")}
            <tr style="border-top:1px solid var(--line)">
              <td class="mono">E</td>
              <td>Check: taxable sales from the lines themselves</td>
              <td class="r num">${result.e_line_level_taxable != null ? plain(result.e_line_level_taxable) : "—"}</td>
            </tr>
            <tr>
              <td></td>
              <td class="dim">Variance, E − C</td>
              <td class="r num" style="color:${varianceOk ? "var(--ok)" : "var(--warn)"}">${result.variance != null ? plain(result.variance) : "—"}</td>
            </tr>
          </tbody>
        </table></div></div>

        <div class="panel">
          <div class="panel-head"><span class="lbl">ST-50 filing lines</span></div>
          <div class="grid-scroll"><table>
            <thead><tr><th style="width:64px">Line</th><th>ST-50 says</th><th style="width:56px">From</th><th class="r">Value</th></tr></thead>
            <tbody>${st50Rows(result).map((r) => `
              <tr>
                <td class="mono dim">${r.line}</td>
                <td>${esc(r.label)}</td>
                <td class="mono dim">${r.source ? esc(r.source) : "—"}</td>
                <td class="r num">${plain(r.amount)}</td>
              </tr>`).join("")}</tbody>
          </table></div>
        </div>
        <p class="hint">A non-zero variance between E and C is expected, not a bug — it's what Dan checks before filing (LEDGER-DESIGN.md §9). Lines 4 through 9 print as zero; there has never been an entry on them.</p>
      </div>`;

      main.querySelectorAll("[data-quarter]").forEach((btn) => {
        btn.addEventListener("click", () => {
          main.dataset.taxQuarter = btn.dataset.quarter;
          renderSalesTax(main);
        });
      });
      main.querySelector("#taxYear")?.addEventListener("change", (e) => {
        main.dataset.taxYear = e.target.value;
        renderSalesTax(main);
      });
    },
  );
}
