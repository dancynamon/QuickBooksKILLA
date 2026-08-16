# Claude Code Kickoff Prompt — Project 2: `qbo-local` (fast local front-end for QBO)

Paste everything below the line into a fresh Claude Code session in an empty repo directory.
This is the **near-term** project. It ships in weeks, not months, and it shares the
`ledger-core` money/domain crate with the long-term replacement project.

---

TIER: Opus. Decided upstream — do not re-classify.

# PROJECT: `qbo-local` — a local mirror and fast UI for QuickBooks Online

You are the lead engineer. I am Dan Cynamon: 40+ years manufacturing,
C/C++/Linux background. I read code and I will review your architecture. No
preamble, no filler, no explaining what a tool does.

## 1. The problem, precisely

I run Aquamentor and WaterLine CNC on QuickBooks Online. QBO's web app is
network-bound on every interaction: opening a customer, adding an invoice line,
tabbing between fields. On a 12-line invoice this costs me minutes per document
and dozens of minutes per day. The data is small. The latency is the product.

**QBO remains the book of record.** This project does not replace it. This
project makes it fast by putting a complete local replica of my QBO data on my
own disk and never letting the network sit in the interaction path.

## 2. The shape of the solution

```
  [ QBO cloud ]
       ^  |
  push |  | pull (CDC poll)
       |  v
  [ sync engine ]  <-- the ONLY component that touches the network
       ^  |
       |  v
  [ SQLite replica on my disk ]
       ^  |
       |  v
  [ Tauri + React UI ]  <-- never makes a network call, ever
```

- **Reads:** 100% local SQLite. Sub-16ms. Works with the wifi off.
- **Writes:** written to a local **outbox** and acknowledged to the UI
  immediately (optimistic). A background worker pushes them to QBO and
  reconciles. The UI never waits on Intuit.
- **Sync:** background poll of QBO's Change Data Capture endpoint, plus a full
  reconciliation sweep on a schedule.

## 3. Stack (decided — do not re-litigate)

Tauri v2 · Rust · SQLite (`rusqlite`, bundled, WAL) · TypeScript + React + Vite.
macOS Apple Silicon primary, must build on Linux. No Electron, no server, no
Docker, no hosted anything.

**Share code with Project 1.** I am separately building `ledger`, a full
double-entry replacement. Put the money type, the domain enums (account types,
document types, tax treatment), and the QBO entity models in a crate that both
repos depend on via a path/git dependency. Do not duplicate money math.

## 4. QBO API facts to design around (verify each against current docs before coding)

- **Auth:** OAuth 2.0. Access token ~1 hour, refresh token ~100 days and rotates
  on use. **Persisting the rotated refresh token durably and atomically is the
  single most likely cause of a 3am breakage.** Write it to disk with an
  fsync'd atomic rename, keep the previous two, and log every rotation.
- **Rate limits (verify):** ~500 requests/minute per realm; ~40 batch
  requests/minute; max ~10 concurrent requests per company per app; HTTP 429 on
  breach. Some report endpoints are lower (~200/min). Build a token-bucket
  limiter with exponential backoff and jitter, and make the budget a config
  value, not a constant buried in code.
- **Batch endpoint:** up to 30 operations per batch request. Use it.
- **Query pagination:** `STARTPOSITION` / `MAXRESULTS`, max 1000 rows per page.
- **Change Data Capture:** `GET /v3/company/{realmId}/cdc?entities=...&changedSince=...`
  returns entities changed since a timestamp, **including deletes**, with a
  limited lookback window (verify the current window — historically ~30 days).
  This is the core of incremental sync. Confirm exactly which entity types CDC
  supports and design a fallback full-sweep for any that it does not.
- **Webhooks** require a public HTTPS endpoint. I am not standing up a server.
  **Poll CDC instead** — at 500 req/min my entire budget is spent by a poll every
  few seconds. Default to a 15-second CDC poll while the app is focused, 5
  minutes when idle. Make it configurable.
- **Two realms.** Aquamentor and WaterLine CNC are separate companies with
  separate realm IDs, separate tokens, separate rate-limit budgets, and hard
  data isolation in the replica. **They share nothing** — separate customer,
  vendor and item masters, separate chart of accounts. There is no cross-realm
  join anywhere. Every query is realm-scoped; make an unscoped query on a
  realm-scoped table impossible, not merely discouraged.
- **Sandbox first.** Build and test against an Intuit sandbox company. Do not
  point write paths at my production realm until I explicitly say so.

## 5. Local replica design

