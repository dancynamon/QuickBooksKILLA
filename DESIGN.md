# DESIGN.md — `qbo-local`

A local replica of QuickBooks Online plus a fast UI over it. QBO remains the
book of record. This project removes the network from the interaction path: every
read is a local SQLite query, every write commits locally and syncs in the
background.

The double-entry accounting system is a separate, later project. It gets its own
`LEDGER-DESIGN.md` when it starts. See §9 for what this design deliberately does
not foreclose.

Decisions taken during review are logged in `DECISIONS.md`.

---

## 0. Verified API facts, and where the kickoff prompt was wrong

The kickoff prompt asked for every API claim to be verified against current Intuit
documentation and cited. **`developer.intuit.com` is blocked by this environment's
network egress policy**, so the primary source was unreachable. What follows was
verified against secondary sources and is marked with a confidence level. Anything
below marked ⚠️ must be confirmed against Intuit's own docs before the write path
(M2) is built.

| Claim in prompt §4 | Finding | Confidence |
|---|---|---|
| ~500 req/min per realm | Confirmed: 500/min per realm ID | High — consistent across sources |
| ~10 concurrent per company | Confirmed | High |
| ~200/min on some report endpoints | Confirmed: resource-intensive endpoints drop to 200/min | Medium |
| **~40 batch req/min** | **Wrong — now 120/min per realm**, changed 31 Oct 2025 | Medium ⚠️ |
| 30 operations per batch | Confirmed | High |
| `STARTPOSITION`/`MAXRESULTS`, 1000 rows max | Confirmed | High |
| CDC lookback ~30 days | Confirmed: up to 30 days | High |
| CDC includes deletes | Confirmed | High |
| CDC covers which entities? | "All entities except…" — **exclusion list not obtained** | Unknown ⚠️ |
| `RequestId` gives idempotency | Confirmed generally — **but reportedly does NOT apply to Customer or Item** | Medium ⚠️ |

Two of these change the design materially and are handled below: the batch limit
(§4.4) and the `RequestId` gap on Customer/Item (§6.5).

Also newly learned and load-bearing: **CDC returns at most 1000 objects per
response, and returns full entity payloads rather than field-level deltas.** The
1000 cap means a CDC response at the limit must be treated as "possibly truncated"
and re-polled over a narrower window, not accepted as complete (§4.3).

