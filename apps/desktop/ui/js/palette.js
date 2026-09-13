// Command palette and reference search. `prototype/README.md`: type any
// number and it resolves against every number a record can be known by —
// a document number, a customer PO, a SKU, an amount — and says which one
// matched, rather than guessing. That is exactly `provider.search`'s own
// contract (`docs/MCP.md`), so this file is thin: a static command list for
// navigation, `provider.search` for everything else, and the usual
// type-ahead list keyboard.

import { esc, plain } from "./format.js";

const REASON_LABEL = {
  DocumentNumber: "document no.",
  PurchaseOrderNumber: "customer PO",
  Sku: "SKU",
  Amount: "amount",
  DocumentNumberPrefix: "document no.",
  Text: "match",
};

const GLYPH_BY_DOC_TYPE = {
  Estimate: "◈", Invoice: "▤", SalesReceipt: "▤", CreditMemo: "◑", RefundReceipt: "◑",
  Payment: "◍", PurchaseOrder: "▥", Bill: "▧", BillPayment: "◍", VendorCredit: "◐",
  Purchase: "▨", Deposit: "◍", JournalEntry: "≡",
};

function commands(go) {
  return [
    { id: "today", label: "Today", glyph: "◆", act: () => go("today") },
    { id: "invoices", label: "Invoices — register", glyph: "▤", act: () => go("register", "Invoice") },
    { id: "estimates", label: "Estimates — open", glyph: "◈", act: () => go("open", "Estimate") },
    { id: "pos", label: "Purchase orders — open", glyph: "▥", act: () => go("open", "PurchaseOrder") },
    { id: "bills", label: "Bills — register", glyph: "▧", act: () => go("register", "Bill") },
    { id: "payments", label: "Payments — register", glyph: "◍", act: () => go("register", "Payment") },
    { id: "ar", label: "A/R aging", glyph: "▨", act: () => go("ar") },
    { id: "ap", label: "A/P aging", glyph: "▨", act: () => go("ap") },
    { id: "classes", label: "Classes", glyph: "◧", act: () => go("classes") },
    { id: "accounts", label: "Chart of accounts", glyph: "▦", act: () => go("accounts") },
    { id: "sync", label: "Sync status", glyph: "↻", act: () => go("sync") },
  ];
}

function fuzzy(haystack, needle) {
  const h = haystack.toLowerCase();
  const n = needle.toLowerCase();
  let i = 0;
  for (const ch of h) {
    if (ch === n[i]) i += 1;
    if (i === n.length) return true;
  }
  return n.length === 0;
}

function hitTarget(hit) {
  if (hit.kind === "Document") return { view: "doc", arg: hit.qbo_id };
  if (hit.kind === "Contact") return { view: "contact", arg: `${hit.contact_type === "Vendor" ? "vendor" : "customer"}:${hit.qbo_id}` };
  return { view: "item", arg: hit.qbo_id };
}

function hitLabel(hit) {
  if (hit.kind === "Document") return `${hit.doc_type} ${hit.doc_number ?? hit.qbo_id}${hit.contact_name ? " — " + hit.contact_name : ""}`;
  if (hit.kind === "Contact") return hit.display_name;
  return `${hit.sku ?? hit.qbo_id} — ${hit.name}`;
}

function hitMeta(hit) {
  if (hit.kind === "Document") return plain(hit.total);
  if (hit.kind === "Contact") return hit.balance != null ? plain(hit.balance) : "";
  return hit.unit_price != null ? plain(hit.unit_price) : "";
}

function hitGlyph(hit) {
  if (hit.kind === "Document") return GLYPH_BY_DOC_TYPE[hit.doc_type] ?? "▤";
  if (hit.kind === "Contact") return "◑";
  return "▦";
}

/**
 * Wire the command palette. `provider` is passed in rather than imported,
 * so the palette always searches whichever realm is currently selected —
 * `app.js` calls `attach` once and the palette reads `provider.search`
 * fresh on every keystroke, after whatever realm switch has happened.
 */
