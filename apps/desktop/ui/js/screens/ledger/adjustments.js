// Adjustments queue (`docs/MCP.md` "list_adjustments"/"propose_adjustment"/
// "decide_adjustment", `docs/ACCOUNTANT.md` "How a request flows"): the
// accountant proposes, validated immediately for balance and the §3 class
// rule; Dan decides. Every write shows the `locked_through` it came back
// with, per the write gate's rule 2.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc, plain } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { stateTag, parseAdjustmentLineSpecs } from "./shared.js";

const STATES = ["proposed", "approved", "rejected", "posted"];

function linesSummary(lines) {
  return lines.map((l) => `${esc(l.account)} ${Number(l.debit) ? `dr ${plain(l.debit)}` : `cr ${plain(l.credit)}`}${l.class ? ` (${esc(l.class)})` : ""}`).join(", ");
}

export async function renderAdjustments(main) {
  const filter = main.dataset.adjFilter || "";
  const notice = main.dataset.adjNotice ? JSON.parse(main.dataset.adjNotice) : null;

  await withState(
    main,
    "adjustments",
    () => ledgerProvider.listAdjustments(filter || undefined),
    (list) => {
      main.innerHTML = viewHead(
        "Adjustments",
        `${list.length} request${list.length === 1 ? "" : "s"}${filter ? ` · ${filter}` : ""}`,
        `<div class="seg" role="group" aria-label="State">
          <button data-filter="" aria-pressed="${filter === ""}">All</button>
          ${STATES.map((s) => `<button data-filter="${s}" aria-pressed="${filter === s}">${s}</button>`).join("")}
        </div>`,
      ) + `
      <div class="pad">
        ${notice ? `<div class="${notice.ok ? "form-ok" : "form-error"}">${esc(notice.text)}${notice.lockedThrough !== undefined ? ` · locked through ${esc(notice.lockedThrough ?? "nothing")}` : ""}</div>` : ""}

        <div class="panel" style="margin-bottom:14px">
          <div class="panel-head"><span class="lbl">Propose an adjustment</span></div>
          <div class="panel-body">
            <form id="proposeForm">
              <div class="form-grid">
                <div class="form-field"><span class="lbl">Requested by</span><input class="form-input" name="requestedBy" value="joel" required></div>
                <div class="form-field wide"><span class="lbl">Description</span><input class="form-input" name="description" placeholder="Why this adjustment is needed" required></div>
                <div class="form-field wide">
                  <span class="lbl">Lines — one per line, ACCOUNT:dr|cr:AMOUNT[:CLASS]</span>
                  <textarea class="form-textarea" name="lines" rows="3" placeholder="6100:dr:125.00:foam&#10;1100:cr:125.00" required></textarea>
                </div>
              </div>
              <div class="form-actions">
                <button class="btn btn-primary" type="submit">Propose</button>
                <span class="hint" style="margin:0">Validated immediately: the lines must balance and every income/COGS/expense line needs a class.</span>
              </div>
            </form>
          </div>
        </div>

        ${list.length === 0 ? `<div class="state-note">Nothing in this queue.</div>` : list.map((r) => `
          <div class="panel" style="margin-bottom:10px" data-request="${esc(r.request_id)}">
            <div class="panel-head">
              <span class="mono dim">${esc(r.request_id)}</span>
              <span class="lbl" style="text-transform:none;font-size:calc(12.5px * var(--fs))">${esc(r.description)}</span>
              <span class="spacer" style="flex:1"></span>
              ${stateTag(r.state)}
              ${r.period_closed ? '<span class="tag crit">period closed</span>' : ""}
            </div>
            <div class="panel-body">
              <p class="hint" style="margin:0 0 8px">Requested by ${esc(r.requested_by)} ${esc(r.requested_at)}. ${esc(linesSummary(r.lines))}.</p>
              ${r.decided_by ? `<p class="hint" style="margin:0 0 8px">Decided by ${esc(r.decided_by)} ${esc(r.decided_at)}${r.decision_note ? ` — "${esc(r.decision_note)}"` : ""}${r.posted_entry_id ? ` · posted as ${esc(r.posted_entry_id)}` : ""}</p>` : ""}
              ${r.state === "proposed" ? `
                <form class="decideForm" data-request="${esc(r.request_id)}">
                  <div class="form-grid">
                    <div class="form-field"><span class="lbl">Decided by</span><input class="form-input" name="decidedBy" value="dan" required></div>
                    <div class="form-field wide"><span class="lbl">Note</span><input class="form-input" name="note" placeholder="Why"></div>
                  </div>
                  <div class="form-actions">
                    <button class="btn btn-primary" type="submit" data-approve="true">Approve and post</button>
                    <button class="btn" type="submit" data-approve="false">Reject</button>
                  </div>
                </form>` : ""}
            </div>
          </div>`).join("")}
      </div>`;

      main.querySelectorAll("[data-filter]").forEach((btn) => {
        btn.addEventListener("click", () => {
          main.dataset.adjFilter = btn.dataset.filter;
          delete main.dataset.adjNotice;
          renderAdjustments(main);
        });
      });

      main.querySelector("#proposeForm")?.addEventListener("submit", async (event) => {
        event.preventDefault();
        const form = event.target;
        const data = new FormData(form);
        try {
          const lines = parseAdjustmentLineSpecs(String(data.get("lines")));
          const result = await ledgerProvider.proposeAdjustment(String(data.get("requestedBy")), String(data.get("description")), lines);
          main.dataset.adjNotice = JSON.stringify({ ok: true, text: `Proposed ${result.request_id}.`, lockedThrough: result.locked_through });
        } catch (err) {
          main.dataset.adjNotice = JSON.stringify({ ok: false, text: err.message });
        }
        renderAdjustments(main);
      });

      main.querySelectorAll(".decideForm").forEach((form) => {
        form.addEventListener("submit", async (event) => {
          event.preventDefault();
          const approve = event.submitter?.dataset.approve === "true";
          const data = new FormData(form);
          try {
            const result = await ledgerProvider.decideAdjustment(
              form.dataset.request, String(data.get("decidedBy")), approve, String(data.get("note") || ""),
            );
            main.dataset.adjNotice = JSON.stringify({
              ok: true,
              text: approve ? `Approved and posted as ${result.posted_entry_id}.` : `Rejected ${result.request_id}.`,
              lockedThrough: result.locked_through,
            });
          } catch (err) {
            main.dataset.adjNotice = JSON.stringify({ ok: false, text: err.message });
          }
          renderAdjustments(main);
        });
      });
    },
  );
}
