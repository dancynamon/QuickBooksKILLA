// Period close (`docs/MCP.md` "close_period"/"reopen_period",
// `LEDGER-DESIGN.md` §5, D7): the locked-through date, closing forward with
// a mandatory note, and a reopen that is loud by design — a red banner and
// the close-history line it will write, because there is no quiet way to
// relock a period back to where it was.

import { ledgerProvider } from "../../data/ledger/index.js";
import { esc } from "../../format.js";
import { viewHead, withState } from "../shared.js";
import { todayIso } from "./shared.js";

export async function renderPeriodClose(main) {
  const notice = main.dataset.closeNotice ? JSON.parse(main.dataset.closeNotice) : null;
  const reopenNote = main.dataset.reopenNote ?? "";

  await withState(
    main,
    "period close",
    () => ledgerProvider.lockedThrough(),
    ({ locked_through: lockedThrough }) => {
      main.innerHTML = viewHead(
        "Period close",
        lockedThrough ? `Locked through ${lockedThrough}` : "Never closed",
      ) + `
      <div class="pad">
        ${notice ? `<div class="${notice.ok ? "form-ok" : "form-error"}">${esc(notice.text)}${notice.lockedThrough !== undefined ? ` · locked through ${esc(notice.lockedThrough ?? "nothing")}` : ""}</div>` : ""}

        <div class="strip">
          <div class="strip-cell"><div class="lbl">Locked through</div><div class="strip-val" style="font-size:calc(17px * var(--fs))">${esc(lockedThrough ?? "nothing")}</div></div>
          <div class="strip-cell"><div class="lbl">Behaviour</div><div class="strip-val neg" style="font-size:calc(17px * var(--fs))">Reject</div><div class="strip-note">not a warning you can pass</div></div>
        </div>

        <div class="panel" style="margin-bottom:14px">
          <div class="panel-head"><span class="lbl">Close a period</span></div>
          <div class="panel-body">
            <form id="closeForm">
              <div class="form-grid">
                <div class="form-field"><span class="lbl">Period end</span><input class="form-input" type="date" name="periodEnd" value="${esc(todayIso())}" required></div>
                <div class="form-field"><span class="lbl">Actor</span><input class="form-input" name="actor" value="dan" required></div>
                <div class="form-field wide"><span class="lbl">Note (required)</span><input class="form-input" name="note" placeholder="Why this period is closing now" required></div>
              </div>
              <div class="form-actions"><button class="btn btn-primary" type="submit">Close through this date</button></div>
            </form>
          </div>
        </div>

        <div class="panel" style="border-color:color-mix(in srgb, var(--crit) 40%, transparent)">
          <div class="panel-head"><span class="lbl" style="color:var(--crit)">Reopen — loud by design</span></div>
          <div class="panel-body">
            <div class="banner banner-crit">
              <b>Reopening is never quiet.</b> It always writes a close-history line flagged as a reopen,
              whatever the note says — there is no quiet way to relock a period back to where it was.
            </div>
            ${lockedThrough ? `
              <form id="reopenForm">
                <div class="form-grid">
                  <div class="form-field"><span class="lbl">Actor</span><input class="form-input" name="actor" value="dan" required></div>
                  <div class="form-field wide"><span class="lbl">Note (required)</span><input class="form-input" id="reopenNoteInput" name="note" value="${esc(reopenNote)}" placeholder="Why this period is reopening" required></div>
                </div>
                <div class="banner" style="margin-top:8px">
                  <span class="lbl">This will write to close history</span><br>
                  <span class="mono">${esc(todayIso())}Z · by &lt;actor&gt; · REOPENED ${esc(lockedThrough)} → nothing · "${esc(reopenNote) || "…"}"</span>
                </div>
                <div class="form-actions"><button class="btn" style="border-color:var(--crit);color:var(--crit)" type="submit">Reopen through ${esc(lockedThrough)}</button></div>
              </form>` : `<p class="hint" style="margin:0">Nothing is closed, so there is nothing to reopen.</p>`}
          </div>
        </div>
      </div>`;

      main.querySelector("#closeForm")?.addEventListener("submit", async (event) => {
        event.preventDefault();
        const data = new FormData(event.target);
        try {
          const result = await ledgerProvider.closePeriod(String(data.get("periodEnd")), String(data.get("actor")), String(data.get("note")));
          main.dataset.closeNotice = JSON.stringify({ ok: true, text: `Closed through ${result.period_end}.`, lockedThrough: result.locked_through });
        } catch (err) {
          main.dataset.closeNotice = JSON.stringify({ ok: false, text: err.message });
        }
        renderPeriodClose(main);
      });

      main.querySelector("#reopenNoteInput")?.addEventListener("input", (e) => {
        main.dataset.reopenNote = e.target.value;
        const preview = main.querySelector("#reopenForm .banner .mono");
        if (preview) preview.textContent = `${todayIso()}Z · by <actor> · REOPENED ${lockedThrough} → nothing · "${e.target.value || "…"}"`;
      });

      main.querySelector("#reopenForm")?.addEventListener("submit", async (event) => {
        event.preventDefault();
        const data = new FormData(event.target);
        try {
          const result = await ledgerProvider.reopenPeriod(lockedThrough, String(data.get("actor")), String(data.get("note")));
          main.dataset.closeNotice = JSON.stringify({ ok: true, text: `Reopened ${result.period_end}.`, lockedThrough: result.locked_through });
          delete main.dataset.reopenNote;
        } catch (err) {
          main.dataset.closeNotice = JSON.stringify({ ok: false, text: err.message });
        }
        renderPeriodClose(main);
      });
    },
  );
}
