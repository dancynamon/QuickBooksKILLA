// Bootstrap: chrome, rail, router, palette. `ROADMAP.md` §B1 — this is the
// only file that wires the pieces together; every screen and every provider
// call lives elsewhere so the Mac session's Tauri work never has to touch
// view code, only `js/data/tauri.js` and the commands crate underneath it.

import { provider, usingFixtures } from "./data/index.js";
import { REALMS, realmName } from "./data/realms.js";
import { createRouter } from "./router.js";
import { initTheme } from "./theme.js";
import { initPalette } from "./palette.js";
import { esc, timeAgo } from "./format.js";

import { renderDashboard } from "./screens/dashboard.js";
import { renderRegister, renderDetail, isDocType } from "./screens/documents.js";
import { renderContact } from "./screens/contacts.js";
import { renderItem } from "./screens/items.js";
import { renderAging } from "./screens/aging.js";
import { renderSync, renderClasses, renderAccounts } from "./screens/reference.js";
import { renderTrialBalance } from "./screens/ledger/trialBalance.js";
import { renderProfitAndLoss } from "./screens/ledger/profitAndLoss.js";
import { renderBalanceSheet } from "./screens/ledger/balanceSheet.js";
import { renderSalesTax } from "./screens/ledger/salesTax.js";
import { renderGeneralLedger } from "./screens/ledger/generalLedger.js";
import { renderAuditTrail } from "./screens/ledger/auditTrail.js";
import { renderAdjustments } from "./screens/ledger/adjustments.js";
import { renderBank } from "./screens/ledger/bank.js";
import { renderPeriodClose } from "./screens/ledger/periodClose.js";

const main = document.getElementById("main");

// ---------------------------------------------------------------------------
// rail navigation
// ---------------------------------------------------------------------------

const NAV = [
  { view: "today", label: "Today", glyph: "◆" },
  { view: "open", arg: "Estimate", label: "Estimates", glyph: "◈" },
  { view: "register", arg: "Invoice", label: "Invoices", glyph: "▤" },
  { view: "open", arg: "PurchaseOrder", label: "Purchase orders", glyph: "▥" },
  { view: "register", arg: "Bill", label: "Bills", glyph: "▧" },
  { view: "register", arg: "Payment", label: "Payments", glyph: "◍" },
  { view: "register", arg: "BillPayment", label: "Bill payments", glyph: "◍" },
];

const REPORTS = [
  { view: "ar", label: "A/R aging", glyph: "▨" },
  { view: "ap", label: "A/P aging", glyph: "▨" },
  { view: "classes", label: "Classes", glyph: "◧" },
  { view: "accounts", label: "Chart of accounts", glyph: "▦" },
  { view: "sync", label: "Sync status", glyph: "↻" },
];

// The ledger's own book (`docs/MCP.md`'s ledger section) — a separate rail
// section because it reads through `ledgerProvider`, not `provider`, and
// carries no realm: `ledger-mcp` is bound to one company per process
// (`apps/desktop/ui/js/data/ledger/provider.js`). No "new journal entry"
// item here or anywhere else in this rail (`LEDGER-DESIGN.md` §4) — every
// entry in this book is generated from a document or an accountant's
// decided adjustment, never hand-posted from a button.
const LEDGER_NAV = [
  { view: "ledger-tb", label: "Trial balance", glyph: "≡" },
  { view: "ledger-pnl", label: "Profit and loss", glyph: "▤" },
  { view: "ledger-bs", label: "Balance sheet", glyph: "▦" },
  { view: "ledger-tax", label: "Sales tax", glyph: "▨" },
  { view: "ledger-gl", label: "General ledger", glyph: "▥" },
  { view: "ledger-audit", label: "Audit trail", glyph: "◈" },
  { view: "ledger-adjustments", label: "Adjustments", glyph: "◑" },
  { view: "ledger-bank", label: "Bank reconciliation", glyph: "◍" },
  { view: "ledger-close", label: "Period close", glyph: "⊘" },
];

function railHref(item) {
  return item.arg ? `#${item.view}/${encodeURIComponent(item.arg)}` : `#${item.view}`;
}

function renderRail(route) {
  const rail = document.getElementById("rail");
  const isCurrent = (item) => route.view === item.view && (item.arg ?? null) === route.arg;

  function section(label, items) {
    const head = `<div class="rail-head lbl">${esc(label)}</div>`;
    const rows = items.map((item) => `
      <a class="rail-item" href="${railHref(item)}" aria-current="${isCurrent(item)}">
        <span class="ri-glyph" aria-hidden="true">${item.glyph}</span>
        <span>${esc(item.label)}</span>
      </a>`).join("");
    return head + rows;
  }

  rail.innerHTML = `<div class="rail-head lbl">${esc(realmName(provider.getRealmId()))}</div>`
    + NAV.map((item) => `
      <a class="rail-item" href="${railHref(item)}" aria-current="${isCurrent(item)}">
        <span class="ri-glyph" aria-hidden="true">${item.glyph}</span>
        <span>${esc(item.label)}</span>
      </a>`).join("")
    + section("Reports", REPORTS)
    + section("Ledger", LEDGER_NAV);
}

// ---------------------------------------------------------------------------
// realm switcher
// ---------------------------------------------------------------------------