Mirror these entities, each with `qbo_id`, `sync_token`, `last_updated_utc`,
`raw_json`, and a local `dirty` / `deleted` flag:

Account, Customer, Vendor, Item, Estimate, SalesReceipt, Invoice, Payment,
CreditMemo, RefundReceipt, PurchaseOrder, Bill, BillPayment, VendorCredit,
Purchase, Deposit, JournalEntry, TaxCode, TaxRate, Term, Class, Department,
Attachable, CompanyInfo, Preferences.

Rules:
- **Store the full raw JSON** of every entity alongside the parsed columns. When
  Intuit changes a field, I lose nothing and can re-parse without re-syncing.
- Parsed columns exist for anything I search, sort, or filter on — that is what
  makes it fast. Full-text search via SQLite FTS5 across customers, vendors,
  items, and document memos/line descriptions.
- **`SyncToken` is QBO's optimistic-concurrency version.** Every update must send
  the current one. A stale token is the canonical conflict signal. Handle it,
  do not retry blindly.
- Track `last_cdc_cursor` per realm per entity type, persisted.
- **Class is a first-class column, not just raw JSON.** I track per-product-line
  P&L (foam, signs, chairs, CNC job work). Mirror `Class` as a real table, parse
  `ClassRef` out to a nullable `class_id` on every document header and document
  line, expose it in grids and filters, and set it on every write. This replica
  is also the source for Project 1's history import, so a class I fail to capture
  here is a class I lose there.

## 6. The outbox — this is where the project succeeds or fails

Every local mutation becomes a durable outbox record:

```
outbox(id UUIDv7, realm_id, entity_type, operation, payload_json,
       local_entity_id, base_sync_token, created_at, attempts,
       state, last_error, qbo_response_json)

state: pending -> in_flight -> applied
                            \-> conflicted
                            \-> rejected
```

Requirements:
1. **Ordered per entity, parallel across entities.** An invoice update must not
   overtake its own create.
2. **Idempotent.** Use QBO's `RequestId` on writes so a retry after a timeout
   cannot create a duplicate invoice. Never let a network timeout become a
   double-billed customer.
3. **Dependency resolution.** If I create a customer and immediately invoice
   them, the invoice's payload references a local id. The worker must rewrite
   local ids to QBO ids after the parent is applied.
4. **A visible failure inbox.** Rejected and conflicted writes appear in the UI
   as a queue I must resolve. Never silently drop, never silently auto-merge.
   The UI must always show me: pending count, last successful sync, and any
   unresolved failures. If sync has been broken for an hour I want to know
   without asking.
5. **Optimistic UI, honest state.** A locally-created invoice is visibly marked
   "not yet in QBO" until confirmed.

## 7. Reconciliation

CDC drift is real. Build a **verification sweep** that runs nightly and on demand:
- Full list of each entity type from QBO (ids + `MetaData.LastUpdatedTime` only)
- Diff against the replica: missing, extra, stale, orphaned
- Report the delta. Auto-heal pulls. Never auto-delete local data — quarantine it.
- Also diff a QBO Trial Balance against a trial balance computed from the replica.
  A non-zero variance is a loud, unmissable error.

## 8. UI — this is the actual product, do not treat it as a skin

Optimize for my hands, not for discoverability:
- **Command palette** (Cmd-K): jump to any customer, invoice, item, PO by fuzzy
  match, instantly.
- **Keyboard-first document entry.** Tab through an invoice line, type a SKU
  prefix, autocomplete resolves locally with zero latency, Enter adds the line.
  I should be able to enter a 12-line invoice without touching the mouse.
- **Dense grids.** TanStack Table. No cards, no whitespace-heavy dashboards. I
  want QuickBooks Desktop's information density with 2026 rendering.
- Deep-link out to the QBO web record from every row (I still need the browser
  for things this app will not do).
- Show sync state in the chrome at all times: last sync, pending writes, errors.

**v1 workflows** (the ones that actually cost me time today):
1. Create/edit an invoice
2. Create/edit a purchase order
3. Create/edit an estimate and convert to invoice or sales order
4. Look up any customer/vendor/item and see full transaction history
5. Receive a customer payment against invoices
6. Enter a vendor bill
7. Search everything

**Out of scope for v1:** bank feeds, reconciliation UI, payroll, reports beyond
AR/AP aging and open POs/SOs, attachments, recurring transactions, inventory
adjustments.

## 9. Performance targets — benchmark them, do not assert them

