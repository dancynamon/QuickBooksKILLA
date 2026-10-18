import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { Aging, api, ApiError, ContactType, Customer, DocType, Document, DocumentDetail, Draft, DraftLine, Hit, Item, Realm, SalesRow, SearchResult, Status, Write } from "./api";
import { createContext, useContext } from "react";
import { ago, DOC_LABELS, DOC_PLURALS, formatDate, formatDecimal, formatMoney, lineAmountMinor } from "./format";

// ---------------------------------------------------------------------------
// Routing: the hash is the whole navigation state, so back/forward and a
// reload land exactly where you were.
// ---------------------------------------------------------------------------

type Report = "ar" | "ap" | "sales";

type Route =
  | { view: "list"; docType: DocType }
  | { view: "report"; report: Report }
  | { view: "items" }
  | { view: "item"; itemId: string }
  | { view: "new"; docType: DocType }
  | { view: "edit"; qboId: string }
  | { view: "writes" }
  | { view: "doc"; qboId: string }
  | { view: "contact"; contactType: ContactType; contactId: string };

function parseRoute(hash: string): { realm: string | null; route: Route } {
  const parts = hash.replace(/^#\/?/, "").split("/").map(decodeURIComponent);
  const fallback: Route = { view: "list", docType: "Invoice" };
  if (parts[0] !== "r" || !parts[1]) return { realm: null, route: fallback };
  const realm = parts[1];
  if (parts[2] === "doc" && parts[3]) return { realm, route: { view: "doc", qboId: parts[3] } };
  if (parts[2] === "contact" && (parts[3] === "Customer" || parts[3] === "Vendor") && parts[4])
    return { realm, route: { view: "contact", contactType: parts[3], contactId: parts[4] } };
  if (parts[2] === "report" && (parts[3] === "ar" || parts[3] === "ap" || parts[3] === "sales"))
    return { realm, route: { view: "report", report: parts[3] } };
  if (parts[2] === "items") return { realm, route: { view: "items" } };
  if (parts[2] === "writes") return { realm, route: { view: "writes" } };
  if (parts[2] === "new" && EDITABLE.includes(parts[3] as DocType)) return { realm, route: { view: "new", docType: parts[3] as DocType } };
  if (parts[2] === "edit" && parts[3]) return { realm, route: { view: "edit", qboId: parts[3] } };
  if (parts[2] === "item" && parts[3]) return { realm, route: { view: "item", itemId: parts[3] } };
  if (parts[2] === "list" && parts[3] && parts[3] in DOC_LABELS)
    return { realm, route: { view: "list", docType: parts[3] as DocType } };
  return { realm, route: fallback };
}

function href(realm: string, route: Route): string {
  switch (route.view) {
    case "list":
      return `#/r/${realm}/list/${route.docType}`;
    case "doc":
      return `#/r/${realm}/doc/${encodeURIComponent(route.qboId)}`;
    case "contact":
      return `#/r/${realm}/contact/${route.contactType}/${encodeURIComponent(route.contactId)}`;
    case "report":
      return `#/r/${realm}/report/${route.report}`;
    case "items":
      return `#/r/${realm}/items`;
    case "item":
      return `#/r/${realm}/item/${encodeURIComponent(route.itemId)}`;
    case "new":
      return `#/r/${realm}/new/${route.docType}`;
    case "edit":
      return `#/r/${realm}/edit/${encodeURIComponent(route.qboId)}`;
    case "writes":
      return `#/r/${realm}/writes`;
  }
}

function useHash(): string {
  const [hash, setHash] = useState(window.location.hash);
  useEffect(() => {
    const on = () => setHash(window.location.hash);
    window.addEventListener("hashchange", on);
    return () => window.removeEventListener("hashchange", on);
  }, []);
  return hash;
}

// Per-viewer conveniences only. Storage can be unavailable; the app must work
// without it.
function stored(key: string, fallback: string): string {
  try {
    return window.localStorage.getItem(key) ?? fallback;
  } catch {
    return fallback;
  }
}

function store(key: string, value: string) {
  try {
    window.localStorage.setItem(key, value);
  } catch {
    /* storage unavailable: preference lasts this session only */
  }
}

const REGISTERS: { group: string; types: DocType[] }[] = [
  { group: "Sales", types: ["Estimate", "Invoice", "SalesReceipt", "Payment", "CreditMemo", "RefundReceipt"] },
  { group: "Purchases", types: ["PurchaseOrder", "Bill", "BillPayment", "VendorCredit", "Purchase"] },
  { group: "Banking", types: ["Deposit", "JournalEntry"] },
];

// Class id to name for the current company (DESIGN.md §2.4).
const Classes = createContext<Record<string, string>>({});

function useClassName() {
  const classes = useContext(Classes);
  return (id: string | null) => (id ? classes[id] ?? id : "");
}

// Document types the editor writes; the rest are entered in QuickBooks.
const EDITABLE: DocType[] = ["Estimate", "Invoice", "SalesReceipt", "CreditMemo", "PurchaseOrder", "Bill"];

// Whether this company takes writes, and a way to refresh the chrome after one.
interface WriteState {
  enabled: boolean;
  environment: "production" | "sandbox";
  refresh: () => void;
}
const Writes = createContext<WriteState>({ enabled: false, environment: "production", refresh: () => {} });

const REPORT_LABELS: Record<Report, string> = {
  ar: "A/R aging",
  ap: "A/P aging",
  sales: "Sales by customer",
};

const VENDOR_SIDE = new Set<DocType>(["PurchaseOrder", "Bill", "BillPayment", "VendorCredit", "Purchase"]);

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

export function App() {
  const hash = useHash();
  const { realm: routedRealm, route } = parseRoute(hash);
  const [realms, setRealms] = useState<Realm[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [status, setStatus] = useState<Status | null>(null);
  const [paletteOpen, setPaletteOpen] = useState(false);
  const [theme, setTheme] = useState(() => stored("bb.theme", "auto"));
  const [scale, setScale] = useState(() => Number(stored("bb.scale", "1")) || 1);
  const [now, setNow] = useState(Date.now());
  const [classes, setClasses] = useState<Record<string, string>>({});

  useEffect(() => {
    api.realms().then(setRealms, (e: ApiError) => setError(e.message));
  }, []);

  const realm = routedRealm ?? realms?.[0]?.realmId ?? null;
  const realmRow = realms?.find((r) => r.realmId === realm) ?? null;

  useEffect(() => {
    if (realms && realms.length > 0 && !routedRealm) {
      window.location.replace(href(realms[0].realmId, route));
    }
  }, [realms, routedRealm, route]);

  useEffect(() => {
    if (realm) api.classes(realm).then(setClasses, () => setClasses({}));
  }, [realm]);

  // The chrome's sync state: polled, never something to go and look for.
  // Faster while writes are in flight, so "queued" turns to "landed" promptly.
  const [statusTick, setStatusTick] = useState(0);
  const busy = (status?.outboxPending ?? 0) > 0;
  useEffect(() => {
    if (!realm) return;
    let live = true;
    const load = () =>
      api.status(realm).then(
        (s) => live && setStatus(s),
        () => live && setStatus(null),
      );
    load();
    const timer = window.setInterval(() => {
      load();
      setNow(Date.now());
    }, busy ? 3_000 : 15_000);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, [realm, busy, statusTick]);

  useEffect(() => {
    if (theme === "auto") delete document.documentElement.dataset.theme;
    else document.documentElement.dataset.theme = theme;
    store("bb.theme", theme);
  }, [theme]);

  useEffect(() => {
    document.documentElement.style.setProperty("--fs", String(scale));
    store("bb.scale", String(scale));
  }, [scale]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
        e.preventDefault();
        setPaletteOpen((open) => !open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (error) return <div className="fatal">Replica unavailable: {error}</div>;
  if (!realms) return <div className="fatal muted">Opening replica…</div>;
  if (!realm || !realmRow) return <div className="fatal">No company in this replica. Run a sync first.</div>;

  const go = (next: Route) => {
    window.location.hash = href(realm, next);
  };

  const writeState: WriteState = {
    enabled: status?.writeEnabled ?? false,
    environment: status?.environment ?? "production",
    refresh: () => setStatusTick((t) => t + 1),
  };

  return (
    <Writes.Provider value={writeState}>
    <Classes.Provider value={classes}>
    <div className="shell">
      <header className="chrome">
        <a className="wordmark" href={href(realm, { view: "list", docType: "Invoice" })} aria-label="BunzBooks home">
          <Bun />
          <span>BunzBooks</span>
        </a>
        <select
          className="realm"
          aria-label="Company"
          value={realm}
          onChange={(e) => (window.location.hash = href(e.target.value, { view: "list", docType: "Invoice" }))}
        >
          {realms.map((r) => (
            <option key={r.realmId} value={r.realmId}>
              {r.displayName}
            </option>
          ))}
        </select>
        <button className="search-trigger" onClick={() => setPaletteOpen(true)}>
          <span>Find a number, name, SKU or amount</span>
          <kbd>{navigator.platform.includes("Mac") ? "⌘K" : "Ctrl K"}</kbd>
        </button>
        <div className="chrome-tools">
          <button aria-label="Smaller text" onClick={() => setScale((s) => Math.max(0.9, +(s - 0.1).toFixed(1)))}>
            A−
          </button>
          <button aria-label="Larger text" onClick={() => setScale((s) => Math.min(1.7, +(s + 0.1).toFixed(1)))}>
            A+
          </button>
          <button
            aria-label="Theme"
            title={`Theme: ${theme}`}
            onClick={() => setTheme((t) => (t === "auto" ? "dark" : t === "dark" ? "light" : "auto"))}
          >
            {theme === "dark" ? "Dark" : theme === "light" ? "Light" : "Auto"}
          </button>
        </div>
      </header>

      <nav className="rail" aria-label="Registers">
        {REGISTERS.map((group) => (
          <div key={group.group} className="rail-group">
            <div className="rail-head">{group.group}</div>
            {group.types.map((t) => {
              const count = status?.entityTypes.find((e) => e.entityType === t)?.count;
              const active = route.view === "list" && route.docType === t;
              return (
                <a key={t} className={active ? "rail-item active" : "rail-item"} href={href(realm, { view: "list", docType: t })}>
                  <span>{DOC_PLURALS[t]}</span>
                  {count !== undefined && <span className="rail-count num">{count.toLocaleString()}</span>}
                </a>
              );
            })}
          </div>
        ))}
        <div className="rail-group">
          <div className="rail-head">Lists</div>
          <a className={route.view === "items" || route.view === "item" ? "rail-item active" : "rail-item"} href={href(realm, { view: "items" })}>
            <span>Products and services</span>
          </a>
          <a className={route.view === "writes" ? "rail-item active" : "rail-item"} href={href(realm, { view: "writes" })}>
            <span>Sync queue</span>
            {status && status.outboxPending + status.outboxNeedsAttention > 0 && (
              <span className={status.outboxNeedsAttention > 0 ? "rail-count crit-text num" : "rail-count num"}>
                {status.outboxPending + status.outboxNeedsAttention}
              </span>
            )}
          </a>
        </div>
        <div className="rail-group">
          <div className="rail-head">Reports</div>
          {(["ar", "ap", "sales"] as Report[]).map((r) => (
            <a key={r} className={route.view === "report" && route.report === r ? "rail-item active" : "rail-item"} href={href(realm, { view: "report", report: r })}>
              <span>{REPORT_LABELS[r]}</span>
            </a>
          ))}
        </div>
      </nav>

      <main className="main">
        {route.view === "list" && <Register key={`${realm}:${route.docType}`} realm={realm} docType={route.docType} go={go} />}
        {route.view === "doc" && <DocumentView key={`${realm}:${route.qboId}`} realm={realm} qboId={route.qboId} go={go} />}
        {route.view === "report" && route.report !== "sales" && (
          <AgingReport key={`${realm}:${route.report}`} realm={realm} side={route.report === "ar" ? "receivable" : "payable"} />
        )}
        {route.view === "report" && route.report === "sales" && <SalesReport key={realm} realm={realm} />}
        {route.view === "items" && <ItemsView key={realm} realm={realm} go={go} />}
        {route.view === "item" && <ItemView key={`${realm}:${route.itemId}`} realm={realm} itemId={route.itemId} />}
        {route.view === "new" && <Editor key={`${realm}:new:${route.docType}`} realm={realm} docType={route.docType} go={go} />}
        {route.view === "edit" && <Editor key={`${realm}:edit:${route.qboId}`} realm={realm} qboId={route.qboId} go={go} />}
        {route.view === "writes" && <WritesView key={realm} realm={realm} />}
        {route.view === "contact" && (
          <ContactView key={`${realm}:${route.contactId}`} realm={realm} contactType={route.contactType} contactId={route.contactId} go={go} />
        )}
      </main>

      <StatusBar status={status} now={now} realm={realm} />

      {paletteOpen && <Palette realm={realm} go={go} close={() => setPaletteOpen(false)} />}
    </div>
    </Classes.Provider>
    </Writes.Provider>
  );
}

function Bun() {
  return (
    <svg viewBox="0 0 32 24" width="26" height="20" aria-hidden="true">
      <path d="M3 12c0-6 6-10 13-10s13 4 13 10z" fill="var(--bun-top)" />
      <ellipse cx="11" cy="6" rx="1.2" ry=".7" fill="var(--bun-seed)" />
      <ellipse cx="17" cy="4.6" rx="1.2" ry=".7" fill="var(--bun-seed)" />
      <ellipse cx="22" cy="7" rx="1.2" ry=".7" fill="var(--bun-seed)" />
      <rect x="2" y="13" width="28" height="3.5" rx="1.7" fill="var(--bun-fill)" />
      <path d="M3 18h26c0 2.5-2 4-5 4H8c-3 0-5-1.5-5-4z" fill="var(--bun-heel)" />
    </svg>
  );
}

// ---------------------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------------------

function StatusBar({ status, now, realm }: { status: Status | null; now: number; realm: string }) {
  if (!status) return <footer className="statusbar">Status unavailable</footer>;
  const total = status.entityTypes.reduce((sum, t) => sum + t.count, 0);
  return (
    <footer className="statusbar" role="status">
      <span title={status.lastSynced ?? "never"}>Synced {ago(status.lastSynced, now)}</span>
      <a href={href(realm, { view: "writes" })} className={status.outboxPending > 0 ? "pill warn" : "pill"}>{status.outboxPending} queued</a>
      <a href={href(realm, { view: "writes" })} className={status.outboxNeedsAttention > 0 ? "pill crit" : "pill"}>{status.outboxNeedsAttention} need attention</a>
      {status.quarantined > 0 && <span className="pill warn">{status.quarantined} quarantined</span>}
      <span className="spacer" />
      <span className="num">{total.toLocaleString()} records on disk</span>
      {status.environment === "sandbox" && <span className="pill">Sandbox</span>}
      <span className={status.writeEnabled ? "pill warn" : "pill ok"}>{status.writeEnabled ? "Writes on" : "Read-only"}</span>
    </footer>
  );
}

// ---------------------------------------------------------------------------
// Register
// ---------------------------------------------------------------------------

type SortKey = "txnDate" | "docNumber" | "contactName" | "totalMinor" | "balanceMinor";

function Register({ realm, docType, go }: { realm: string; docType: DocType; go: (r: Route) => void }) {
  const [rows, setRows] = useState<Document[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  const [openOnly, setOpenOnly] = useState(false);
  const [sort, setSort] = useState<{ key: SortKey; desc: boolean }>({ key: "txnDate", desc: true });
  const [cursor, setCursor] = useState(0);
  const [elapsed, setElapsed] = useState<number | null>(null);
  const body = useRef<HTMLTableSectionElement>(null);

  useEffect(() => {
    const started = performance.now();
    api.documents(realm, docType).then(
      (r) => {
        setRows(r);
        setElapsed(performance.now() - started);
      },
      (e: ApiError) => setError(e.message),
    );
  }, [realm, docType]);

  const hasBalance = rows?.some((r) => r.balanceMinor !== null) ?? false;

  const visible = useMemo(() => {
    if (!rows) return [];
    const needle = filter.trim().toLowerCase();
    const filtered = rows.filter((r) => {
      if (openOnly && !(r.balanceMinor && r.balanceMinor !== 0)) return false;
      if (!needle) return true;
      return [r.docNumber, r.contactName, r.poNumber, r.status, formatMoney(r.totalMinor), r.txnDate]
        .some((v) => v?.toLowerCase().includes(needle));
    });
    const dir = sort.desc ? -1 : 1;
    return filtered.sort((a, b) => {
      const x = a[sort.key] ?? "";
      const y = b[sort.key] ?? "";
      if (sort.key === "docNumber") {
        return dir * String(x).localeCompare(String(y), undefined, { numeric: true });
      }
      return x < y ? -dir : x > y ? dir : 0;
    });
  }, [rows, filter, openOnly, sort]);

  useEffect(() => setCursor(0), [filter, openOnly, sort]);

  // Only the visible window is rendered: a nine-thousand-row register costs
  // the same to draw as a forty-row one. Rows are fixed height, measured once.
  const scroller = useRef<HTMLDivElement>(null);
  const [rowHeight, setRowHeight] = useState(28);
  const [view, setView] = useState({ top: 0, height: 800 });

  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const measure = () => {
      const row = el.querySelector("tbody tr[data-index]");
      if (row) setRowHeight(row.getBoundingClientRect().height || 28);
      setView({ top: el.scrollTop, height: el.clientHeight });
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(el);
    return () => observer.disconnect();
  }, [rows]);

  useEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const header = el.querySelector("thead")?.getBoundingClientRect().height ?? 0;
    const footer = el.querySelector("tfoot")?.getBoundingClientRect().height ?? 0;
    const rowTop = cursor * rowHeight;
    if (rowTop < el.scrollTop) el.scrollTop = rowTop;
    else if (rowTop + rowHeight > el.scrollTop + el.clientHeight - header - footer)
      el.scrollTop = rowTop + rowHeight - el.clientHeight + header + footer;
  }, [cursor, rowHeight]);

  const OVERSCAN = 20;
  const first = Math.max(0, Math.floor(view.top / rowHeight) - OVERSCAN);
  const last = Math.min(visible.length, Math.ceil((view.top + view.height) / rowHeight) + OVERSCAN);

  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "ArrowDown") {
      e.preventDefault();
      setCursor((c) => Math.min(visible.length - 1, c + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setCursor((c) => Math.max(0, c - 1));
    } else if (e.key === "Enter" && visible[cursor]) {
      go({ view: "doc", qboId: visible[cursor].qboId });
    }
  };

  const header = (key: SortKey, label: string, className = "") => (
    <th
      className={className}
      aria-sort={sort.key === key ? (sort.desc ? "descending" : "ascending") : "none"}
      onClick={() => setSort((s) => ({ key, desc: s.key === key ? !s.desc : key !== "contactName" }))}
    >
      {label}
      {sort.key === key ? (sort.desc ? " ▾" : " ▴") : ""}
    </th>
  );

  const openTotal = visible.reduce((sum, r) => sum + (r.balanceMinor ?? 0), 0);
  const total = visible.reduce((sum, r) => sum + r.totalMinor, 0);

  return (
    <section className="register" onKeyDown={onKey}>
      <div className="toolbar">
        <h1>{DOC_PLURALS[docType]}</h1>
        <input
          className="filter"
          placeholder="Filter this register"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          autoFocus
          aria-label="Filter"
        />
        {hasBalance && (
          <label className="check">
            <input type="checkbox" checked={openOnly} onChange={(e) => setOpenOnly(e.target.checked)} /> Open only
          </label>
        )}
        <span className="spacer" />
        {EDITABLE.includes(docType) && <NewButton realm={realm} docType={docType} />}
        {elapsed !== null && rows && (
          <span className="muted num">
            {visible.length.toLocaleString()} of {rows.length.toLocaleString()} · {elapsed.toFixed(0)} ms
          </span>
        )}
      </div>
      {error && <div className="error">{error}</div>}
      {!rows && !error && <div className="muted pad">Loading…</div>}
      {rows && rows.length === 0 && <div className="muted pad">No {DOC_PLURALS[docType].toLowerCase()} in the replica.</div>}
      {rows && rows.length > 0 && (
        <div className="grid-wrap" ref={scroller} onScroll={(e) => setView({ top: e.currentTarget.scrollTop, height: e.currentTarget.clientHeight })}>
          <table className="grid">
            <thead>
              <tr>
                {header("txnDate", "Date")}
                {header("docNumber", "No.")}
                {header("contactName", VENDOR_SIDE.has(docType) ? "Vendor" : "Customer")}
                <th>PO</th>
                <th>Status</th>
                {header("totalMinor", "Total", "r")}
                {hasBalance && header("balanceMinor", "Open", "r")}
              </tr>
            </thead>
            <tbody ref={body}>
              {first > 0 && (
                <tr aria-hidden="true" style={{ height: first * rowHeight }}>
                  <td colSpan={7} className="spacer-row" />
                </tr>
              )}
              {visible.slice(first, last).map((r, offset) => {
                const i = first + offset;
                return (
                <tr
                  key={r.qboId}
                  data-index={i}
                  className={i === cursor ? "cursor" : ""}
                  onClick={() => setCursor(i)}
                  onDoubleClick={() => go({ view: "doc", qboId: r.qboId })}
                >
                  <td className="num">{formatDate(r.txnDate)}</td>
                  <td className="num">
                    <a href={href(realm, { view: "doc", qboId: r.qboId })}>{r.docNumber ?? "—"}</a>
                  </td>
                  <td className="ellipsis">
                    {r.contactId && r.contactType ? (
                      <a href={href(realm, { view: "contact", contactType: r.contactType, contactId: r.contactId })}>
                        {r.contactName ?? r.contactId}
                      </a>
                    ) : (
                      <span className="muted">—</span>
                    )}
                  </td>
                  <td className="num muted">{r.poNumber ?? ""}</td>
                  <td>
                    <Badge doc={r} />
                  </td>
                  <td className="num r">{formatMoney(r.totalMinor)}</td>
                  {hasBalance && <td className="num r">{r.balanceMinor ? formatMoney(r.balanceMinor) : ""}</td>}
                </tr>
                );
              })}
              {last < visible.length && (
                <tr aria-hidden="true" style={{ height: (visible.length - last) * rowHeight }}>
                  <td colSpan={7} className="spacer-row" />
                </tr>
              )}
            </tbody>
            <tfoot>
              <tr>
                <td colSpan={5}>{visible.length.toLocaleString()} shown</td>
                <td className="num r">{formatMoney(total)}</td>
                {hasBalance && <td className="num r">{formatMoney(openTotal)}</td>}
              </tr>
            </tfoot>
          </table>
        </div>
      )}
    </section>
  );
}

function Badge({ doc }: { doc: Document }) {
  if (doc.pendingWrite) {
    const failed = doc.pendingWrite === "conflicted" || doc.pendingWrite === "rejected";
    return <span className={failed ? "badge crit" : "badge warn"}>{doc.status}</span>;
  }
  if (doc.balanceMinor !== null) {
    if (doc.balanceMinor === 0) return <span className="badge ok">Paid</span>;
    const overdue = doc.dueDate && doc.dueDate < new Date().toISOString().slice(0, 10);
    if (doc.balanceMinor !== doc.totalMinor) return <span className={overdue ? "badge crit" : "badge warn"}>Partial</span>;
    return <span className={overdue ? "badge crit" : "badge"}>{overdue ? "Overdue" : "Open"}</span>;
  }
  return doc.status ? <span className="badge">{doc.status}</span> : null;
}

// ---------------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------------

function DocumentView({ realm, qboId, go }: { realm: string; qboId: string; go: (r: Route) => void }) {
  const [detail, setDetail] = useState<DocumentDetail | null>(null);
  const [error, setError] = useState<string | null>(null);

  const writes = useContext(Writes);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    api.document(realm, qboId).then(setDetail, (e: ApiError) => setError(e.message));
  }, [realm, qboId, tick]);
  // While a write about this document is on its way, look again until it lands.
  const waiting = !!detail && (!!detail.document.pendingWrite || detail.openWrites.some((w) => w.state === "pending" || w.state === "in_flight"));
  useEffect(() => {
    if (!waiting) return;
    const timer = window.setTimeout(() => setTick((t) => t + 1), 3_000);
    return () => window.clearTimeout(timer);
  }, [waiting, tick]);

  const className = useClassName();
  if (error) return <div className="error pad">{error}</div>;
  if (!detail) return <div className="muted pad">Loading…</div>;
  const d = detail.document;

  return (
    <section className="document">
      <div className="toolbar">
        <a className="back" href={href(realm, { view: "list", docType: d.docType })}>
          ← {DOC_PLURALS[d.docType]}
        </a>
        <h1>
          {DOC_LABELS[d.docType]} <span className="num">{d.docNumber ?? `#${d.qboId}`}</span>
        </h1>
        <Badge doc={d} />
        <span className="spacer" />
        {EDITABLE.includes(d.docType) && !d.pendingWrite && writes.enabled ? (
          <a className="button" href={href(realm, { view: "edit", qboId: d.qboId })}>Edit</a>
        ) : (
          <span className="muted">{writes.enabled ? "Edit in QuickBooks" : "Read-only · edit in QuickBooks"}</span>
        )}
      </div>
      {d.pendingWrite && (
        <p className={d.pendingWrite === "conflicted" || d.pendingWrite === "rejected" ? "banner crit" : "banner"}>
          {d.pendingWrite === "rejected" || d.pendingWrite === "conflicted"
            ? <>QuickBooks did not accept this document. <a href={href(realm, { view: "writes" })}>Open the sync queue</a> to retry or discard it.</>
            : "Saved here. Not yet in QuickBooks: it will be sent in the background, and this page updates when it lands. Tax is added by QuickBooks."}
        </p>
      )}
      {!d.pendingWrite && detail.openWrites.map((w) => (
        <p key={w.id} className={w.state === "conflicted" || w.state === "rejected" ? "banner crit" : "banner"}>
          {w.state === "conflicted"
            ? <>Someone changed this {DOC_LABELS[d.docType].toLowerCase()} in QuickBooks after your edit was made. <a href={href(realm, { view: "writes" })}>Review it in the sync queue</a>.</>
            : w.state === "rejected"
              ? <>QuickBooks refused your edit: {w.lastError}. <a href={href(realm, { view: "writes" })}>Open the sync queue</a>.</>
              : "Your edit is saved here and on its way to QuickBooks. The figures below are QuickBooks' until it lands."}
        </p>
      ))}

      <dl className="facts">
        <div>
          <dt>{d.contactType ?? "Contact"}</dt>
          <dd>
            {d.contactId && d.contactType ? (
              <a href={href(realm, { view: "contact", contactType: d.contactType, contactId: d.contactId })}>{d.contactName ?? d.contactId}</a>
            ) : (
              "—"
            )}
          </dd>
        </div>
        <div>
          <dt>Date</dt>
          <dd className="num">{formatDate(d.txnDate)}</dd>
        </div>
        {d.dueDate && (
          <div>
            <dt>Due</dt>
            <dd className="num">{formatDate(d.dueDate)}</dd>
          </div>
        )}
        {d.poNumber && (
          <div>
            <dt>Customer PO</dt>
            <dd className="num">{d.poNumber}</dd>
          </div>
        )}
        <div>
          <dt>Total</dt>
          <dd className="num">{formatMoney(d.totalMinor)}</dd>
        </div>
        {d.balanceMinor !== null && (
          <div>
            <dt>Open</dt>
            <dd className="num">{formatMoney(d.balanceMinor)}</dd>
          </div>
        )}
        {d.classId && (
          <div>
            <dt>Class</dt>
            <dd>{className(d.classId)}</dd>
          </div>
        )}
      </dl>
      {d.privateNote && <p className="note">Note: {d.privateNote}</p>}

      <div className="grid-wrap">
        <table className="grid lines">
          <thead>
            <tr>
              <th>#</th>
              <th>Description</th>
              <th className="r">Qty</th>
              <th className="r">Rate</th>
              <th className="r">Amount</th>
              <th>Class</th>
              <th>Tax</th>
            </tr>
          </thead>
          <tbody>
            {detail.lines.map((l) => (
              <tr key={l.lineNo}>
                <td className="num muted">{l.lineNo}</td>
                <td>{l.description ?? <span className="muted">—</span>}</td>
                <td className="num r">{formatDecimal(l.qty)}</td>
                <td className="num r">{formatDecimal(l.unitPrice)}</td>
                <td className="num r">{formatMoney(l.amountMinor)}</td>
                <td className="muted">{className(l.classId)}</td>
                <td className="muted">{l.taxable ? "Tax" : ""}</td>
              </tr>
            ))}
            {detail.lines.length === 0 && (
              <tr>
                <td colSpan={7} className="muted">
                  No item lines
                </td>
              </tr>
            )}
          </tbody>
          <tfoot>
            <tr>
              <td colSpan={4}>Total</td>
              <td className="num r">{formatMoney(d.totalMinor)}</td>
              <td colSpan={2} />
            </tr>
          </tfoot>
        </table>
      </div>

      <h2>Chain</h2>
      {detail.related.length <= 1 && detail.unresolved.length === 0 ? (
        <p className="muted indent">Not linked to any other document.</p>
      ) : (
        <ol className="chain">
          {[...detail.related]
            .sort((a, b) => a.txnDate.localeCompare(b.txnDate))
            .map((r) => (
              <li key={r.qboId} className={r.qboId === d.qboId ? "here" : ""}>
                <button onClick={() => go({ view: "doc", qboId: r.qboId })} disabled={r.qboId === d.qboId}>
                  <span className="chain-type">{DOC_LABELS[r.docType]}</span>
                  <span className="num">{r.docNumber ?? `#${r.qboId}`}</span>
                  <span className="num muted">{formatDate(r.txnDate)}</span>
                  <span className="num">{formatMoney(r.totalMinor)}</span>
                </button>
              </li>
            ))}
          {detail.unresolved.map((id) => (
            <li key={id} className="gap">
              Linked record {id} is not in the replica
            </li>
          ))}
        </ol>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Contact
// ---------------------------------------------------------------------------

function ContactView({
  realm,
  contactType,
  contactId,
  go,
}: {
  realm: string;
  contactType: ContactType;
  contactId: string;
  go: (r: Route) => void;
}) {
  const [rows, setRows] = useState<Document[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [aging, setAging] = useState<Aging["rows"][number] | null>(null);
  useEffect(() => {
    api.contactDocuments(realm, contactType, contactId).then(setRows, (e: ApiError) => setError(e.message));
    // The statement view: this contact's open balance by days past due.
    api.aging(realm, contactType === "Customer" ? "receivable" : "payable").then(
      (a) => setAging(a.rows.find((r) => r.contactId === contactId) ?? null),
      () => setAging(null),
    );
  }, [realm, contactType, contactId]);

  if (error) return <div className="error pad">{error}</div>;
  if (!rows) return <div className="muted pad">Loading…</div>;
  const name = rows.find((r) => r.contactName)?.contactName ?? `${contactType} ${contactId}`;
  const open = rows.reduce((sum, r) => sum + (r.docType === "Invoice" || r.docType === "Bill" ? r.balanceMinor ?? 0 : 0), 0);
  const sorted = [...rows].sort((a, b) => b.txnDate.localeCompare(a.txnDate));

  return (
    <section className="register">
      <div className="toolbar">
        <h1>{name}</h1>
        <span className="badge">{contactType}</span>
        <span className="spacer" />
        <span className="num">Open {formatMoney(open)}</span>
      </div>
      {aging && (
        <dl className="facts aging-strip" aria-label="Open balance by days past due">
          {AGING_COLUMNS.map((c) => (
            <div key={c.key}>
              <dt>{c.label}</dt>
              <dd className={c.key === "over90Minor" && aging.over90Minor ? "num crit-text" : "num"}>{formatMoney(aging[c.key] as number)}</dd>
            </div>
          ))}
        </dl>
      )}
      <div className="grid-wrap">
        <table className="grid">
          <thead>
            <tr>
              <th>Date</th>
              <th>Type</th>
              <th>No.</th>
              <th>Status</th>
              <th className="r">Total</th>
              <th className="r">Open</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((r) => (
              <tr key={r.qboId} onDoubleClick={() => go({ view: "doc", qboId: r.qboId })}>
                <td className="num">{formatDate(r.txnDate)}</td>
                <td>{DOC_LABELS[r.docType]}</td>
                <td className="num">
                  <a href={href(realm, { view: "doc", qboId: r.qboId })}>{r.docNumber ?? `#${r.qboId}`}</a>
                </td>
                <td>
                  <Badge doc={r} />
                </td>
                <td className="num r">{formatMoney(r.totalMinor)}</td>
                <td className="num r">{r.balanceMinor ? formatMoney(r.balanceMinor) : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Command palette
// ---------------------------------------------------------------------------

function Palette({ realm, go, close }: { realm: string; go: (r: Route) => void; close: () => void }) {
  const [query, setQuery] = useState("");
  const [result, setResult] = useState<SearchResult | null>(null);
  const [cursor, setCursor] = useState(0);
  const sequence = useRef(0);

  useEffect(() => {
    const ticket = ++sequence.current;
    if (!query.trim()) {
      setResult(null);
      return;
    }
    api.search(realm, query).then((r) => {
      // Responses can return out of order; only the latest keystroke wins.
      if (ticket === sequence.current) {
        setResult(r);
        setCursor(0);
      }
    });
  }, [realm, query]);

  const open = useCallback(
    (hit: Hit) => {
      close();
      if (hit.kind === "document") go({ view: "doc", qboId: hit.document.qboId });
      else if (hit.kind === "contact") go({ view: "contact", contactType: hit.contactType, contactId: hit.qboId });
    },
    [close, go],
  );

  const hits = result?.hits ?? [];
  const onKey = (e: React.KeyboardEvent) => {
    if (e.key === "Escape") close();
    else if (e.key === "ArrowDown") {
      e.preventDefault();
      setCursor((c) => Math.min(hits.length - 1, c + 1));
    } else if (e.key === "ArrowUp") {
      e.preventDefault();
      setCursor((c) => Math.max(0, c - 1));
    } else if (e.key === "Enter" && hits[cursor]) open(hits[cursor]);
  };

  return (
    <div className="scrim" onMouseDown={close}>
      <div className="palette" role="dialog" aria-label="Search" onMouseDown={(e) => e.stopPropagation()} onKeyDown={onKey}>
        <input autoFocus value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Invoice 9005, PO-41230, Harbor, RT-RED, 1234.56" aria-label="Search" />
        <ul className="hits">
          {hits.map((hit, i) => (
            <li key={i} className={i === cursor ? "cursor" : ""} onMouseEnter={() => setCursor(i)} onClick={() => open(hit)}>
              <HitRow hit={hit} />
              <span className="reason">{hit.reason}</span>
            </li>
          ))}
          {result && hits.length === 0 && <li className="muted">Nothing matches.</li>}
        </ul>
        <div className="palette-foot muted">
          {result ? `${hits.length} results · ${(result.elapsedMicros / 1000).toFixed(1)} ms local` : "Searches the replica on disk; no network."}
        </div>
      </div>
    </div>
  );
}

function HitRow({ hit }: { hit: Hit }) {
  switch (hit.kind) {
    case "document":
      return (
        <span className="hit hit-doc">
          <span className="hit-type">{DOC_LABELS[hit.document.docType]}</span>
          <span className="num">{hit.document.docNumber ?? `#${hit.document.qboId}`}</span>
          <span className="ellipsis">{hit.document.contactName}</span>
          <span className="num muted">{formatDate(hit.document.txnDate)}</span>
          <span className="num">{formatMoney(hit.document.totalMinor)}</span>
        </span>
      );
    case "contact":
      return (
        <span className="hit hit-contact">
          <span className="hit-type">{hit.contactType}</span>
          <span className="ellipsis">{hit.displayName}</span>
          <span className="num">{hit.balanceMinor ? formatMoney(hit.balanceMinor) : ""}</span>
        </span>
      );
    case "item":
      return (
        <span className="hit hit-item">
          <span className="hit-type">Item</span>
          <span className="ellipsis">{hit.name}</span>
          <span className="num muted">{hit.sku}</span>
          <span className="num">{formatDecimal(hit.unitPrice)}</span>
        </span>
      );
  }
}

// ---------------------------------------------------------------------------
// Reports
// ---------------------------------------------------------------------------

const todayIso = () => {
  const d = new Date();
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
};

const AGING_COLUMNS: { key: keyof Aging["rows"][number]; label: string }[] = [
  { key: "currentMinor", label: "Current" },
  { key: "days1to30Minor", label: "1–30" },
  { key: "days31to60Minor", label: "31–60" },
  { key: "days61to90Minor", label: "61–90" },
  { key: "over90Minor", label: "Over 90" },
  { key: "totalMinor", label: "Total" },
];

function AgingReport({ realm, side }: { realm: string; side: "receivable" | "payable" }) {
  const [asOf, setAsOf] = useState(todayIso);
  const [data, setData] = useState<Aging | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (!/^\d{4}-\d{2}-\d{2}$/.test(asOf)) return;
    api.aging(realm, side, asOf).then(setData, (e: ApiError) => setError(e.message));
  }, [realm, side, asOf]);
  const contactType: ContactType = side === "receivable" ? "Customer" : "Vendor";
  const totals = AGING_COLUMNS.map((c) => (data?.rows ?? []).reduce((sum, r) => sum + (r[c.key] as number), 0));

  return (
    <section className="register">
      <div className="toolbar">
        <h1>{side === "receivable" ? "A/R aging" : "A/P aging"}</h1>
        <label className="check">
          As of <input type="date" className="filter date" value={asOf} onChange={(e) => setAsOf(e.target.value)} aria-label="As of" />
        </label>
        <span className="spacer" />
        <span className="muted">Open {side === "receivable" ? "invoices" : "bills"} by days past due · balances as of last sync</span>
      </div>
      {error && <div className="error">{error}</div>}
      {!data && !error && <div className="muted pad">Loading…</div>}
      {data && data.rows.length === 0 && <div className="muted pad">Nothing open on {formatDate(data.asOf)}.</div>}
      {data && data.rows.length > 0 && (
        <div className="grid-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>{contactType}</th>
                {AGING_COLUMNS.map((c) => (
                  <th key={c.key} className="r">{c.label}</th>
                ))}
                <th className="r">Docs</th>
              </tr>
            </thead>
            <tbody>
              {data.rows.map((r) => (
                <tr key={r.contactId}>
                  <td className="ellipsis">
                    <a href={href(realm, { view: "contact", contactType, contactId: r.contactId })}>{r.contactName ?? r.contactId}</a>
                  </td>
                  {AGING_COLUMNS.map((c) => (
                    <td key={c.key} className={c.key === "over90Minor" && r.over90Minor ? "num r crit-text" : "num r"}>
                      {(r[c.key] as number) ? formatMoney(r[c.key] as number) : ""}
                    </td>
                  ))}
                  <td className="num r muted">{r.documents}</td>
                </tr>
              ))}
            </tbody>
            <tfoot>
              <tr>
                <td>{data.rows.length} {contactType.toLowerCase()}s</td>
                {totals.map((t, i) => (
                  <td key={i} className="num r">{formatMoney(t)}</td>
                ))}
                <td />
              </tr>
            </tfoot>
          </table>
        </div>
      )}
    </section>
  );
}

function SalesReport({ realm }: { realm: string }) {
  const year = new Date().getFullYear();
  const [from, setFrom] = useState(`${year}-01-01`);
  const [to, setTo] = useState(todayIso);
  const [rows, setRows] = useState<SalesRow[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const ok = /^\d{4}-\d{2}-\d{2}$/;
    if (!ok.test(from) || !ok.test(to)) return;
    api.salesByCustomer(realm, from, to).then(setRows, (e: ApiError) => setError(e.message));
  }, [realm, from, to]);
  const total = (rows ?? []).reduce((sum, r) => sum + r.netSalesMinor, 0);
  const max = Math.max(1, ...(rows ?? []).map((r) => r.netSalesMinor));

  return (
    <section className="register">
      <div className="toolbar">
        <h1>Sales by customer</h1>
        <label className="check">
          From <input type="date" className="filter date" value={from} onChange={(e) => setFrom(e.target.value)} aria-label="From" />
        </label>
        <label className="check">
          To <input type="date" className="filter date" value={to} onChange={(e) => setTo(e.target.value)} aria-label="To" />
        </label>
        <span className="spacer" />
        <span className="muted">Line amounts, before tax, less credit memos and refunds</span>
      </div>
      {error && <div className="error">{error}</div>}
      {!rows && !error && <div className="muted pad">Loading…</div>}
      {rows && rows.length === 0 && <div className="muted pad">No sales in this range.</div>}
      {rows && rows.length > 0 && (
        <div className="grid-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Customer</th>
                <th className="r">Net sales</th>
                <th className="bar-col">Share</th>
                <th className="r">Docs</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.contactId}>
                  <td className="ellipsis">
                    <a href={href(realm, { view: "contact", contactType: "Customer", contactId: r.contactId })}>{r.contactName ?? r.contactId}</a>
                  </td>
                  <td className="num r">{formatMoney(r.netSalesMinor)}</td>
                  <td className="bar-col">
                    <span className="inline-bar" style={{ width: `${Math.max(0, (100 * r.netSalesMinor) / max)}%` }} />
                  </td>
                  <td className="num r muted">{r.documents}</td>
                </tr>
              ))}
            </tbody>
            <tfoot>
              <tr>
                <td>{rows.length} customers</td>
                <td className="num r">{formatMoney(total)}</td>
                <td colSpan={2} />
              </tr>
            </tfoot>
          </table>
        </div>
      )}
    </section>
  );
}

// ---------------------------------------------------------------------------
// Items
// ---------------------------------------------------------------------------

function ItemsView({ realm, go }: { realm: string; go: (r: Route) => void }) {
  const [items, setItems] = useState<Item[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [filter, setFilter] = useState("");
  useEffect(() => {
    api.items(realm).then(setItems, (e: ApiError) => setError(e.message));
  }, [realm]);
  const needle = filter.trim().toLowerCase();
  const shown = (items ?? []).filter((i) => !needle || [i.name, i.sku, i.itemType].some((v) => v?.toLowerCase().includes(needle)));

  return (
    <section className="register">
      <div className="toolbar">
        <h1>Products and services</h1>
        <input className="filter" placeholder="Filter by name or SKU" value={filter} onChange={(e) => setFilter(e.target.value)} autoFocus aria-label="Filter" />
        <span className="spacer" />
        {items && <span className="muted num">{shown.length.toLocaleString()} of {items.length.toLocaleString()}</span>}
      </div>
      {error && <div className="error">{error}</div>}
      {!items && !error && <div className="muted pad">Loading…</div>}
      {items && (
        <div className="grid-wrap">
          <table className="grid">
            <thead>
              <tr>
                <th>Name</th>
                <th>SKU</th>
                <th>Type</th>
                <th className="r">Price</th>
                <th className="r">Cost</th>
                <th className="r">On hand</th>
                <th className="r">Used on</th>
              </tr>
            </thead>
            <tbody>
              {shown.map((i) => (
                <tr key={i.qboId} onDoubleClick={() => go({ view: "item", itemId: i.qboId })} className={i.active ? "" : "inactive"}>
                  <td className="ellipsis">
                    <a href={href(realm, { view: "item", itemId: i.qboId })}>{i.name}</a>
                  </td>
                  <td className="num muted">{i.sku ?? ""}</td>
                  <td className="muted">{i.itemType ?? ""}</td>
                  <td className="num r">{formatDecimal(i.unitPrice)}</td>
                  <td className="num r">{formatDecimal(i.purchaseCost)}</td>
                  <td className="num r">{formatDecimal(i.qtyOnHand)}</td>
                  <td className="num r muted">{i.usedOn.toLocaleString()}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

function ItemView({ realm, itemId }: { realm: string; itemId: string }) {
  const [item, setItem] = useState<Item | null>(null);
  const [docs, setDocs] = useState<Document[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    api.items(realm).then((all) => setItem(all.find((i) => i.qboId === itemId) ?? null), (e: ApiError) => setError(e.message));
    api.itemDocuments(realm, itemId).then(setDocs, (e: ApiError) => setError(e.message));
  }, [realm, itemId]);

  if (error) return <div className="error pad">{error}</div>;
  if (!docs) return <div className="muted pad">Loading…</div>;
  const sorted = [...docs].sort((a, b) => b.txnDate.localeCompare(a.txnDate));
  const price = item?.unitPrice ? Number(item.unitPrice) : null;
  const cost = item?.purchaseCost ? Number(item.purchaseCost) : null;
  // Display only: a ratio of two prices, never an amount that is stored or summed.
  const margin = price && cost !== null && price > 0 ? `${(((price - cost) / price) * 100).toFixed(1)}%` : "—";

  return (
    <section className="document">
      <div className="toolbar">
        <a className="back" href={href(realm, { view: "items" })}>← Products and services</a>
        <h1>{item?.name ?? `Item ${itemId}`}</h1>
        {item?.sku && <span className="badge">{item.sku}</span>}
      </div>
      <dl className="facts">
        <div><dt>Price</dt><dd className="num">{formatDecimal(item?.unitPrice ?? null) || "—"}</dd></div>
        <div><dt>Cost</dt><dd className="num">{formatDecimal(item?.purchaseCost ?? null) || "—"}</dd></div>
        <div><dt>Margin</dt><dd className="num">{margin}</dd></div>
        <div><dt>On hand</dt><dd className="num">{formatDecimal(item?.qtyOnHand ?? null) || "—"}</dd></div>
        <div><dt>Used on</dt><dd className="num">{docs.length.toLocaleString()} documents</dd></div>
      </dl>
      <h2>Where used</h2>
      <div className="grid-wrap">
        <table className="grid">
          <thead>
            <tr>
              <th>Date</th>
              <th>Type</th>
              <th>No.</th>
              <th>Contact</th>
              <th className="r">Total</th>
            </tr>
          </thead>
          <tbody>
            {sorted.map((d) => (
              <tr key={d.qboId}>
                <td className="num">{formatDate(d.txnDate)}</td>
                <td>{DOC_LABELS[d.docType]}</td>
                <td className="num">
                  <a href={href(realm, { view: "doc", qboId: d.qboId })}>{d.docNumber ?? `#${d.qboId}`}</a>
                </td>
                <td className="ellipsis">{d.contactName ?? ""}</td>
                <td className="num r">{formatMoney(d.totalMinor)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </section>
  );
}

// ---------------------------------------------------------------------------
// Writing (M2): the editor and the sync queue
// ---------------------------------------------------------------------------

function NewButton({ realm, docType }: { realm: string; docType: DocType }) {
  const writes = useContext(Writes);
  if (!writes.enabled) {
    return (
      <span className="button disabled" title="Writes are off for this company. They are turned on after the read-only period.">
        New {DOC_LABELS[docType].toLowerCase()}
      </span>
    );
  }
  return (
    <a className="button primary" href={href(realm, { view: "new", docType })}>
      New {DOC_LABELS[docType].toLowerCase()}
    </a>
  );
}

const emptyLine = (): DraftLine => ({ itemId: null, description: "", qty: "1", rate: "", classId: null, taxable: false });

const plusDays = (iso: string, days: number) => {
  const d = new Date(iso + "T00:00:00");
  d.setDate(d.getDate() + days);
  return `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
};

function Editor({ realm, docType, qboId, go }: { realm: string; docType?: DocType; qboId?: string; go: (r: Route) => void }) {
  const writes = useContext(Writes);
  const classes = useContext(Classes);
  const [draft, setDraft] = useState<Draft | null>(null);
  const [customers, setCustomers] = useState<Customer[]>([]);
  const [items, setItems] = useState<Item[]>([]);
  const [customerText, setCustomerText] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [adding, setAdding] = useState<{ name: string; email: string } | null>(null);

  const purchase = draft ? VENDOR_SIDE.has(draft.docType) : docType ? VENDOR_SIDE.has(docType) : false;
  // Contacts follow the document's side, known once the draft has loaded.
  useEffect(() => {
    (purchase ? api.vendors(realm) : api.customers(realm)).then(setCustomers, (e: ApiError) => setError(e.message));
  }, [realm, purchase]);
  useEffect(() => {
    api.items(realm).then((all) => setItems(all.filter((i) => i.active)), () => setItems([]));
    if (qboId) {
      api.documentDraft(realm, qboId).then(
        (r) => {
          setDraft({ ...r.draft, lines: r.draft.lines.length ? r.draft.lines : [emptyLine()] });
          setCustomerText(r.contactName ?? "");
        },
        (e: ApiError) => setError(e.message),
      );
    } else if (docType) {
      const today = todayIso();
      setDraft({
        docType,
        contactId: "",
        txnDate: today,
        dueDate: docType === "Invoice" || docType === "Bill" ? plusDays(today, 30) : null,
        docNumber: null,
        poNumber: null,
        privateNote: null,
        customerMemo: null,
        classId: null,
        lines: [emptyLine()],
      });
    }
  }, [realm, docType, qboId]);

  if (!writes.enabled) {
    return <div className="pad">Writes are off for this company. They are turned on with <code>qbo-local writes --enable</code> after the read-only period.</div>;
  }
  if (!draft) return error ? <div className="error pad">{error}</div> : <div className="muted pad">Loading…</div>;

  const set = (patch: Partial<Draft>) => setDraft({ ...draft, ...patch });
  const setLine = (index: number, patch: Partial<DraftLine>) =>
    set({ lines: draft.lines.map((l, i) => (i === index ? { ...l, ...patch } : l)) });
  const amounts = draft.lines.map((l) => lineAmountMinor(l.qty, l.rate));
  const subtotal = amounts.reduce<number>((sum, a) => sum + (a ?? 0), 0);
  const label = DOC_LABELS[draft.docType];

  const chooseCustomer = (text: string) => {
    setCustomerText(text);
    const match = customers.find((c) => c.displayName === text);
    set({ contactId: match ? match.qboId : "" });
  };

  const chooseItem = (index: number, name: string) => {
    const item = items.find((i) => i.name === name);
    if (!item) {
      setLine(index, { itemId: null, description: name });
      return;
    }
    const line = draft.lines[index];
    setLine(index, {
      itemId: item.qboId,
      description: line.description && line.itemId === null ? line.description : item.name,
      rate: line.rate || formatDecimal(purchase ? item.purchaseCost : item.unitPrice).replace(/,/g, ""),
    });
  };

  const addCustomer = async () => {
    if (!adding) return;
    setError(null);
    try {
      const created = await api.createCustomer(realm, adding.name, undefined, adding.email || undefined);
      const customer: Customer = { qboId: created.entityId, displayName: created.displayName, companyName: null, pending: true };
      setCustomers([...customers, customer]);
      setCustomerText(created.displayName);
      set({ contactId: created.entityId });
      setAdding(null);
      writes.refresh();
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const save = async () => {
    setError(null);
    // The list may have arrived after the name was typed; resolve it now.
    const contactId = draft.contactId || customers.find((c) => c.displayName === customerText)?.qboId || "";
    const toSave = { ...draft, contactId };
    if (!contactId) return setError(purchase ? "Choose a vendor from the list." : "Choose a customer from the list, or add a new one.");
    if (purchase && draft.lines.some((l) => !l.itemId)) return setError("Every purchase line needs a product from the list.");
    if (amounts.some((a) => a === null)) return setError("Every line needs a number for quantity and rate.");
    setSaving(true);
    try {
      const result = await api.saveDocument(realm, toSave, qboId);
      writes.refresh();
      go({ view: "doc", qboId: qboId ?? result.entityId });
    } catch (e) {
      setError((e as Error).message);
      setSaving(false);
    }
  };

  const onKey = (e: React.KeyboardEvent) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "s") {
      e.preventDefault();
      void save();
    }
  };

  const classNames = Object.entries(classes);

  return (
    <section className="document editor" onKeyDown={onKey}>
      <div className="toolbar">
        <a className="back" href={qboId ? href(realm, { view: "doc", qboId }) : href(realm, { view: "list", docType: draft.docType })}>
          ← {qboId ? "Back" : DOC_PLURALS[draft.docType]}
        </a>
        <h1>{qboId ? `Edit ${label.toLowerCase()} ${draft.docNumber ?? ""}` : `New ${label.toLowerCase()}`}</h1>
        <span className="spacer" />
        {writes.environment === "sandbox" && <span className="pill">Sandbox</span>}
        <button className="button primary" onClick={() => void save()} disabled={saving}>
          {saving ? "Saving…" : "Save"} <kbd>{navigator.platform.includes("Mac") ? "⌘S" : "Ctrl S"}</kbd>
        </button>
      </div>
      {error && <div className="error">{error}</div>}

      <div className="form-grid">
        <label className="field wide">
          <span>{purchase ? "Vendor" : "Customer"}</span>
          <input list="bb-customers" value={customerText} onChange={(e) => chooseCustomer(e.target.value)} placeholder="Start typing a name" autoFocus={!qboId} />
          <datalist id="bb-customers">
            {customers.map((c) => (
              <option key={c.qboId} value={c.displayName}>{c.pending ? "new, not yet in QuickBooks" : c.companyName ?? ""}</option>
            ))}
          </datalist>
          {!purchase && !draft.contactId && customerText && !adding && (
            <button className="link" onClick={() => setAdding({ name: customerText, email: "" })}>Add “{customerText}” as a new customer</button>
          )}
        </label>
        <label className="field">
          <span>Date</span>
          <input type="date" value={draft.txnDate} onChange={(e) => set({ txnDate: e.target.value })} />
        </label>
        {(draft.docType === "Invoice" || draft.docType === "Bill") && (
          <label className="field">
            <span>Due</span>
            <input type="date" value={draft.dueDate ?? ""} onChange={(e) => set({ dueDate: e.target.value || null })} />
          </label>
        )}
        <label className="field">
          <span>No.</span>
          <input value={draft.docNumber ?? ""} onChange={(e) => set({ docNumber: e.target.value || null })} placeholder="QuickBooks assigns" />
        </label>
        {!purchase && (
          <label className="field">
            <span>Customer PO</span>
            <input value={draft.poNumber ?? ""} onChange={(e) => set({ poNumber: e.target.value || null })} />
          </label>
        )}
        {classNames.length > 0 && (
          <label className="field">
            <span>Class</span>
            <select value={draft.classId ?? ""} onChange={(e) => set({ classId: e.target.value || null })}>
              <option value="">None</option>
              {classNames.map(([id, name]) => (
                <option key={id} value={id}>{name}</option>
              ))}
            </select>
          </label>
        )}
      </div>

      {adding && (
        <div className="inline-form">
          <strong>New customer</strong>
          <input value={adding.name} onChange={(e) => setAdding({ ...adding, name: e.target.value })} aria-label="Customer name" />
          <input value={adding.email} onChange={(e) => setAdding({ ...adding, email: e.target.value })} placeholder="Billing email (optional)" aria-label="Billing email" />
          <button className="button primary" onClick={() => void addCustomer()}>Add customer</button>
          <button className="button" onClick={() => setAdding(null)}>Cancel</button>
          <span className="muted">Usable on this {label.toLowerCase()} now; sent to QuickBooks first.</span>
        </div>
      )}

      <datalist id="bb-items">
        {items.map((i) => (
          <option key={i.qboId} value={i.name}>{i.sku ?? ""}</option>
        ))}
      </datalist>
      <div className="grid-wrap">
        <table className="grid lines edit-lines">
          <thead>
            <tr>
              <th>#</th>
              <th>Product or service</th>
              <th>Description</th>
              <th className="r">Qty</th>
              <th className="r">Rate</th>
              <th className="r">Amount</th>
              {!purchase && <th>Tax</th>}
              <th />
            </tr>
          </thead>
          <tbody>
            {draft.lines.map((line, i) => {
              const itemName = items.find((it) => it.qboId === line.itemId)?.name ?? "";
              return (
                <tr key={i}>
                  <td className="num muted">{i + 1}</td>
                  <td><input list="bb-items" defaultValue={itemName} onBlur={(e) => chooseItem(i, e.target.value)} aria-label={`Line ${i + 1} product`} /></td>
                  <td><input value={line.description ?? ""} onChange={(e) => setLine(i, { description: e.target.value })} aria-label={`Line ${i + 1} description`} /></td>
                  <td className="r"><input className="num r" inputMode="decimal" value={line.qty} onChange={(e) => setLine(i, { qty: e.target.value })} aria-label={`Line ${i + 1} quantity`} /></td>
                  <td className="r"><input className="num r" inputMode="decimal" value={line.rate} onChange={(e) => setLine(i, { rate: e.target.value })} aria-label={`Line ${i + 1} rate`} /></td>
                  <td className={amounts[i] === null && line.rate ? "num r crit-text" : "num r"}>{amounts[i] === null ? (line.rate ? "?" : "") : formatMoney(amounts[i])}</td>
                  {!purchase && <td><input type="checkbox" checked={line.taxable} onChange={(e) => setLine(i, { taxable: e.target.checked })} aria-label={`Line ${i + 1} taxable`} /></td>}
                  <td>
                    {draft.lines.length > 1 && (
                      <button className="link" onClick={() => set({ lines: draft.lines.filter((_, j) => j !== i) })} aria-label={`Remove line ${i + 1}`}>Remove</button>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
          <tfoot>
            <tr>
              <td colSpan={2}>
                <button className="link" onClick={() => set({ lines: [...draft.lines, emptyLine()] })}>Add a line</button>
              </td>
              <td colSpan={3} className="r">{purchase ? "Total" : "Subtotal · tax is added by QuickBooks"}</td>
              <td className="num r">{formatMoney(subtotal)}</td>
              <td colSpan={purchase ? 1 : 2} />
            </tr>
          </tfoot>
        </table>
      </div>

      <div className="form-grid">
        <label className="field wide">
          <span>{purchase ? "Message to vendor" : `Message on ${label.toLowerCase()}`}</span>
          <textarea rows={2} value={draft.customerMemo ?? ""} onChange={(e) => set({ customerMemo: e.target.value || null })} />
        </label>
        <label className="field wide">
          <span>Internal note</span>
          <textarea rows={2} value={draft.privateNote ?? ""} onChange={(e) => set({ privateNote: e.target.value || null })} />
        </label>
      </div>
    </section>
  );
}

function WritesView({ realm }: { realm: string }) {
  const writes = useContext(Writes);
  const [rows, setRows] = useState<Write[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  useEffect(() => {
    api.outbox(realm).then(setRows, (e: ApiError) => setError(e.message));
  }, [realm, tick]);
  useEffect(() => {
    const timer = window.setTimeout(() => setTick((t) => t + 1), 3_000);
    return () => window.clearTimeout(timer);
  }, [tick]);

  const act = async (id: string, action: "retry" | "discard") => {
    setError(null);
    try {
      await api.resolveWrite(realm, id, action);
      writes.refresh();
      setTick((t) => t + 1);
    } catch (e) {
      setError((e as Error).message);
    }
  };

  const failed = (rows ?? []).filter((r) => r.state === "conflicted" || r.state === "rejected");
  const queued = (rows ?? []).filter((r) => r.state === "pending" || r.state === "in_flight");

  return (
    <section className="document">
      <div className="toolbar">
        <h1>Sync queue</h1>
        <span className="spacer" />
        <span className="muted">Writes saved here and not yet confirmed by QuickBooks. Nothing is dropped or merged without you.</span>
      </div>
      {error && <div className="error">{error}</div>}
      {!rows && !error && <div className="muted pad">Loading…</div>}
      {rows && rows.length === 0 && <p className="muted indent">Nothing waiting. Every saved change is in QuickBooks.</p>}
      {failed.length > 0 && (
        <>
          <h2>Needs you ({failed.length})</h2>
          <div className="grid-wrap">
            <table className="grid">
              <thead>
                <tr><th>Change</th><th>What happened</th><th>Saved</th><th /></tr>
              </thead>
              <tbody>
                {failed.map((w) => (
                  <tr key={w.id}>
                    <td>
                      <WriteLink realm={realm} write={w} />
                      <div className="muted">{w.operation === "create" ? "New" : "Edit"} · {w.entityType}</div>
                    </td>
                    <td className="wrap">
                      <span className="badge crit">{w.status}</span>
                      <div className="muted">{w.state === "conflicted" ? "Someone changed it in QuickBooks after this edit was made. Retry applies your edit on top of their version once the replica has it." : w.lastError}</div>
                    </td>
                    <td className="num muted">{new Date(w.createdAt).toLocaleString()}</td>
                    <td className="actions">
                      <button className="button" onClick={() => void act(w.id, "retry")}>Retry</button>
                      <button className="button" onClick={() => void act(w.id, "discard")}>Discard</button>
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
      {queued.length > 0 && (
        <>
          <h2>On the way ({queued.length})</h2>
          <div className="grid-wrap">
            <table className="grid">
              <thead>
                <tr><th>Change</th><th>Status</th><th className="r">Tries</th><th>Saved</th></tr>
              </thead>
              <tbody>
                {queued.map((w) => (
                  <tr key={w.id}>
                    <td>
                      <WriteLink realm={realm} write={w} />
                      <div className="muted">{w.operation === "create" ? "New" : "Edit"} · {w.entityType}</div>
                    </td>
                    <td>
                      <span className="badge warn">{w.blockedBy ? `Waiting for ${w.blockedBy}` : w.status}</span>
                      {w.lastError && <div className="muted">Last try: {w.lastError}</div>}
                    </td>
                    <td className="num r muted">{w.attempts}</td>
                    <td className="num muted">{new Date(w.createdAt).toLocaleString()}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        </>
      )}
    </section>
  );
}

function WriteLink({ realm, write }: { realm: string; write: Write }) {
  const isDocument = !["Customer", "Vendor", "Item"].includes(write.entityType);
  if (isDocument) return <a href={href(realm, { view: "doc", qboId: write.entityId })}>{write.label}</a>;
  if (write.entityType === "Customer" && !write.entityId.startsWith("local:"))
    return <a href={href(realm, { view: "contact", contactType: "Customer", contactId: write.entityId })}>{write.label}</a>;
  return <span>{write.label}</span>;
}