function initRealmSwitcher(onSwitch) {
  const btn = document.getElementById("coBtn");
  const menu = document.getElementById("coMenu");
  const nameEl = document.getElementById("coName");
  const realmEl = document.getElementById("coRealm");

  function paint() {
    nameEl.textContent = realmName(provider.getRealmId());
    realmEl.textContent = provider.getRealmId();
    menu.innerHTML = REALMS.map((r) => `
      <button class="co-menu-item" data-realm="${esc(r.id)}" aria-current="${r.id === provider.getRealmId()}">
        <b>${esc(r.name)}</b><span>${esc(r.id)}</span>
      </button>`).join("");
    menu.querySelectorAll("[data-realm]").forEach((el) => {
      el.addEventListener("click", () => {
        closeMenu();
        if (el.dataset.realm === provider.getRealmId()) return;
        provider.setRealmId(el.dataset.realm);
        paint();
        onSwitch();
      });
    });
  }

  function openMenu() {
    menu.hidden = false;
    btn.setAttribute("aria-expanded", "true");
  }
  function closeMenu() {
    menu.hidden = true;
    btn.setAttribute("aria-expanded", "false");
  }

  btn.addEventListener("click", () => (menu.hidden ? openMenu() : closeMenu()));
  document.addEventListener("click", (event) => {
    if (!menu.hidden && !menu.contains(event.target) && event.target !== btn) closeMenu();
  });

  paint();
}

// ---------------------------------------------------------------------------
// sync chrome
// ---------------------------------------------------------------------------

async function refreshSyncPill() {
  const pill = document.getElementById("syncPill");
  const text = document.getElementById("syncText");
  const failPill = document.getElementById("failPill");
  const failCount = document.getElementById("failCount");
  const sbWrites = document.getElementById("sbWrites");
  try {
    const status = await provider.syncStatus();
    const newestCursor = status.entities
      .map((e) => e.last_cdc_cursor)
      .filter(Boolean)
      .sort()
      .at(-1);
    pill.className = `pill ${status.write_enabled ? "is-live" : "is-off"}`;
    text.textContent = newestCursor ? `Synced ${timeAgo(newestCursor)}` : "Never synced";
    failPill.hidden = status.quarantined_total === 0;
    failCount.textContent = String(status.quarantined_total);
    sbWrites.textContent = `Writes ${status.write_enabled ? "enabled" : "disabled"}`;
  } catch {
    pill.className = "pill is-crit";
    text.textContent = "Sync status unavailable";
  }
}

// ---------------------------------------------------------------------------
// router dispatch
// ---------------------------------------------------------------------------

async function renderRoute(route) {
  renderRail(route);
  const sbMs = document.getElementById("sbMs");
  const started = performance.now();

  if (route.view === "today") {
    await renderDashboard(main);
  } else if (route.view === "register" && isDocType(route.arg)) {
    await renderRegister(main, route.arg, false);
  } else if (route.view === "open" && isDocType(route.arg)) {
    await renderRegister(main, route.arg, true);
  } else if (route.view === "doc" && route.arg) {
    await renderDetail(main, route.arg);
  } else if (route.view === "contact" && route.arg) {
    const sep = route.arg.indexOf(":");
    await renderContact(main, route.arg.slice(0, sep), route.arg.slice(sep + 1));
  } else if (route.view === "item" && route.arg) {
    await renderItem(main, route.arg);
  } else if (route.view === "ar") {
    await renderAging(main, "ar");
  } else if (route.view === "ap") {
    await renderAging(main, "ap");
  } else if (route.view === "classes") {
    await renderClasses(main);
  } else if (route.view === "accounts") {
    await renderAccounts(main);
  } else if (route.view === "sync") {
    await renderSync(main);
  } else if (route.view === "ledger-tb") {
    await renderTrialBalance(main);
  } else if (route.view === "ledger-pnl") {
    await renderProfitAndLoss(main);
  } else if (route.view === "ledger-bs") {
    await renderBalanceSheet(main);
  } else if (route.view === "ledger-tax") {
    await renderSalesTax(main);
  } else if (route.view === "ledger-gl") {
    await renderGeneralLedger(main);
  } else if (route.view === "ledger-audit") {
    await renderAuditTrail(main);
  } else if (route.view === "ledger-adjustments") {
    await renderAdjustments(main);
  } else if (route.view === "ledger-bank") {
    await renderBank(main);
  } else if (route.view === "ledger-close") {
    await renderPeriodClose(main);
  } else {
    main.innerHTML = `<div class="state-note">Nothing here. <a href="#today">Back to Today</a>.</div>`;
  }

  sbMs.textContent = `${Math.round(performance.now() - started)}ms`;
}

// ---------------------------------------------------------------------------
// boot
// ---------------------------------------------------------------------------

function boot() {
  document.getElementById("sbProvider").textContent = usingFixtures ? "fixture" : "tauri";

  initTheme({
    onFsChange: (fs) => {
      document.getElementById("sbFs").textContent = `${Math.round(fs * 100)}%`;
    },
  });

  const router = createRouter(renderRoute);

  initRealmSwitcher(() => router.go(router.current().view, router.current().arg));
  initPalette({ provider, go: (view, arg) => router.go(view, arg) });

  refreshSyncPill();
  setInterval(refreshSyncPill, 15_000);

  router.start();
}

boot();