- Cold start to usable: < 1000 ms
- Any lookup or search keystroke: < 16 ms
- Adding an invoice line: < 16 ms
- Saving a document (local commit, before any network): < 50 ms
- Full initial sync of both realms: report the number, target < 10 min

Build a synthetic realm generator so you can benchmark against 50k transactions
without hammering Intuit.

## 10. Safety — this touches my real books

- **Read-only mode is the default** until I flip a config flag per realm. Ship
  read-only first and I will live on it for **two full weeks** before any write
  path is enabled against a production realm. Writes are where duplicate invoices
  come from, and M1 already removes most of my daily QBO time on its own. Do not
  compress this window; do not ask me to shorten it.
- Every write to QBO is logged to an append-only JSONL audit file: timestamp,
  request, RequestId, response, resulting QBO id.
- Nightly snapshot of the replica DB, dated, N-deep rotation.
- **No delete/void path in v1.** I will do destructive operations in QBO's web
  UI where the confirmation dialogs live.
- Secrets (client id/secret, tokens) in the OS keychain, never in the repo,
  never in a dotfile that could sync to Dropbox.

## 11. Testing

- Record/replay HTTP fixtures for the QBO API (`wiremock` or equivalent). The
  test suite must run fully offline.
- Property test: any sequence of local mutations, when drained through the
  outbox against a mock QBO, converges to a replica identical to a fresh pull.
- Chaos test: kill the process mid-flight on a write, restart, assert exactly-once.
- Token rotation test: expire the access token mid-batch, assert clean recovery.

## 12. Milestones — stop at each for my review

**M0 — Auth + read-only sync.** OAuth flow, token persistence and rotation,
rate limiter, full initial pull of both realms into SQLite, CDC incremental
poll. No UI beyond a sync status window and a `sqlite3` prompt.
*Acceptance:* both realms fully mirrored, CDC keeps them current, token survives
a restart and a 24-hour idle.

**M1 — Read-only UI.** Command palette, search, customer/vendor/item pages with
full transaction history, invoice/PO/estimate viewers, AR/AP aging, open POs/SOs.
*Acceptance:* I stop opening QBO in the browser to *look things up*. Every
§9 read target met and benchmarked.

**M2 — The outbox, against sandbox only.** Full write pipeline, RequestId
idempotency, dependency resolution, conflict handling, failure inbox.
*Acceptance:* chaos test passes, convergence property test passes, zero
duplicates under induced network failure.

**M3 — Write UI, production behind a flag.** Invoice, PO, estimate, bill,
customer payment entry. Keyboard-first. Class set on every line. Production
writes stay off for a minimum of two weeks of read-only daily use, then only on
my explicit go, and then one realm at a time.

**M4 — Reconciliation + hardening.** Nightly verification sweep, TB diff,
backup/restore drill, audit log review tooling.

## 13. How I want you to work

- **First output is `DESIGN.md` and nothing else.** Cover: the entity mirror
  schema, the outbox state machine (draw the transitions), the dependency-
  resolution algorithm, the CDC cursor strategy and its failure modes, the token
  rotation design, and exactly what happens when a write is rejected. Do not
  scaffold the repo until I approve it.
- Verify every QBO API claim in §4 against current Intuit docs and correct me in
  `DESIGN.md` where I am wrong. Cite the doc URL for each.
- Ask when a decision is mine. Do not guess at accounting behavior.
- One concern per commit. Conventional commits. State the alternative you
  rejected in one line whenever you make a trade-off.

## 13a. Decisions already made — do not re-open these

- Realms share **nothing**; separate masters, no cross-realm joins.
- **Class is mirrored as a real dimension**, parsed onto headers and lines.
- **Two weeks minimum read-only** on production before any write is enabled.
- This replica is designed to also serve as the **full-history source** for
  Project 1's import, so mirror complete history, not a recent window. Size the
  initial sync accordingly and report actual row counts and wall-clock time.
- v1 reports (AR/AP aging, open POs/SOs) are **accrual only**. Cash-basis
  reporting is Project 1's problem, not this one's.

## 14. Questions to answer in DESIGN.md, with a recommendation not just a question

1. Does CDC cover every entity in §5, and what is the current lookback window?
   What is the fallback for uncovered entities?
2. What happens if I am offline for longer than the CDC window? (Design the
   full-resync path and make it not take an hour.)
3. `SyncToken` conflict: last-writer-wins, or always surface to me? Argue one.
4. Is there any QBO write that cannot be safely made idempotent with RequestId?
   If so, name it and gate it.
5. Should the shared crate with Project 1 be a git dependency or a monorepo?
   Argue one and commit to it.