export function initPalette({ provider, go }) {
  const scrim = document.getElementById("scrim");
  const input = document.getElementById("palInput");
  const list = document.getElementById("palList");
  const timeEl = document.getElementById("palTime");

  let items = [];
  let activeIndex = 0;
  let requestId = 0;

  function open() {
    scrim.hidden = false;
    input.value = "";
    input.focus();
    renderCommandsOnly();
  }

  function close() {
    scrim.hidden = true;
    document.getElementById("main")?.focus();
  }

  function renderList(sections) {
    items = sections.flatMap((s) => s.items);
    activeIndex = items.length ? 0 : -1;
    if (items.length === 0) {
      list.innerHTML = `<div class="pal-empty">No matches.</div>`;
      return;
    }
    let index = -1;
    list.innerHTML = sections.map((section) => {
      if (section.items.length === 0) return "";
      return `<div class="pal-sec lbl">${esc(section.label)}</div>` + section.items.map((item) => {
        index += 1;
        return `<button class="pal-item" data-idx="${index}" data-active="${index === activeIndex}">
          <span class="pi-glyph" aria-hidden="true">${item.glyph}</span>
          <span class="pi-main">${esc(item.label)}</span>
          ${item.reason ? `<span class="pi-why${item.soft ? " soft" : ""}">${esc(item.reason)}</span>` : ""}
          <span class="pi-meta">${esc(item.meta ?? "")}</span>
        </button>`;
      }).join("");
    }).join("");
    list.querySelectorAll(".pal-item").forEach((el) => {
      el.addEventListener("mouseenter", () => setActive(Number(el.dataset.idx)));
      el.addEventListener("click", () => activate(Number(el.dataset.idx)));
    });
  }

  function setActive(index) {
    activeIndex = index;
    list.querySelectorAll(".pal-item").forEach((el) => {
      el.dataset.active = String(Number(el.dataset.idx) === activeIndex);
    });
    list.querySelector('[data-active="true"]')?.scrollIntoView({ block: "nearest" });
  }

  function activate(index) {
    const item = items[index];
    if (!item) return;
    close();
    item.act();
  }

  function renderCommandsOnly() {
    const cmdItems = commands(go).map((c) => ({ label: c.label, glyph: c.glyph, act: c.act }));
    renderList([{ label: "Jump to", items: cmdItems }]);
    timeEl.textContent = "";
  }

  async function renderQuery(query) {
    const cmdItems = commands(go)
      .filter((c) => fuzzy(c.label, query))
      .map((c) => ({ label: c.label, glyph: c.glyph, act: c.act }));

    const myRequest = (requestId += 1);
    const started = performance.now();
    let hits = [];
    try {
      hits = await provider.search(query, 20);
    } catch {
      hits = [];
    }
    if (myRequest !== requestId) return; // a newer keystroke has already superseded this one

    const elapsed = Math.round(performance.now() - started);
    timeEl.textContent = `${hits.length} match${hits.length === 1 ? "" : "es"} · ${elapsed}ms`;

    const hitItems = hits.map((h) => {
      const target = hitTarget(h.hit);
      return {
        label: hitLabel(h.hit),
        glyph: hitGlyph(h.hit),
        meta: hitMeta(h.hit),
        reason: REASON_LABEL[h.reason] ?? h.reason,
        act: () => go(target.view, target.arg),
      };
    });

    renderList([
      { label: "Commands", items: cmdItems },
      { label: "Records", items: hitItems },
    ]);
  }

  input.addEventListener("input", () => {
    const q = input.value.trim();
    if (q === "") renderCommandsOnly();
    else renderQuery(q);
  });

  input.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      if (items.length) setActive((activeIndex + 1) % items.length);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      if (items.length) setActive((activeIndex - 1 + items.length) % items.length);
    } else if (event.key === "Enter") {
      event.preventDefault();
      activate(activeIndex);
    } else if (event.key === "Escape") {
      event.preventDefault();
      close();
    }
  });

  scrim.addEventListener("mousedown", (event) => {
    if (event.target === scrim) close();
  });

  document.getElementById("omniBtn")?.addEventListener("click", open);

  document.addEventListener("keydown", (event) => {
    const meta = event.metaKey || event.ctrlKey;
    if (meta && event.key.toLowerCase() === "k") {
      event.preventDefault();
      open();
    }
  });

  return { open, close };
}