Sources: [Intuit CDC blog](https://blogs.intuit.com/2023/08/24/building-smarter-with-intuit-stay-in-sync-with-cdc/) ·
[Intuit API optimization](https://blogs.intuit.com/2025/08/11/best-practices-for-intuit-api-optimization-part-1/) ·
[Intuit RequestId](https://blogs.intuit.com/2015/04/06/15346/) ·
[Intuit best practices](https://help.developer.intuit.com/s/article/QuickBooks-Online-API-Best-Practices) ·
[Satva rate limits](https://satvasolutions.com/blog/quickbooks-online-api-limitations-guide) ·
[Truto integration guide](https://truto.one/blog/how-to-integrate-with-the-quickbooks-online-api-2026-guide/)

### Measured scale (Aquamentor, live, 16 Aug 2026)

| | |
|---|---|
| Oldest invoice | 22 Mar 2012 (#9665) |
| Newest invoice | 15 Aug 2026 (#21234) |
| Invoices | ~9,200 over 14.5 years |
| Highest internal txn id | 94,450 (all transaction types share one sequence) |

Total documents across all types is in the tens of thousands. At a few KB of raw
JSON each this is a few hundred MB for complete history — small enough that
mirroring everything is the cheap option, which is why §3.1 does.

WaterLine CNC was not measured (separate realm, not connected to this session).
Expected smaller and younger; confirm before sizing the initial sync.

---

## 1. Money and rounding

Shared with the future ledger project via `crates/ledger-core`. That crate holds
**money and rounding only** — see `DECISIONS.md` D3. QBO entity models live in
this app, not in the shared crate, because they are Intuit's data shapes and do
not belong in the core of an eventual QuickBooks replacement.

```rust
/// Signed integer amount in minor units (cents). Never a float, anywhere.
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Money(i64);

impl Money {
    pub const ZERO: Money = Money(0);

    /// Construct from stored minor units. This is the DB-hydration path.
    pub const fn from_minor(minor: i64) -> Self { Money(minor) }
    pub const fn minor(self) -> i64 { self.0 }

    pub fn checked_add(self, rhs: Money) -> Result<Money, MoneyError>;
    pub fn checked_sub(self, rhs: Money) -> Result<Money, MoneyError>;
    pub fn checked_neg(self) -> Result<Money, MoneyError>;
    pub fn checked_sum<I: IntoIterator<Item = Money>>(iter: I) -> Result<Money, MoneyError>;
    pub fn is_zero(self) -> bool { self.0 == 0 }
}
```

- `i64` minor units covers ±92 quadrillion dollars at cent granularity. No
  overflow risk at this business's scale, but arithmetic is checked anyway and
  returns `Result` — overflow is an error to handle, never a panic and never a
  silent wrap. *Rejected: unchecked arithmetic with a debug assertion — a
  release-mode wrap in money code is exactly the silent balance corruption the
  brief calls catastrophic.*
- No currency tag on `Money`. Both realms are USD and multi-currency is out of
  scope. *Rejected: a `Currency` phantom parameter — it would touch every
  signature to buy nothing until a scope change that is explicitly ruled out.*

### Rounding

```rust
pub enum RoundingPolicy {
    /// qty × rate → Money. Half-up, away from zero.
    LineExtension,
    /// taxable subtotal × rate → Money. Banker's rounding (half-to-even).
    TaxCalculation,
}

pub fn round_money(amount: Decimal, policy: RoundingPolicy)
    -> Result<Money, MoneyError>;
```

`round_money` is **the only function in `ledger-core` that accepts a `Decimal`**.
That is what makes it the sole conversion point — not a naming convention. It
returns `Result`; a value too large for `i64` cents is an error, not an
`unwrap()` panic.

`rust_decimal::Decimal` carries unit rates, quantities, tax percentages and
board-foot costs. It converts to `Money` exactly once, when a line is extended.

**Open risk, flagged deliberately.** The policy above is as specified in the
kickoff prompt. The earlier draft justified banker's-rounding-on-tax as "matching
how QBO itself rounds" — that claim was asserted, not verified, and I could not
verify it. It matters: if QBO rounds tax half-up, this produces systematic
one-cent divergence on tax lines. For `qbo-local` the consequence is limited,
because this app mirrors the tax QBO already calculated rather than computing its
own. It becomes load-bearing only when the ledger project computes tax
independently and reconciles against QBO. Recorded here so it is not rediscovered
then.

---

## 2. Domain enums and QBO entity models

These live in `apps/qbo-local`, not the shared crate.

### 2.1 Realm scoping — enforced, not documented

Both kickoff prompts require that an unscoped query on a realm-scoped table be
impossible rather than merely discouraged. The earlier draft had `realm_id:
String` as a bare field and no enforcement, which is neither.

```rust
/// Opaque realm identifier. Cannot be constructed from a bare string outside
/// the config loader, so a query cannot be scoped to a realm that was never
/// configured.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RealmId(String);
```

Enforcement is structural, in three parts:

1. The `rusqlite::Connection` is private to the `store` module. No code outside
   it can issue SQL.
2. Every repository function in `store` takes `realm: &RealmId` as its first
   parameter. There is no accessor that omits it.
3. Every realm-scoped table carries `realm_id NOT NULL` and every index on those
   tables is prefixed by `realm_id`, so a query that forgets the predicate is a
   full scan that shows up immediately in benchmarks rather than silently
   returning another company's rows.

*Rejected: phantom-typed connections parameterised by realm.* It gives
compile-time proof rather than structural discipline, but it infects every
signature in the app with a type parameter for a system that has exactly two
realms and one writer. The private-connection rule gets most of the guarantee at
a fraction of the cost.

### 2.2 Entity type — wider than document type

The earlier draft typed the outbox's entity as `DocumentType`, which cannot
represent Customer, Vendor, Item, Account or Class. That makes the outbox's
own canonical dependency case — create a customer, immediately invoice them —
unrepresentable. Corrected:

```rust
pub enum EntityType {
    // Masters
    Account, Class, Customer, Vendor, Item, TaxCode, TaxRate, Term,
    Department, CompanyInfo, Preferences, Attachable,
    // Documents — every variant here has a DocumentType equivalent
    Estimate, Invoice, SalesReceipt, CreditMemo, RefundReceipt, Payment,
    PurchaseOrder, Bill, BillPayment, VendorCredit, Purchase, Deposit,
    JournalEntry,
}

pub enum DocumentType { /* the document subset, above */ }

impl DocumentType { pub fn as_entity(self) -> EntityType; }
impl EntityType { pub fn as_document(self) -> Option<DocumentType>; }
```

### 2.3 Mirrored entity shape

```rust
pub struct QboMeta {
    pub realm_id: RealmId,
    pub qbo_id: String,
    pub sync_token: String,
    pub last_updated_utc: DateTime<Utc>,
    pub raw_json: serde_json::Value,
}
```

Every mirrored entity is `QboMeta` plus the parsed columns needed for search,
sort, filter and display. **The full raw JSON is always stored.** When Intuit adds
a field, it is already on disk and can be parsed without re-syncing.

`QboDocument` covers Estimate / Invoice / SalesReceipt / CreditMemo /
RefundReceipt / PurchaseOrder / Bill / VendorCredit / JournalEntry uniformly,
with `doc_type` disambiguating. Payment / BillPayment / Deposit have a different
shape — applications against other documents rather than qty×rate lines — and get
`QboPaymentApplication`.

### 2.4 Class

```rust
pub struct ClassId(String);
```

`Option<ClassId>` — the newtype, not a bare `String`; the earlier draft
contradicted itself between §4 and §5 on this — on every document header and
every document line. Parsed out of `ClassRef`, mirrored as a real table, exposed
in grids and filters, and set on every write. This replica is the history source
for the ledger project, so a class not captured here is a class lost there.

---

## 3. Replica schema

SQLite via `rusqlite`, bundled, WAL mode, `foreign_keys=ON`. Forward-only
numbered migrations applied in a transaction, version recorded in the DB, never
edited once shipped.

### 3.1 History depth and entity coverage

**Complete history, every year.** Measured cost is a few hundred MB and a
one-time sync of minutes (§0). The benefit is that any question about your own
past — what a customer bought in 2015, what blue XLPE cost in 2023 — is answered
from local disk at zero latency. A recent-window replica would send you back to
the browser for older records, defeating the purpose.

Entity coverage is staged so a working sync arrives sooner. Depth is never
staged — whatever is mirrored is mirrored in full.

| Tier | Entities | When |
|---|---|---|
| 1 — masters | CompanyInfo, Account, Class, Customer, Vendor, Item, TaxCode, TaxRate, Term | M0 |
| 2 — documents | Estimate, Invoice, SalesReceipt, CreditMemo, RefundReceipt, Payment, PurchaseOrder, Bill, BillPayment, VendorCredit, Purchase, Deposit, JournalEntry | M0 |
| 3 — peripheral | Attachable, Department, Preferences | after M0 |

Tier 3 is a config-list change plus one backfill run, not new machinery.

### 3.2 Core tables

```sql
CREATE TABLE realms (
    realm_id        TEXT PRIMARY KEY,
    display_name    TEXT NOT NULL,
    is_write_enabled INTEGER NOT NULL DEFAULT 0,   -- §8, read-only until flipped
    created_at      TEXT NOT NULL
);

-- One row per mirrored entity, all types, all realms.
CREATE TABLE entities (
    realm_id          TEXT NOT NULL REFERENCES realms(realm_id),
    entity_type       TEXT NOT NULL,
    qbo_id            TEXT NOT NULL,
    sync_token        TEXT NOT NULL,
    last_updated_utc  TEXT NOT NULL,
    is_deleted        INTEGER NOT NULL DEFAULT 0,
    raw_json          TEXT NOT NULL,
    mirrored_at       TEXT NOT NULL,
    PRIMARY KEY (realm_id, entity_type, qbo_id)
) STRICT;

CREATE INDEX idx_entities_updated
    ON entities(realm_id, entity_type, last_updated_utc);
```

`entities` is the durable truth of the mirror. The parsed tables below are a
**projection** of it — derived, rebuildable, and safe to drop and regenerate when
a parser improves. That is what makes "store the raw JSON" more than a slogan: a
parsing bug is fixed by re-projecting from disk, with no Intuit round-trip.

```sql
CREATE TABLE documents (
    realm_id       TEXT NOT NULL,
    qbo_id         TEXT NOT NULL,
    doc_type       TEXT NOT NULL,
    doc_number     TEXT,
    txn_date       TEXT NOT NULL,
    contact_id     TEXT,               -- customer or vendor
    class_id       TEXT,
    total_minor    INTEGER NOT NULL,
    balance_minor  INTEGER,
    is_deleted     INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (realm_id, qbo_id)
) STRICT;

CREATE TABLE document_lines (
    realm_id        TEXT NOT NULL,
    doc_qbo_id      TEXT NOT NULL,
    line_no         INTEGER NOT NULL,
    item_id         TEXT,
    description     TEXT,
    qty             TEXT,              -- Decimal as text; never a float
    unit_price      TEXT,              -- Decimal as text
    amount_minor    INTEGER NOT NULL,
    class_id        TEXT,
    is_taxable      INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (realm_id, doc_qbo_id, line_no),
    FOREIGN KEY (realm_id, doc_qbo_id) REFERENCES documents(realm_id, qbo_id)
) STRICT;
```

Money is `INTEGER` minor units. `Decimal` values are stored as **text**, not
`REAL` — a float column would reintroduce exactly the drift the money type exists
to prevent, and SQLite has no decimal type.

`STRICT` tables throughout: SQLite's default type affinity will silently accept a
string into an integer column, which is not acceptable in a financial store.

Masters (`customers`, `vendors`, `items`, `accounts`, `classes`) follow the same
pattern: realm-scoped composite primary key, parsed columns for search and
display, raw JSON in `entities`.

### 3.3 Search

FTS5 across customers, vendors, items, document memos and line descriptions —
this is what makes Cmd-K instant. External-content FTS tables over the parsed
tables, rebuilt by trigger, so the index cannot drift from its source.

### 3.4 Data outlives the app

The brief requires the SQLite file be meaningful under plain `sqlite3` with no
application code. Consequences honoured above: readable table and column names,
no application-level encoding, no opaque blobs, dates as ISO-8601 text.

---

## 4. Sync: keeping the replica current

### 4.1 Position tracking

```sql
CREATE TABLE sync_cursors (
    realm_id         TEXT NOT NULL,
    entity_type      TEXT NOT NULL,
    last_cdc_cursor  TEXT,             -- ISO-8601, the changedSince for next poll
    last_full_sweep  TEXT,
    PRIMARY KEY (realm_id, entity_type)
) STRICT;
```

Per realm **per entity type**, persisted on every successful poll, in the same
transaction that writes the entities it covers. A cursor advanced outside that
transaction can skip changes after a crash.

### 4.2 Poll cadence

15 seconds while the app is focused, 5 minutes idle, both configurable. At 500
req/min per realm a 15-second poll costs 4 requests/min/realm — under 1% of
budget for two realms.

### 4.3 CDC failure modes

This is where incremental sync actually breaks, so each mode gets an explicit
response.

**Truncation.** CDC returns at most 1000 objects. A response at exactly the cap
must be assumed truncated. Response: halve the window and re-poll, recursively,
until responses come back under the cap; only then advance the cursor. *Rejected:
advancing the cursor to the newest returned record — with an unordered or
partially-returned set, that silently drops everything not returned.*

**Cursor older than the lookback window.** CDC looks back at most 30 days. Past
that, a CDC call does not error — it returns what it can, which is the dangerous
failure: silent incompleteness. Response: before every poll, compare cursor age
against a configured `cdc_max_age` defaulted to **25 days**, five days of margin
under the documented 30. Older than that, skip CDC entirely and run a full sweep
(§7). This is the "laptop closed for a month" case and it must not be a judgement
call at runtime.

**Uncovered entity types.** The exclusion list is unverified (§0). Response:
**build the full-sweep path for every entity from day one and do not depend on
CDC coverage for correctness.** CDC becomes a latency optimisation over a
sweep-based baseline that is always correct. This converts "CDC does not cover
entity X" from a production incident into a slower refresh for that entity.

**Clock skew.** Cursors are Intuit's timestamps, never the local clock. The local
clock is used only for cadence.

### 4.4 Rate limiting

Token-bucket per realm, per bucket class, all budgets from config rather than
constants:

| Bucket | Budget | Note |
|---|---|---|
| General | 500/min/realm | |
| Batch | **120/min/realm** | Corrects prompt's ~40; changed Oct 2025 ⚠️ |
| Reports | 200/min/realm | Resource-intensive endpoints |
| Concurrency | 10 in flight/realm | Semaphore, not a bucket |

429 responses back off exponentially with jitter and are never retried
immediately. Each realm has its own budget — a heavy sweep on Aquamentor must not
throttle WaterLine.

---

## 5. Token rotation

The kickoff prompt calls this the single most likely cause of a 3am breakage. It
is right, and the mechanism is worth stating precisely: the access token lasts
about an hour; refreshing it returns a **new refresh token and invalidates the
old one**. Lose the new one before it reaches disk — power loss, crash, a
half-written file — and the account is locked out until re-authorised by hand.

### 5.1 Persistence

Secrets live in the **OS keychain**, never in the repo and never in a dotfile
that could sync to Dropbox. Write sequence for every rotation:

1. Serialise the new token set.
2. Write to a temporary file in the same directory as the target.
3. `fsync` the temporary file.
4. `rename` over the target — atomic within a filesystem.
5. `fsync` the containing directory, so the rename itself is durable.
6. Append to the rotation log.

Steps 3 and 5 are both required. A rename is atomic but not automatically
durable; without the directory fsync a crash can leave the old file in place with
the new one already invalidated by Intuit.

### 5.2 Generations

The **previous two** token sets are retained. On startup, if the current set
fails, fall back through the generations before declaring lockout. This covers a
crash between Intuit invalidating the old token and the new one reaching disk.

### 5.3 Logging

Every rotation appends to an append-only JSONL log: timestamp, realm, token
fingerprint (never the token), outcome. When a 3am breakage happens this file is
the first thing to read.

### 5.4 Refresh timing

Refresh proactively at ~50 minutes rather than reactively on 401, so rotation
happens while idle rather than mid-batch. Reactive refresh is still implemented
as the fallback, and the test suite forces token expiry mid-batch to prove
recovery.

---

## 6. The outbox

Every local mutation becomes a durable outbox record before the UI is told it
succeeded. This is where the project succeeds or fails.

### 6.1 State machine

```
                    ┌──────────────────────────────────────┐
                    │                                      │
                    v                                      │
  [ pending ] ──> [ in_flight ] ──> [ applied ]            │ retry
       ^                │                                  │ (backoff,
       │                ├──> [ conflicted ] ── resolve ─────┤  bounded)
       │                │         (stale SyncToken)         │
       │                ├──> [ rejected ]   ── resolve ─────┘
       │                │         (QBO refused: validation, permission)
       │                │
       └────────────────┘
          transient failure (network, 429, 5xx)

  applied     — terminal, success
  conflicted  — terminal until I resolve it; never auto-merged
  rejected    — terminal until I resolve it; never silently dropped
```

Only `applied` is terminal without human involvement. `conflicted` and `rejected`
both land in the failure inbox (§6.6).

`in_flight` is persisted before the HTTP request is issued, not after. A process
killed mid-request restarts to find the record in `in_flight` and must resolve it
by querying QBO for the `RequestId` rather than blindly resending — this is the
chaos test in §10.

### 6.2 Schema

```sql
CREATE TABLE outbox (
    id                TEXT PRIMARY KEY,      -- UUIDv7, sorts chronologically
    realm_id          TEXT NOT NULL,
    entity_type       TEXT NOT NULL,         -- EntityType, §2.2
    operation         TEXT NOT NULL,         -- create | update
    payload_json      TEXT NOT NULL,
    local_entity_id   TEXT NOT NULL,
    base_sync_token   TEXT,                  -- NULL on create
    request_id        TEXT NOT NULL,         -- stable across retries
    state             TEXT NOT NULL,
    attempts          INTEGER NOT NULL DEFAULT 0,
    depends_on        TEXT,                  -- outbox id of the parent
    last_error        TEXT,
    qbo_response_json TEXT,
    created_at        TEXT NOT NULL,
    updated_at        TEXT NOT NULL
) STRICT;
```

No delete or void operation in v1 — destructive changes are done in QBO's web UI
where the confirmation dialogs live.

`request_id` is generated once when the record is created and **reused on every
retry**. A `RequestId` regenerated per attempt provides no idempotency at all,
which is the failure mode that turns a network timeout into a double-billed
customer.

### 6.3 Ordering

Ordered per entity, parallel across entities. Records for the same
`(realm_id, entity_type, local_entity_id)` drain strictly in UUIDv7 order, so an
update cannot overtake its own create. Different entities drain concurrently up
to the §4.4 concurrency limit.

### 6.4 Dependency resolution

Creating a customer and immediately invoicing them produces an invoice whose
payload references a local id that does not exist in QBO.

1. On create, the new entity gets a local id (`local:` prefixed UUIDv7) and is
   immediately usable in the UI.
2. Any outbox record whose payload references an unapplied local id records
   `depends_on` = the parent's outbox id.
3. A record with an unsatisfied `depends_on` is not eligible to drain.
4. When the parent reaches `applied`, QBO's assigned id is written to an id-map
   table, dependants rewrite `local:` references to the real id, and become
   eligible.
5. If the parent reaches `conflicted` or `rejected`, dependants stay blocked and
   are shown in the failure inbox as blocked-by, not as independent failures.

### 6.5 Idempotency, and the Customer/Item gap

`RequestId` is the primary defence: a retry after a timeout returns the original
response rather than creating a second invoice.

**But `RequestId` reportedly does not cover Customer or Item** (§0 ⚠️). That is
precisely the dependency path in §6.4, so it needs its own guard. Design
defensively — assume the gap is real, because assuming otherwise risks duplicate
customer records in the book of record:

- Customer and Item creates are gated behind a **query-before-create** check
  inside the same drain step: query QBO by `DisplayName` (Customer) or `Name`
  (Item), and adopt the existing record if found rather than creating.
- QBO independently enforces `DisplayName` uniqueness for customers, so a
  duplicate create fails loudly rather than silently duplicating. That failure is
  caught and converted into an adopt.
- The `in_flight` recovery path (§6.1) always queries before resending for these
  two types, never blind-resends.

*Rejected: treating `RequestId` as sufficient everywhere.* The cost of being
wrong is duplicate masters in production, which is exactly the class of damage
this project must not cause.

### 6.6 Failure inbox

`conflicted` and `rejected` records surface in the UI as a queue to resolve. Never
silently dropped, never auto-merged.

Always visible in the app chrome: last successful sync per realm, pending write
count, unresolved failure count. If sync has been broken for an hour that must be
apparent without asking.

**SyncToken conflicts always surface — never last-writer-wins.** A stale token
means someone changed that record in the QBO web UI. Silently overwriting
discards a real edit that nobody notices until it matters. Surfacing costs a
glance at the inbox. *Rejected: last-writer-wins with an audit entry — the audit
entry is only useful to someone who already knows to look.*

### 6.7 Optimistic UI, honest state

A locally-created document is visibly marked "not yet in QBO" until confirmed.
The UI never lies about what QBO actually holds.

---

## 7. Reconciliation

CDC drift is real. A verification sweep runs nightly and on demand.

1. Pull id + `MetaData.LastUpdatedTime` only, for every entity type, both realms.
2. Diff against the replica: **missing** (in QBO, not local), **extra** (local,
   not in QBO), **stale** (local `last_updated_utc` older than QBO's),
   **orphaned** (line rows with no parent document).
3. Auto-heal by pulling missing and stale records.
4. **Never auto-delete local data** — quarantine into `quarantine_entities` with
   the reason, and report.
5. Diff a QBO Trial Balance against one computed from the replica. Non-zero
   variance is a loud, unmissable error, not a log line.

The sweep is also the recovery path for a stale CDC cursor (§4.3) and for
entities CDC may not cover, which is why it is built in M0 rather than M4.

---

## 8. Safety

- **Read-only by default**, per realm, flipped by explicit config
  (`realms.is_write_enabled`). Ships read-only. Production writes stay off for a
  **minimum of two weeks** of daily read-only use, then only on explicit go, then
  **one realm at a time**. Not compressible.
- **Sandbox first.** Write paths point at an Intuit sandbox company until
  explicitly repointed.
- **No delete or void path in v1.**
- Every write to QBO appends to an append-only JSONL audit file: timestamp,
  realm, request, `RequestId`, response, resulting QBO id.
- Nightly dated snapshot of the replica via SQLite's backup API, N-deep rotation.
- Secrets in the OS keychain only (§5.1).
- The replica DB is **never** placed in Dropbox. Stated here so it does not
  happen by accident later.

Losing the replica is not a business event — it is rebuildable from QBO, which
remains the book of record. Losing the **outbox** is, because it may hold writes
QBO has never seen. The outbox is therefore included in every snapshot and is
never truncated on startup.

---

## 9. The accounting system — direction, not design

Deliberately not designed here. It gets `LEDGER-DESIGN.md` when it starts, and
that document leads with the **posting-rules table** — document type → debit and
credit effects — because that encodes accounting policy that needs review rather
than engineering judgement.

What this project does to keep that door open, at no extra cost:

- **Complete history is mirrored** (§3.1), so the ledger project's import source
  is already on local disk.
- **Class is captured everywhere** (§2.4). A class not captured here is a class
  lost there, and it cannot be backfilled without a re-import.
- **Raw JSON is retained** (§3.2), so fields nobody parses today are still
  available when the ledger needs them.
- **Money and rounding are shared** (§1) so the two systems cannot drift.

What is explicitly **not** built now: double-entry schema, posting rules, the
command/oplog log, cash-basis reporting. v1 reports here are AR/AP aging and open
POs/SOs, **accrual only**.

---

## 10. Testing

- Record/replay HTTP fixtures for the QBO API. **The suite runs fully offline** —
  a requirement, not a nicety, since this environment cannot reach Intuit.
- Property test: any sequence of local mutations, drained through the outbox
  against a mock QBO, converges to a replica identical to a fresh pull.
- Chaos test: kill the process mid-flight on a write, restart, assert
  exactly-once — specifically exercising the `in_flight` recovery path (§6.1).
- Token rotation test: expire the access token mid-batch, assert clean recovery
  and that no token generation is lost.
- CDC truncation test: a mock returning exactly 1000 objects must trigger window
  halving, not cursor advance.

## 11. Performance targets — benchmarked, not asserted

| | Target |
|---|---|
| Cold start to usable | < 1000 ms |
| Lookup / search keystroke | < 16 ms |
| Add an invoice line | < 16 ms |
| Save a document (local commit) | < 50 ms |
| Full initial sync, both realms | report actual; target < 10 min |

A synthetic realm generator produces 50k transactions so these can be measured
without hammering Intuit. Numbers get reported as measurements, never as claims.

---

## 12. Open items

| # | Item | Blocks |
|---|---|---|
| 1 | Confirm §0 ⚠️ items against `developer.intuit.com` — batch limit, CDC entity exclusions, `RequestId` scope | M2 write path |
| 2 | Measure WaterLine CNC realm size | initial sync sizing |
| 3 | Confirm QBO's tax rounding mode (§1) | ledger project, not this one |
