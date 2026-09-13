// AR and AP aging (`ROADMAP.md` §B1). One screen for both — `ar_aging` and
// `ap_aging` return the same shape, bucketed by customer or by vendor.

import { provider } from "../data/index.js";
import { esc, plain, money } from "../format.js";
import { viewHead, withState } from "./shared.js";

const BUCKETS = [
  ["current", "Current"],
  ["d1_30", "1-30"],
  ["d31_60", "31-60"],
  ["d61_90", "61-90"],
  ["over_90", "90+"],
];

function todayIso() {
  return new Date().toISOString().slice(0, 10);
}

function rowsHtml(rows, kind) {
  if (rows.length === 0) return `<tr><td colspan="7" class="dim">Nothing outstanding.</td></tr>`;
  return rows.map((r) => `
    <tr data-clickable data-contact="${esc(r.contact_id)}">
      <td class="trunc">${esc(r.contact_name)}</td>
      ${BUCKETS.map(([key]) => `<td class="r num" style="color:${bucketColor(key, r.buckets[key])}">${plain(r.buckets[key], { dashForZero: true })}</td>`).join("")}
      <td class="r num" style="font-weight:500">${plain(r.total)}</td>
    </tr>`).join("");
}

function bucketColor(key, value) {
  if (Number(value) === 0) return "var(--ink-dim)";
  if (key === "over_90") return "var(--crit)";
  if (key === "current") return "var(--ink)";
  return "var(--warn)";
}

/** @param {"ar" | "ap"} kind */
export async function renderAging(main, kind) {
  const asOf = main.dataset.agingAsOf || todayIso();
  const title = kind === "ar" ? "A/R aging" : "A/P aging";
  const partyLabel = kind === "ar" ? "customer" : "vendor";
  const contactRoute = kind === "ar" ? "customer" : "vendor";

  await withState(
    main,
    title,
    () => (kind === "ar" ? provider.arAging(asOf) : provider.apAging(asOf)),
    (report) => {
      const grand = Number(report.totals.current) + Number(report.totals.d1_30)
        + Number(report.totals.d31_60) + Number(report.totals.d61_90) + Number(report.totals.over_90);

      main.innerHTML = viewHead(
        title,
        `As of ${report.as_of} · ${report.rows.length} ${partyLabel}${report.rows.length === 1 ? "" : "s"} with a balance`,
        `<label class="btn" style="gap:8px"><span class="lbl" style="color:inherit">As of</span>
          <input type="date" id="agingAsOf" value="${esc(report.as_of)}" style="border:none;background:none;font:inherit;color:inherit">
        </label>`,
      ) + `
      <div class="pad">
        <div class="strip">
          ${BUCKETS.map(([key, label]) => `<div class="strip-cell"><div class="lbl">${label}</div>
            <div class="strip-val ${key === "over_90" ? "neg" : key === "current" ? "pos" : "warn"}">${money(report.totals[key])}</div>
            <div class="strip-note">${grand ? Math.round((Number(report.totals[key]) / grand) * 100) : 0}% of total</div></div>`).join("")}
          <div class="strip-cell"><div class="lbl">Total due</div><div class="strip-val">${money(String(grand.toFixed(2)))}</div></div>
        </div>
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th style="text-transform:capitalize">${esc(partyLabel)}</th>${BUCKETS.map(([, label]) => `<th class="r">${label}</th>`).join("")}<th class="r">Total</th></tr></thead>
          <tbody>${rowsHtml(report.rows, kind)}</tbody>
          ${report.rows.length ? `<tr style="border-top:1px solid var(--line)"><td style="font-weight:600">Total</td>
            ${BUCKETS.map(([key]) => `<td class="r num" style="font-weight:600">${plain(report.totals[key])}</td>`).join("")}
            <td class="r num" style="font-weight:600">${plain(String(grand.toFixed(2)))}</td></tr>` : ""}
        </table></div></div>
      </div>`;

      main.querySelectorAll("tr[data-contact]").forEach((row) => {
        row.addEventListener("click", () => {
          window.location.hash = `#contact/${encodeURIComponent(`${contactRoute}:${row.dataset.contact}`)}`;
        });
      });
      main.querySelector("#agingAsOf")?.addEventListener("change", (event) => {
        main.dataset.agingAsOf = event.target.value;
        renderAging(main, kind);
      });
    },
  );
}
