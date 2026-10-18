import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { api, ApiError, ContactType, DocType, Document, DocumentDetail, Hit, Realm, SearchResult, Status } from "./api";
import { createContext, useContext } from "react";
import { ago, DOC_LABELS, DOC_PLURALS, formatDate, formatDecimal, formatMoney } from "./format";

// ---------------------------------------------------------------------------
// Routing: the hash is the whole navigation state, so back/forward and a
// reload land exactly where you were.
// ---------------------------------------------------------------------------

type Route =
  | { view: "list"; docType: DocType }
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
    }, 15_000);
    return () => {
      live = false;
      window.clearInterval(timer);
    };
  }, [realm]);

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

  return (
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
      </nav>

      <main className="main">
        {route.view === "list" && <Register key={`${realm}:${route.docType}`} realm={realm} docType={route.docType} go={go} />}
        {route.view === "doc" && <DocumentView key={`${realm}:${route.qboId}`} realm={realm} qboId={route.qboId} go={go} />}
        {route.view === "contact" && (
          <ContactView key={`${realm}:${route.contactId}`} realm={realm} contactType={route.contactType} contactId={route.contactId} go={go} />
        )}
      </main>

      <StatusBar status={status} now={now} />

      {paletteOpen && <Palette realm={realm} go={go} close={() => setPaletteOpen(false)} />}
    </div>
    </Classes.Provider>
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

function StatusBar({ status, now }: { status: Status | null; now: number }) {
  if (!status) return <footer className="statusbar">Status unavailable</footer>;
  const total = status.entityTypes.reduce((sum, t) => sum + t.count, 0);
  return (
    <footer className="statusbar" role="status">
      <span title={status.lastSynced ?? "never"}>Synced {ago(status.lastSynced, now)}</span>
      <span className={status.outboxPending > 0 ? "pill warn" : "pill"}>{status.outboxPending} queued</span>
      <span className={status.outboxNeedsAttention > 0 ? "pill crit" : "pill"}>{status.outboxNeedsAttention} need attention</span>
      {status.quarantined > 0 && <span className="pill warn">{status.quarantined} quarantined</span>}
      <span className="spacer" />
      <span className="num">{total.toLocaleString()} records on disk</span>
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

  useEffect(() => {
    api.document(realm, qboId).then(setDetail, (e: ApiError) => setError(e.message));
  }, [realm, qboId]);

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
        <span className="muted">Read-only replica · edit in QuickBooks</span>
      </div>

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
  useEffect(() => {
    api.contactDocuments(realm, contactType, contactId).then(setRows, (e: ApiError) => setError(e.message));
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
