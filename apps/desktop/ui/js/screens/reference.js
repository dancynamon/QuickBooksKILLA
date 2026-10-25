// Sync status, classes and the chart of accounts — three small reference
// screens, one per remaining tool that doesn't need a screen of its own.

import { provider } from "../data/index.js";
import { esc, plain, timeAgo } from "../format.js";
import { viewHead, withState } from "./shared.js";

export async function renderSync(main) {
  await withState(
    main,
    "sync status",
    () => provider.syncStatus(),
    (status) => {
      main.innerHTML = viewHead("Sync status", status.write_enabled ? "Writes enabled" : "Writes disabled — read-only") + `
      <div class="pad">
        <div class="strip">
          <div class="strip-cell"><div class="lbl">Writes</div><div class="strip-val">${status.write_enabled ? "Enabled" : "Disabled"}</div></div>
          <div class="strip-cell"><div class="lbl">Entity types</div><div class="strip-val">${status.entities.length}</div></div>
          <div class="strip-cell"><div class="lbl">Quarantined</div><div class="strip-val ${status.quarantined_total ? "neg" : "pos"}">${status.quarantined_total}</div></div>
        </div>
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Entity</th><th class="r">Mirrored</th><th>Last CDC cursor</th><th>Last full sweep</th><th class="r">Quarantined</th></tr></thead>
          <tbody>${status.entities.map((e) => `
            <tr>
              <td class="mono">${esc(e.entity_type)}</td>
              <td class="r num">${e.mirrored}</td>
              <td class="dim">${timeAgo(e.last_cdc_cursor)}</td>
              <td class="dim">${timeAgo(e.last_full_sweep)}</td>
              <td class="r num ${e.quarantined ? "" : "dim"}" style="${e.quarantined ? "color:var(--crit)" : ""}">${e.quarantined || "—"}</td>
            </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;
    },
  );
}

export async function renderClasses(main) {
  await withState(
    main,
    "classes",
    () => provider.classTree(),
    (classes) => {
      main.innerHTML = viewHead("Classes", `${classes.length} on file`) + `
      <div class="pad">
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>Name</th><th>Fully qualified</th><th>Parent</th><th>Active</th></tr></thead>
          <tbody>${classes.map((c) => `
            <tr>
              <td>${esc(c.name)}</td>
              <td class="dim">${esc(c.fully_qualified_name ?? c.name)}</td>
              <td class="dim">${c.parent_id ? esc(c.parent_id) : "—"}</td>
              <td>${c.is_active ? "yes" : "no"}</td>
            </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;
    },
  );
}

export async function renderAccounts(main) {
  await withState(
    main,
    "chart of accounts",
    () => provider.chartOfAccounts(),
    (accounts) => {
      main.innerHTML = viewHead("Chart of accounts", `${accounts.length} accounts`) + `
      <div class="pad">
        <div class="grid-wrap"><div class="grid-scroll"><table>
          <thead><tr><th>No.</th><th>Name</th><th>Type</th><th>Subtype</th><th class="r">Balance</th><th>Active</th></tr></thead>
          <tbody>${accounts.map((a) => `
            <tr>
              <td class="mono dim">${a.acct_num ? esc(a.acct_num) : "—"}</td>
              <td>${esc(a.name)}</td>
              <td class="dim">${esc(a.account_type ?? "—")}</td>
              <td class="dim">${esc(a.account_subtype ?? "—")}</td>
              <td class="r num">${a.balance != null ? plain(a.balance) : "—"}</td>
              <td>${a.is_active ? "yes" : "no"}</td>
            </tr>`).join("")}</tbody>
        </table></div></div>
      </div>`;
    },
  );
}
