# Design review decisions

Running log of decisions made during review of `DESIGN.md`. Newest last.
Each entry records what was decided and why, so the reasoning survives the
conversation it was made in.

---

## D1 — DESIGN.md is scoped to the near-term QBO front-end

**Decided:** `DESIGN.md` covers `qbo-local` — the local replica and fast UI that
puts QBO data on local disk and keeps the network out of the interaction path.
The double-entry accounting system is a later project and gets its own
`LEDGER-DESIGN.md`, written when that project starts.

**Why:** `qbo-local` ships in weeks and is the near-term priority. The accounting
system is intentionally less defined for now — "start off QuickBooks-like, then
build out features that make sense for me" — so designing it in detail today
would be designing against requirements that do not exist yet.

**Consequence:** the posting-rules table, the double-entry schema, and the ledger
command/oplog design leave `DESIGN.md`. They are prerequisites for the ledger
project, not for this one — `qbo-local` posts nothing, it mirrors what QBO has
already computed.

---

## D2 — Approved structure for DESIGN.md

1. Money and rounding — kept as drafted
2. Domain enums and QBO entity models — kept as drafted
3. Replica schema — SQLite tables, FTS5 search (new)
4. Sync: CDC cursor strategy and its failure modes (new)
5. Token rotation and durable credential persistence (new)
6. The outbox: state machine, rejection handling, dependency resolution (new)
7. Reconciliation sweep (new)
8. Safety: read-only default, audit log, backups (new)
9. Forward-looking note on the accounting system — direction, not design (shrunk)

Sections 3-8 do not exist in the current draft and are the substance of the
rewrite. Several depend on Intuit API facts the current draft left explicitly
unverified (CDC lookback window and entity coverage, current rate limits);
these get verified against live Intuit documentation and cited.

---

## D3 — `ledger-core` shrinks to money and rounding

**Decided:** the shared crate holds the money type, the rounding policy, and
nothing else. QBO entity models move into `apps/qbo-local`. The ledger project's
`Command` type is not written until that project exists.

**Why:** a shared library is a contract with its consumers, and right now there is
one real consumer and one hypothetical one whose requirements are explicitly
unsettled. Money math is stable regardless of what the accounting system becomes —
cents are cents — and is the one thing that must not be duplicated. Everything
else was speculative sharing. The QBO entity models in particular are Intuit's
data shapes and do not belong in the core of an eventual QuickBooks replacement.

**Cost of being wrong:** low. Single repository, so moving code between crates is
a file move plus a manifest line.

---

## D4 — Complete history, staged entity coverage

**Decided:** mirror all history for every entity that gets mirrored. Stage which
entity types arrive first: masters and documents in M0, peripheral types
(Attachable, Department, Preferences) after.

**Why:** measured against the live realm — roughly nine thousand invoices going
back to 2012, with total documents across all types in the tens of thousands.
That is a few hundred MB of raw JSON and a one-time sync measured in minutes.
(Exact figures deliberately not recorded here; re-measure when needed.) A recent-window replica would save almost nothing and would send lookups
of older records back to the browser, which defeats the purpose of the project.
Depth is never staged; breadth is, only so a working sync arrives sooner.

---

## D5 — Remaining recommendations accepted en bloc (autopilot)

Dan stepped away and authorised proceeding on all standing recommendations.
Applied:

- `round_money` returns `Result` rather than panicking on overflow; all `Money`
  arithmetic returns `Result`.
- `RealmId` newtype with structural scoping enforcement — private connection,
  realm as first parameter on every repository function, `realm_id`-prefixed
  indexes. Phantom-typed connections rejected as disproportionate.
- `EntityType` introduced, wider than `DocumentType`, fixing an outbox type that
  could not represent Customer/Vendor/Item.
- `class_id` uses the `ClassId` newtype consistently; the draft contradicted
  itself between its §4 and §5.
- Banker's-rounding-on-tax kept as specified, but the unverified "QBO rounds this
  way too" justification is removed and recorded as an open risk for the ledger
  project.

---

## D6 — API facts verified where possible; two corrections

`developer.intuit.com` is blocked by this environment's network egress policy, so
primary-source verification was not possible. Secondary sources were used and
every claim in `DESIGN.md` §0 carries a confidence level. Two findings changed the
design:

1. **The batch endpoint limit is 120 requests/minute per realm**, not the ~40 the
   kickoff prompt assumed — changed 31 October 2025.
2. **`RequestId` idempotency reportedly does not cover Customer or Item.** That is
   exactly the outbox's dependency-resolution path, so those two types get a
   query-before-create guard rather than relying on `RequestId`.

Also load-bearing: CDC returns at most 1000 objects per response, so a response at
the cap must be treated as truncated and re-polled over a narrower window rather
than accepted as complete.

Both ⚠️ items must be confirmed against Intuit's own documentation before the M2
write path ships.

---

## D7 — Fiscal year and period locking

**Decided (Dan, 20 August 2026):** fiscal year is the **calendar year** for both
entities. A closed period **rejects** posting outright.

**Why reject rather than warn:** a warning that can be clicked through is not a
control — it is a speed bump that produces an audit trail of people ignoring it.
If a period is closed, the correct response to a late transaction is a dated
entry in an open period, not a quiet backdate into a period the CPA has already
signed off.

**Consequence:** every action that writes to the ledger passes through one gate
before it does anything. Reopening a period is permitted, because sometimes a
bill genuinely arrives late — but it is never quiet. Each reopen writes its own
flagged line into the close history, so a period that was reopened cannot later
be made to look like one that was never touched.

These were the last two open items on the ledger side. The remaining open
questions are the API confidence items in `DESIGN.md` §0 and the class taxonomy.

## D8 — Rounding gets a third policy: `MirroredAmount`

23 Aug 2026, in build.

`round_money` is the only `Decimal → Money` conversion point, and its policy
enum names *why* a rounding decision is being made — `LineExtension`,
`TaxCalculation`. Projecting a QBO amount is neither: the book of record already
rounded it, and `qbo-local` is changing representation, not deciding anything.

Borrowing `LineExtension` for it would have been a lie about intent in the one
place the codebase is most careful about intent. So the policy set grows by one:
`MirroredAmount`, documented as unreachable by construction — its caller rejects
anything over two decimal places before it gets there, and if the strategy ever
does fire, the precision check is the bug.

## D9 — An unreadable amount is quarantined, never defaulted

23 Aug 2026, in build.

Two cases, one rule. A document with no readable `TotalAmt` does not project as
zero, and an amount carrying three decimal places does not get rounded to two.
Both go to `quarantine_entities` with the reason, raw JSON untouched in
`entities`.

The asymmetry is the point: a quarantined row is recoverable by fixing the parser
and re-projecting, and it is visibly wrong in the meantime. A wrong figure
written into the projection is neither. The one exception is a description-only
line with no `Amount`, which genuinely contributes nothing — there, zero is the
honest reading rather than a default.

## D10 — A bare number is a reference, not an amount

23 Aug 2026, in build.

Search tries exact identifiers before text, which follows directly from the
brief. The judgement call is what to do with digits: `21234` could be invoice
21234 or $21,234.00.

It is treated as a reference only. Amounts are searched only when the query says
money — a currency symbol, a thousands separator, or a decimal point. Searching
both would mean every reference lookup drags a list of coincidental totals behind
it, which defeats the requirement that typing a number lands on the transaction.

## D11 — `LinkedTxn` types stay strings

23 Aug 2026, in build.

QBO's `TxnType` vocabulary in a link is wider than the entity names the API
accepts on the wire: `BillPaymentCheck`, `Check` and `ReimburseCharge` all appear
and map to no single endpoint. Narrowing links to the `DocumentType` enum would
mean either silently dropping those edges or inventing a mapping.

Neither is acceptable in a lineage view whose whole value is that it shows the
real chain, so the payload's own word is kept. Links pointing outside the
mirrored window are reported in an `unresolved` list for the same reason: a gap
in a chain should read as a gap, not as the chain ending.

## D12 — CDC truncation recovers through the query endpoint, not by halving

23 Aug 2026, in build.

§4.3 originally said to halve the CDC window and re-poll until responses came
back under the cap. Building the driver showed that cannot work: CDC takes a
`changedSince` and no end bound, so there is no window to halve. The only
available move is advancing the start, which is the exact silent-loss case the
rule existed to prevent.

The query endpoint takes both bounds and pages, so a truncated window is re-read
there instead — completely, at a page size we control. `halve_window` and its
tests have been **deleted** rather than left in place. Machinery with tests
around it reads as a strategy in use, and this one was neither used nor usable;
leaving it would have meant the next person implementing against §4.3 built the
wrong thing twice.

The cost of being wrong here is bounded and worth stating: if the query endpoint
turns out not to accept a `LastUpdatedTime` range the way §0 assumes, the
fallback is a full sweep — correct, and expensive. That assumption is on the §12
open-items list with the rest of the ⚠️ API facts.

## D13 — The sync cursor advances only inside the transaction that wrote its entities

23 Aug 2026, in build.

§4.1 asked for this and nothing enforced it. `Store::apply_batch` now takes the
entities and the cursor together and commits them in one transaction, so a
cursor that has moved is a cursor whose records are on disk.

Two consequences fall out. A multi-page sweep writes **no** cursor until its last
page, so a crash halfway through re-sweeps rather than resuming from a position
that was never fully covered — the partial pages are already mirrored, so the
re-sweep is idempotent, not wasted. And batching is now the normal path rather
than an optimisation, which fixes the one-transaction-per-entity cost flagged in
§11 for initial sync.

## D14 — M1 stack: Tauri v2 shell over the prototype's vanilla front-end

11 Sep 2026, Dan.

The brief said Tauri + React + Vite. The prototype is 3,900 lines of vanilla
HTML/JS that already encodes every M1 read screen, and the cutover target
(D16) makes the cheapest path to a real screen the right one. So: Tauri v2 for
the native shell and the no-network guarantee, the prototype's view code kept
and its fake data layer replaced with `invoke` calls into a read-only query API
over `Store`. No framework until the vanilla code is shown to be the bottleneck.

*Rejected: React as briefed — every screen rebuilt for no M1 benefit. Rejected:
an axum server with the prototype in a browser — a listening port on the machine
that holds the book, and not the desktop app the brief asked for.*

## D15 — Customer payments move to Authorize.net at cutover

11 Sep 2026, Dan. Amended the same day.

First reading: QBO Payments is in use, so a replacement is a hard blocker.
Corrected by Dan: it is used sparingly, and the replacement is Authorize.net,
which the business already has an account with. So it is a go/no-go item
(hosted payment link on the own invoice, settlement import posting the customer
payment) but not one that keeps invoices flowing to QBO after cutover.

## D17 — The decoupling blockers, as Dan scoped them

11 Sep 2026, Dan.

- **Bank and card feeds:** import the statements the bank and Chase already
  generate; no aggregator. Reconciliation is a per-statement close.
- **Sales tax:** replicate the handful of QBO reports the filing is done from.
  No tax engine. Which reports is still to be named.
- **1099-NEC:** out of scope entirely.
- **Accountant:** an accountant mode in the app, modelled on what QBO gives the
  CPA, rather than an export pack. Read-only, period-locked, with an
  adjusting-entry request queue Dan approves. Scoped in `LEDGER-DESIGN.md`.
- **Channel intake and every other skill:** an MCP server over the own store,
  so the existing `claude-config` skills are repointed rather than rewritten.

*Rejected: Plaid, a tax engine, and a CPA export pack — each solves a problem
Dan does not have, at a cost he would pay every month.*

## D16 — Cutover target 1 January 2027, fallback 1 January 2028

11 Sep 2026, Dan.

Fiscal year is calendar year (D7), so cutover is a year boundary. 1/1/27 is the
target; the go/no-go is 1 November 2026 against the bar in ROADMAP §F; missing
any item moves the date to 1/1/28 without argument. The bar is not shortened to
hit the date.

## D18 — LEDGER-DESIGN.md first draft

11 Sep 2026.

`LEDGER-DESIGN.md` exists, written from ROADMAP §B2, §E, §F, §G and §H, the
prototype's ledger, period-close and manufacturing sections, and the original
kickoff brief. Fourteen sections: scope and basis, the posting-rules table,
chart of accounts, class taxonomy, ledger schema and the command/oplog design,
period close, import from the replica, the parallel run and trial-balance diff,
accountant mode, the sales-tax liability report, bank statement import,
manufacturing costing, what is deliberately excluded, and the open items.

**It is a draft for Dan's and Joel's review, not an approved design.** It
carries **30 open policy items**, marked `⚠️ Dan/CPA to confirm` and collected
as W1 to W30 in §13, each stating what changes if the answer is no. Thirteen
are Dan's, sixteen are Joel's, one is a fact for engineering to verify against
Intuit's documentation. Six of them — W2, W3, W4, W12, W15 and W16 — gate
ROADMAP §B', because they are the ones that cannot be changed once history has
been imported.

Nothing here is decided. The document's purpose is to be marked up.

## D19 — The sales tax return is a four-line derivation, not a tax engine

11 Sep 2026, Dan.

Dan showed the sheet he files from each quarter: A = total income from the P&L
for the quarter; B = total sales tax collected; C = B / 6.625% = taxable sales;
D = A - C = non-taxable sales. The own system reproduces those four lines from
the ledger (`LEDGER-DESIGN.md` §9) and adds a fifth: taxable sales summed from
line-level `is_taxable`, with the variance against C shown, so the derivation
can be checked against the lines for the first time.

*Rejected: a per-agency, per-tax-code liability engine. New Jersey at one rate
is the whole filing today; the rate is a config value in case that changes.*

## D20 — Four ledger policies decided: W2, W3, W4, W12

12 Sep 2026, Dan, each as the draft recommended.

- **W2** Raw-material bills capitalise to inventory; cost reaches COGS when a
  build consumes the material.
- **W3** A vendor credit reduces the landed cost of the lot it relates to.
- **W4** Build variance plugs to a manufacturing-variance account; finished
  goods carry build-sheet standard cost.
- **W12** Inventory is valued at moving weighted average.

*Rejected: expensing material on purchase (what QBO does for Dan today), which
makes a large foam order distort two months of margin; FIFO, which needs lot
tracking on every cut for a material where lots are interchangeable.*

## D21 — W6, W8, W15, W16 decided

12 Sep 2026, Dan, each as the draft recommended.

- **W6** Shipping charged to customers is income (4300), so line A of the
  sales tax worksheet keeps its shape.
- **W8** Customer payments land in undeposited funds by default; bank matching
  is per deposit, which is what a batched Authorize.net settlement needs.
- **W15** Aquamentor's class list is `foam`, `sign`, `chair`, `drop`, `cnc`,
  `uv`; `cnc` and `uv` exist in both books.
- **W16** At import an unclassed line takes its item's default class and is
  marked `class_source = 'item_default'`; only a line whose item has no default
  is quarantined.

Every import-gating item (W2, W3, W4, W12, W15, W16) is now decided. The ledger
engine and the replica import (ROADMAP §B') are unblocked on Dan's side; the
sixteen Joel items remain open and none of them gate the start of §B'.

## D22 — Ledger engine built in four parallel tracks against a shared contract

12 Sep 2026, in build.

`apps/ledger` went from a stub to an engine in one session: a shared
`types.rs` and `chart.rs` were written first, then the store (§4, §5, §7,
§9), the posting function (§1) and the importer (§6) were built in parallel
against them, and a fourth pass wired import to post to store and added the
`ledger` binary.

Two judgement calls made in build, both Dan's to override:

- **A re-run replaces, never duplicates.** A document whose saved payload is
  unchanged is skipped; one whose payload changed has its old entries reversed
  as of the new `txn_date` and is reposted. Import is therefore safe to run
  nightly against a moving replica.
- **Seed-chart accounts take the QBO id the import maps onto them**, first
  writer wins, so the §7 diff can join 1200, 2200 and the bank accounts. A
  second QBO account landing on the same number is reported for mapping rather
  than silently taking or losing the slot.

Known gaps, carried in `ROADMAP.md` §0, closed in the same build session that
follows this one (D23):

- ~~opening balances and the boundary-year walk are implemented but not wired
  into the pipeline~~ — closed: `pipeline::apply_opening_balance` and
  `pipeline::boundary_walk`, plus `ledger opening` and `ledger boundary`.
- ~~`ledger init` is not idempotent~~ — closed: `create_company` upserts the
  company row and seeds accounts/classes with `ON CONFLICT DO NOTHING`.
- ~~non-posting documents are re-saved on every run~~ — closed: an unchanged
  Estimate or PurchaseOrder is now skipped the same way a posting document is.
- ~~§9 line E needs per-line taxability on `journal_lines`, which the schema
  does not carry yet~~ — closed: migration 2 (D23).

## D23 — §9 line E exists; a non-zero variance is not by itself a problem

12 Sep 2026, in build.

`journal_lines` gained `is_taxable`, `tax_amount_minor` and `tax_rate`
(migration 2). `crate::post` sets them on the 4100/4300 income leg an
Item or Shipping line produces, from the document line's own `is_taxable`
and, when the document carries tax, that line's share of it
(`amount × rate`, `RoundingPolicy::TaxCalculation`). `report::sales_tax_lines`
sums the taxable lines' credits for the quarter as E, and reports `E - C`.

The variance is the point, not a defect to chase to zero. E comes from what
was actually marked taxable on each line; C comes from B (the 2200 balance)
divided by the rate, and B itself is already a rounded figure — every tax
amount posted was rounded per line (banker's rounding, §9), so summing them
back and dividing by the rate does not, in general, reproduce the exact
pre-tax subtotal to the cent. A cent or two of variance most quarters is
therefore expected and uninteresting; what §9 asks Dan to look at is a
variance that is *large* relative to volume — the sign of a taxable line
invoiced with no tax, an exempt customer charged tax, or a tax adjustment
posted straight to 2200 rather than through a document line.

*Rejected: rounding C the same way B was rounded so the two always match to
the cent.* That would make line E's whole reason for existing — catching a
line that disagrees with the tax account — invisible exactly when rounding
is the only thing separating them, which is the common case, not the rare
one.

## D24 — Quarterly cutover: books open 1/1/27, QBO stays the entry point until cutover

28 Sep 2026, Dan.

Replaces the 1/1/27-cutover / 1/1/28-fallback plan (ROADMAP §F as of 11 Sep).

- **New books open 1 January 2027** with opening balances as of 31 December
  2026, taken from QBO.
- **January–March 2027: QBO stays the entry point.** The nightly
  `ledger import` + `ledger tbdiff` is the parallel run and backfills Q1 into
  the new books as it goes.
- **After Joel's FY2026 close**, re-apply the opening balances (his adjusting
  entries move 12/31/26) and lock 12/31/26 and earlier.
- **Go/no-go ~15 March 2027; cutover 1 April 2027.** A no-go rolls to the next
  quarter boundary: 7/1/27, 10/1/27, 1/1/28. The parallel run continues until
  one is clean. **FY2027 lives whole in the new books** regardless of which
  quarter cutover lands on.
- **Q1 2027 sales tax is filed from QBO**; the own four-line report runs
  alongside as a check.
- **Phase C (outbox live to QBO) becomes optional and deferred.** QBO is
  current because it is the entry point until cutover; nothing needs mirroring
  to it. Reprioritised ahead of it: write UI into the own store, skill
  repointing via `ledger-mcp`, Authorize.net, Joel's written sign-off.
- **October–November 2026: practice parallel run** on Q3/Q4 2026 data, to
  surface posting-policy differences early while there is time to decide them
  with Joel.

Why: the 1/1/27 cutover needed a full quarter of parallel run starting ~1 Oct,
and Phase A had not started. Opening the books on a year boundary but cutting
over on a quarter boundary keeps the fiscal year whole, keeps QBO as the
single entry point (so there is no dual entry and no write path to production
QBO before cutover), and turns a missed date into a three-month slip instead of
a twelve-month one.

*Rejected: keep 1/1/27 as the cutover with a 1/1/28 fallback.* A miss costs a
year, and the only way to make the date was to shorten the parallel-run bar.
*Rejected: open the new books on the cutover date.* A mid-year opening splits
FY2027 across two systems and makes the year-end package a merge.

## D25 — The four ⚠️ API facts, checked against Intuit's own pages

28 Sep 2026, Cowork session on Dan's machine, pages read in Chrome
(developer.intuit.com renders client-side; plain fetchers get an empty shell).

| # | Claim | Intuit says | Verdict | Code |
|---|---|---|---|---|
| 1 | Batch 120/min/realm | Limits page: "Throttled at 40 Batch requests per minute, per realm ID and app" and "120 Batch requests per minute per realm ID"; general 500/min/realm, 10/s per realm and app; 429 → wait 60 s; query max 1000 rows | **Wrong for us**: one app per realm, so 40 binds | `RealmLimits::default().batch` 120 → 40 |
| 2 | `RequestId` does not dedupe Customer/Item | Help article "What is RequestId and its usage": same RequestId → "sends the same response as it did for the original request"; max 50 chars (36 for batch). No entity exceptions listed | **Unconfirmed either way**; Intuit states no exception | None. The query-before-create guard stays (harmless if coverage is total). Settle empirically in the sandbox if Phase C is ever built (D24 defers it) |
| 3 | CDC exclusion list | CDC page: "all API entities except" JournalCode, TaxAgency, TimeActivity, TaxCode, TaxRate | **Obtained** | `sync::CDC_EXCLUDED` = TaxCode, TaxRate; `cdc_coverage_confirmed` returns false for them. Before this they were CDC-polled and would silently never refresh |
| 4 | CDC lookback 30 days | CDC page: "CDC operations can track changes within the last 30 days"; "a maximum of 1,000 objects" | **Confirmed** | None (`CDC_LOOKBACK_DAYS` = 30, max-age 25) |
| 5 | Query supports a `LastUpdatedTime` range (D12 backfill) | Query page: multiple filters joined with AND, range example on `MetaData.CreateTime >= … AND <= …`; OR unsupported; Id supports only = and IN | **Syntax confirmed**; `LastUpdatedTime` filterability is per entity in the API reference. Proven for real on the first live truncation backfill | None |

URLs: developer.intuit.com/app/developer/qbo/docs/learn/limits-and-throttles ·
…/explore-the-quickbooks-online-api/change-data-capture ·
…/explore-the-quickbooks-online-api/data-queries ·
help.developer.intuit.com/s/article/What-is-RequestId-and-its-usage.

DESIGN §0 and §4.4 updated. Phase C's gate (verified facts) is now met except
item 2, which only a sandbox can settle.

## D26 — M0 first live sync: five fixes, and what the numbers are

13–14 Oct 2026, local Claude Code session on Dan's Mac, read-only against
production. The Rust side's first HTTP calls to Intuit.

**Measured.** A clean initial pull into an empty replica, both query fixes in
(`.local/m0-run/initial-pull-timing.log`): **225 s wall clock, 81 requests,
67,747 rows, rc=0**, target < 600 s. Per entity: CompanyInfo 1, Account 346,
Class 3, Customer 10,127, Vendor 1,280, Item 3,473, TaxCode 539, TaxRate 715,
Term 30, Estimate 2,520, Invoice 8,418, SalesReceipt 1,191, CreditMemo 230,
RefundReceipt 71, Payment 6,924, PurchaseOrder 3,969, Bill 3,900,
BillPayment 3,249, VendorCredit 135, Purchase 16,298, Deposit 3,432,
JournalEntry 896. The reconcile sweep over the main replica then reported
every type remote = local with 0 missing, 0 stale, 0 extra, `clean: true`,
quarantine 0 (229 s). The first pull, before the fixes, took 334 s.

**The two-week read-only window (DESIGN §8) starts 14 Oct 2026**, the day the
sweep came back clean. Earliest write go: 28 Oct 2026, on Dan's word only.

Five things the fake Intuit server could not have shown:

1. **Projection drops MetaData** (`862fc9a`). `SELECT Id, MetaData FROM
   CompanyInfo` came back with `DefaultTimeZone, Id, SyncToken, sparse` and no
   `MetaData.LastUpdatedTime`. `HttpQboClient::index` now checks every row of
   a projected page and, when any lacks the timestamp, falls back to the full
   `SELECT *` page for the rest of the run, saying so once on stderr.
   *Rejected: special-casing CompanyInfo.* Nothing documents which entities
   honour the projection; checking the answer covers the next one too.
2. **Sync ordering** (`0cb72b8`). On an empty replica the reconcile sweep
   finds every id missing and heals each with its own GET — 226 s and then
   "rate budget exhausted". `m0-live.sh sync` now runs `daemon --once` first
   (the driver's full sweep pages `SELECT *` at 1,000 a page, ~80 requests for
   the whole book) and the reconcile sweep after it, as verification only.
3. **Paging on a tied key** (`889188d`). Every paged query sorted on
   `MetaData.LastUpdatedTime`, which has ties wherever records were
   bulk-imported. Offset paging over ties lets QBO order the tied rows
   differently on each request: the first pull read 15 Customers twice and
   never saw 15 others; the index missed 41 Items. Both now sort on `Id`. No
   caller depends on the order: the cursor advances to the sync's start time.
4. **Inactive name-list records** (`857b866`). QBO filters Account, Class,
   Customer, Vendor, Item, Term and Department queries to `Active = true`
   unless asked. The first pull never mirrored 224 inactive Items, 32
   Customers, 22 Vendors or 5 Accounts, all still referenced by old
   documents; the gap only showed when 41 Items were deactivated in QBO on
   14 Oct, CDC (which returns inactive rows) mirrored the change, and the
   index flagged them as extra. Queries now carry `WHERE Active IN (true,
   false)`, ANDed with the backfill window. The six cursors were cleared once
   to force a full re-pull; the sweep after it was clean.

5. **EOF on stdin stopped the daemon** (`b2aaea4`). The stop watcher treated
   end of stdin as "stop", so a daemon started in the background (`nohup`,
   launchd, a non-interactive shell) ran one tick and exited cleanly. Only a
   line reading `stop` ends it now. This would have hit the launchd job too.

**Daemon, measured** (`status-t0-step5.log`, `status-t10-step5.log`): over ten
minutes the CDC cursor advanced 16:28:50 → 16:41:35 UTC on every entity type,
one new Bill arrived (3,900 → 3,901), quarantine stayed 0. **Restart:** the
daemon was killed and restarted with `m0-live.sh daemon`; it ticked within
seconds with no token prompt. **Rotation:** the access token was refreshed
and rotated at 16:33:39 UTC while the daemon ran; the restarted daemon and
`status` read the rotated keychain item without a prompt. **24 hours:** by
15 Oct 16:03 UTC the daemon had run 23 h unattended (two transient network
timeouts, each retried next tick; nightly snapshot at 07:00 UTC); Intuit
rotated the refresh token overnight (expiry 23 → 24 Jan 2027) and `status`
showed the realm authorised. Dan ticked the 24-hour box on this rotation
evidence; the token was in use throughout, so a no-use 24-hour gap is
untested.

**Fixtures: recorded, not committed.** `record` ran (rc=0, 81 pages, 172 MB
under `.local/fixtures/raw`), then `scrub` hit two things. The fake-name list
held 576 names against ~11,400 customers and vendors (fixed, `ca04d5e`). After
that the leak detector refused the run with 933,320 hits, because 806 real
name parts are one to three characters long and the detector matches
substrings, so it fires on `USD` and `QBO`. It worked as designed: nothing was
written. Even a clean run would not be committable as it stands, because the
scrubber keeps amounts by design (`Balance`, `CurrentBalance`, every total),
and HANDOFF §2.6 forbids real balances. What it needs, for Dan to decide:
record a small slice rather than the whole book, scrub balances and amounts,
and have the detector match whole words (or whole fields) for short names
instead of being loosened.

Operational, not code: the token store is the login keychain, and every
release build is ad-hoc signed, so **each rebuild of `qbo-local` triggers one
macOS "allow access" dialog** that only a person can answer ("Always Allow").
Unanswered, the call blocks; after ~8 h macOS refused it (`OSStatus -60008`,
`sweep-attempt3-keychain-timeout.log`). Signing with a stable identity would
end the per-build prompt; deferred to M1.

Not hit, still open: `Reconciler::spend` and `SyncDriver::spend` pass the
run's start time to `try_acquire`, so the bucket never refills within one run
and a single sweep or tick is capped at ~500 requests. The whole book is 81
requests, so only a sweep with many hundreds of heals would reach it.

## D27 — M0 tbdiff baseline

14 Oct 2026, first `ledger import` against the live replica, then `ledger tb`
and `ledger tbdiff` against QBO's own trial balance as of the same date
(`qbo-local report --name trial-balance --live`, 192 rows). This is the
baseline the parallel run starts from. No code was changed to move any of it.

Import: 51,233 documents translated, 4,781 posted, 6,489 non-posting,
**39,963 rejected**; 318 accounts created, **185 needing mapping**, 3 classes
created. Rejections by reason: 29,253 no class on the line, its item or the
header; 6,920 payment applications plus unapplied ≠ stated total; 3,775 entry
does not balance; 14 journal entry debits ≠ credits; 1 no account for a line.
By type: Purchase 14,187, Invoice 8,418 (every one), Payment 6,921,
Bill 3,672, BillPayment 3,249 (every one), Deposit 1,438, SalesReceipt 1,191,
JournalEntry 468, CreditMemo 230, VendorCredit 118, RefundReceipt 71.

`tbdiff.log`, verbatim except that the 148 per-account `skip` rows between the
summary and the footer are left out: they name real counterparties and lenders
and stay in `.local/m0-run/tbdiff.log` (HANDOFF §3.5).

```
TRIAL BALANCE DIFF   aquamentor   as of 2026-10-14

account                                       ledger           qbo       delta  tier
Bank — Checking (1100)                    1448959.17      14681.50  1434277.67  must
Credit card (2100)                          77478.37          0.00    77478.37  must
Accounts receivable (1200+2300)            -16519.97      89717.17  -106237.14  must
Accounts payable (2000+2050)              -541047.40     -22339.48  -518707.92  must
Sales tax payable (2200)                      762.84          0.00      762.84  must
Total income (4xxx)                             0.00   -1067532.28  1067532.28  must
Total equity (3xxx)                       1071003.15      22113.17  1048889.98  must
Inventory, raw materials (1300)                 0.00          0.00        0.00  may
Inventory, finished goods (1310)          -125027.54      99117.18  -224144.72  may
Cost of goods sold (5000)                       0.00        633.71     -633.71  may
Manufacturing variance (5100)                   0.00      64297.22   -64297.22  may
Parts and labour applied (5300)                 0.00          0.00        0.00  may
Undeposited funds (1150)                   -28001.73      35212.69   -63214.42  may
Sales income (4100)                             0.00          0.00        0.00  may
Shipping income (4300)                          0.00      -3295.94     3295.94  may
Discounts given (4900)                          0.00          0.00        0.00  may
Returns and allowances (4950)                   0.00          0.00        0.00  may
[... 148 per-account skip rows omitted, see above ...]

MUST-MATCH FAILURES: 7        MAY-DIFFER UNEXPLAINED: 5
```

Open questions for Dan and Joel, each with a best guess at the policy behind
it. A guess is a place to start the conversation, not a finding.

1. **Class is mandatory in the ledger and nearly absent in QBO.** 29,253
   rejections; QBO has 3 classes and most lines carry none. Every income,
   AR, inventory and COGS variance below is downstream of this. *Guess:* W16
   (D21) falls back to the item's default class, but QBO items carry none, so
   the fallback never fires. Options: assign a default class to each of the
   3,473 items (once, in the ledger's own item map), a single "unclassified"
   class for history before a cut date, or class required only from the
   1 Jan 2027 opening. The three QBO classes also did not match the W15 list
   (`foam`, `sign`, `chair`, `drop`, `cnc`, …).
2. **Payments and bill payments import with zero applications** (6,920 +
   3,249). In the raw QBO payment the applied amount sits on `Line[].Amount`;
   `LinkedTxn[]` has only `TxnId`/`TxnType`. `ledger`'s
   `collect_applications` reads `LinkedTxn[].Amount`, so every application
   is 0. *Guess:* not a policy question but an importer mapping defect,
   recorded here rather than fixed because this entry is a baseline. Fixing
   it should move AR, AP, Undeposited funds and Checking more than anything
   else on this list.
3. **Checking (1100): ledger 1,448,959.17 vs QBO 14,681.50.** *Guess:* two
   things at once: (a) outflows (bill payments, purchases, deposits) are
   mostly rejected, so the ledger keeps inflows it never spends; (b) QBO
   splits cash across many bank accounts (PayPal, processors, a second
   checking) and the ledger's 1100 may be collecting some of them.
4. **Total income: ledger 0.00 vs QBO −1,067,532.28; equity 1,071,003.15 vs
   22,113.17.** Every invoice and sales receipt was rejected (no class), so no
   income posted. The ledger's equity looks like an opening-balance or
   unmapped-account plug landing in 3xxx. *Guess:* QBO shows current-year
   income and closes prior years into retained earnings; the ledger needs to
   agree on that boundary (D24: books open 1 Jan 2027) and on where
   unmapped balances go.
5. **Accounts payable: −541,047.40 vs −22,339.48.** Bills partly post, bill
   payments never do (item 2), so AP only grows. *Guess:* resolves with
   item 2.
6. **Accounts receivable: −16,519.97 vs 89,717.17.** No invoices posted, but
   whatever payments and credit memos did reduced AR below zero. *Guess:*
   items 1 and 2.
7. **Credit card (2100): 77,478.37 vs 0.00.** *Guess:* QBO books card spend to
   named card accounts that the tbdiff's 2100 line does not include, while
   the ledger defaults card purchases to 2100. Mapping question: which QBO
   accounts are 2100.
8. **Sales tax payable (2200): 762.84 vs 0.00.** *Guess:* QBO keeps sales tax
   in its agency liability account ("sales tax pay", one of the 185 unmapped);
   the ledger posts line tax to 2200. Mapping question.
9. **May-differ, unexplained (5):** Inventory finished goods (1310)
   −125,027.54 vs 99,117.18; COGS (5000) 0 vs 633.71; Manufacturing variance
   (5100) 0 vs 64,297.22; Undeposited funds (1150) −28,001.73 vs 35,212.69;
   Shipping income (4300) 0 vs −3,295.94. *Guess:* inventory and COGS follow
   from rejected invoices (no relief) and from the D20 costing policy (W2
   capitalise raw material, W4 standard cost with variance to 5100, W12
   moving average) against QBO expensing material on purchase; the 5100 that
   QBO shows is history the ledger has no posting rule to reproduce; Undeposited funds drains through deposits while the payments that
   fill it are rejected (item 2); shipping income follows item 1.
10. **185 accounts need mapping before sign-off** (`ledger-import.log`).
    Several look like personal or owner items (loans, "Personal Expense",
    "y/e adj", "Ask My Accountant"). These are Joel's call.

## D28 — Standalone first, slim one-shot import last

15 Oct 2026, Dan. Amends D24 and LEDGER-DESIGN §6 ("Full history from
where").

**Order.** Build everything that lets the own books stand alone first —
document entry into the own store (invoice, PO, estimate, bill, receive
payment), bank lines, Authorize.net on the own invoice, skill repointing
through `ledger-mcp` — tested against a blank or synthetic book. The import
from QBO is the last step before cutover, run once.

**Scope of the import.** Slim, not full history:

1. Chart of accounts (the 185 `needs_mapping` accounts resolved), classes,
   customers, vendors, items, terms.
2. One opening-balance entry from QBO's `TrialBalance` as of the cutover
   date, balanced by construction; 3950 nets to zero.
3. Open documents only: unpaid invoices and bills at their open balance, open
   POs, open sales orders. Lines with no class take an import-only fallback
   class.
4. Checked: AR aging by customer and AP aging by vendor equal QBO's; the
   post-import trial balance equals QBO's on every must-tier line.

It is rehearsed as a dry run against the live replica before cutover day, so
the one real run holds no surprises.

**History** before cutover stays in the `qbo-local` replica, read-only and
searchable, and in QBO itself. It is not replayed into the ledger. The
full-history replay and boundary walk stay in the code but leave the cutover
path, and D27's rejection list stops being a blocker: classes on old lines,
old payment applications and old unbalanced entries no longer have to
reconcile.

**Still open** (Dan, Joel): the cutover date under this order. 1 January 2027
keeps FY2027 whole with nothing to replay, and needs the standalone pieces
and a clean dry run by mid-December. A later quarter boundary needs either
that quarter's documents replayed (one recent quarter, not fourteen years) or
FY2027 split across two systems. W21 (Joel accepting one opening-balance
entry for everything before it) now applies to the cutover date itself.

*Rejected: keep the nightly full-history import as the critical path.* 40k
of 51k documents were rejected on the first run (D27), mostly for history no
one will post against again. Fixing that first holds up the pieces that make
the switch possible.
*Rejected: drop history entirely.* The replica already answers every
historical lookup, so nothing is lost by not replaying it, and dropping the
replica would lose it.

## D29 — D24's dates stand under D28

17 Oct 2026, Dan. Settles D28's open item.

- **Books open 1 January 2027; cutover 1 April 2027**, rolling quarterly on a
  no-go; go/no-go ~15 March 2027. Unchanged from D24.
- **The slim import (D28) runs as of 31 December 2026**: chart and masters,
  one opening-balance entry from QBO's `TrialBalance` at 12/31/26, and the
  documents open on that date. It is re-run after Joel's FY2026 close, as D24
  already required of the opening balances.
- **Q1 2027 is the one replayed quarter.** QBO stays the entry point through
  March, and the nightly `ledger import` backfills January–March 2027 into
  the new books, checked by `ledger tbdiff` and the AR/AP aging. That window
  is held to the full posting gate: D28's fallback class covers only lines
  dated before 1 January 2027, so every Q1 2027 line needs a class in QBO.
- **W21** (one opening-balance entry for everything before the books open)
  applies to 1 January 2027.

Why: keeps FY2027 whole in the new books and QBO the single entry point
until cutover, with one quarter of replay instead of fourteen years.

### D28 addendum — how the open items and the opening entry share AR and AP

17 Oct 2026, the slim import as built (`ledger import-slim`,
`apps/ledger/src/slim.rs`, runbook `docs/CUTOVER.md`). Numbered as an
addendum because D29 and D30 are taken on other branches.

**The opening entry carries the control accounts; the open documents carry
the detail and never post.** The entry takes 1200 and 2000 straight from
QBO's trial balance. Each document open on the as-of date is saved (oplog
kind `import_slim_open_item`) and never posted, so its AR or AP is counted
once, in the opening entry. `books::open_documents` reads saved documents and
their applications, not entries, so it sees them like any other: the aging
is the sum of the open items, and the import checks that this equals 1200
and 2000 to the cent, then that each customer's and vendor's buckets equal
QBO's aging report. After cutover a payment applied to one of them relieves
it in the aging and credits 1200 in the GL, so the tie holds.

Three things make that true:

1. **Unapplied customer payments go to 2300.** QBO nets them into AR; the
   ledger's posting rule parks them in 2300 (W8). The opening entry moves
   their total from 1200 to 2300, so 1200 equals the open invoices less open
   credits, and §7's AR group (1200+2300) still equals QBO's AR. The aging
   check adds them back per customer to compare with QBO.
2. **Open on the date, from the documents.** An invoice is open for its total
   less every application dated on or before the as-of date, the rule
   `books::open_items` (now pure, shared with the ledger's own aging) applies
   everywhere. QBO's `Balance` field is today's balance, so a re-run after
   January payments would otherwise drop invoices that were open on
   31 December. A partly paid document is saved as one line at its open
   balance; a fully open one keeps its lines.
3. **Imported applications target imported ids.** Translated payments named
   their targets by bare QBO id (`1045`) while imported documents are keyed
   `qbo:Invoice:1045`, so the Q1 replay's payments would never have relieved
   anything. `translate::qbo_document_id` now builds both. In passing,
   `open_items` now reads a payment's application to a credit memo (recorded
   negative) as using the credit up, counts a bill payment's unapplied
   remainder as a negative AP item (`post` debits it to 2000), and ignores
   links to estimates and purchase orders.

**Guard.** Posting a new version of a carried document would book its AR or
AP a second time, so `ledger import` rejects it and `ledger-mcp` refuses it,
both naming the reason (`books::carried_by_opening`).

**Not imported: QBO sales orders.** The replica does not mirror them.

*Rejected: post the open documents and leave AR/AP out of the opening entry.*
An invoice posts income; posting 2026 invoices again would double FY2026
income, and reversing that would need a second plug per document.
*Rejected: unapplied payments as negative AR items.* It would make the
imported book disagree with the ledger's own rule for every payment entered
after cutover.

## D30 — The payment link is an Accept Hosted token, minted on demand

17 Oct 2026. Builds the D15 go/no-go piece: a hosted Authorize.net payment
page for the own invoice (`apps/ledger/src/authnet/link.rs`, `payment_link`
on `ledger-mcp`, `ledger authnet link`; `docs/AUTHNET.md`, "Payment link").

**Decided.**

1. **Accept Hosted, `getHostedPaymentPageRequest`.** The request carries
   `order.invoiceNumber` = the invoice's own number and `amount` = its open
   balance (total less every application so far), so the settlement import's
   existing `Invoice Number` match (`docs/AUTHNET.md`) applies the payment
   back to that invoice. `refId` is the number too. `customer.id` is the
   ledger contact id when it fits the gateway's rule (≤ 20, alphanumeric),
   `customer.email` the contact's email.
2. **A link is minted when it is about to be used.** The form token is valid
   for 15 minutes and must be POSTed. So `payment_link` returns the form URL,
   the token, `expires_at`, and a self-submitting HTML page — not a URL that
   can be printed on an invoice PDF. **Follow-up:** a durable "Pay now" link
   on a sent invoice needs a small public endpoint (e.g. an HMAC-signed
   `pay.aquamentor.com/i/<number>`) that calls this same code at click time.
   Not built; needs hosting decided.
3. **Refused** before any API call: not an invoice, voided, fully paid, no
   number, number over 20 characters (it could not be matched back).
4. **Sandbox by default.** Production needs `environment = "production"`
   *and* `allow_production = true` under `[authnet]` in the gitignored
   `.local/config.toml`.
5. **Credentials** never in a file: `AUTHNET_API_LOGIN_ID` /
   `AUTHNET_TRANSACTION_KEY`, else the macOS login keychain (service
   `com.aquamentor.ledger.authnet`, account `sandbox`/`production`, JSON
   `{api_login_id, transaction_key}`), stored by `ledger authnet
   credentials`, which reads stdin. A key written under `[authnet]` is a
   config error. `Credentials`' `Debug` and every error message omit both.
6. **`HostedPaymentClient` is the seam** (`qbo_local::client::QboClient`'s
   shape): `HttpAuthnetClient` (blocking `ureq`), `MockAuthnet` for the
   offline suite, and a loopback fake gateway test for the wire shape. No
   test reaches Authorize.net.
7. The CLI's settlement import loop moved into the library
   (`authnet::import_settlement`) so the end-to-end test drives the same
   code, and it now posts before saving: a document the rules refuse is no
   longer left saved, where the next re-import would have skipped it for
   good.

*Rejected: a Payment Link product.* Authorize.net's developer docs show no
payment-link API; Accept Hosted is the documented hosted-form path. The
Merchant Interface's own invoicing feature would bill from Authorize.net's
side, outside this book, and could not carry the ledger's numbering.

**API facts relied on**, from Authorize.net's own pages (read 17 Oct 2026):

| Fact | Source |
|---|---|
| API endpoints: sandbox `https://apitest.authorize.net/xml/v1/request.api`, production `https://api.authorize.net/xml/v1/request.api`; HTTP POST, `application/json` | [API reference](https://developer.authorize.net/api/reference/index.html) |
| Hosted form: sandbox `https://test.authorize.net/payment/payment`, production `https://accept.authorize.net/payment/payment`; the token is sent by HTML form POST | [Accept Hosted](https://developer.authorize.net/api/reference/features/accept-hosted.html) |
| "The form token is valid for 15 minutes." | Accept Hosted |
| Only `transactionType` and `amount` are required; `transactionRequest` takes `createTransactionRequest`'s children | Accept Hosted |
| Response: `token`, `messages.resultCode` `Ok`/`Error`, `messages.message[].code`/`text` | Accept Hosted; API reference |
| `amount` a plain decimal, no currency symbol; `order.invoiceNumber` ≤ 20; `order.description` ≤ 255; `customer.id` ≤ 20, alphanumeric | API reference (createTransactionRequest) |
| Settings `hostedPaymentReturnOptions` (`showReceipt`, `url`, `urlText`, `cancelUrl`, `cancelUrlText`), `hostedPaymentButtonOptions` (`text`), `hostedPaymentOrderOptions` (`show`, `merchantName`); values are JSON objects sent as strings | Accept Hosted |
| Element order: request `merchantAuthentication`, `refId`, `transactionRequest`, `hostedPaymentSettings`; transaction `transactionType`, `amount`, …, `order`, …, `customer`; customer `type`, `id`, `email` | Authorize.net's PHP SDK schema ([GetHostedPaymentPageRequest.yml](https://github.com/AuthorizeNet/sdk-php/blob/master/lib/net/authorize/api/yml/v1/GetHostedPaymentPageRequest.yml), TransactionRequestType.yml) and [sample code](https://github.com/AuthorizeNet/sample-code-python/blob/master/AcceptSuite/get-an-accept-payment-page.py) |

**Not verified** — handled defensively or to check in the sandbox:

- That the JSON API rejects out-of-order elements. Believed from developer
  forum reports, not found on an official page; the body is built from
  structs in schema order either way.
- That JSON responses can start with a UTF-8 byte-order mark. Same: forum
  reports only; `parse_response` strips one if present.
- Whether `refId` is echoed by this call, and whether a token can be posted
  more than once inside its 15 minutes. Not relied on.
- That the Merchant Interface's Transaction Detail export fills `Invoice
  Number` from `order.invoiceNumber` on an Accept Hosted sale. Strongly
  expected (it is the same field), but the first sandbox settlement should
  confirm it before cutover.
- The sandbox sign-up URL and test card `4111111111111111` in
  `docs/AUTHNET.md` are from Authorize.net's testing guide as commonly
  cited, not re-read today.

**Closed (was open):** a settled sale used to be applied for its full amount
even if the invoice was meanwhile paid another way (two links paid, or a
check plus a link), over-applying the invoice. `import_settlement` now caps
each application at the invoice's open balance (`books::open_documents`,
receivable book, every date) less what earlier rows in the same file already
applied, and parks the excess in 2300 with a warning; an invoice with nothing
open (paid or voided) takes the whole sale unapplied. Sales already imported
are dropped before the cap is computed, since the open balance already
reflects them.

## D31 — Real fixtures: a closed slice, amounts scaled per document, a secret seed

17 Oct 2026, in build. Unblocks D26's "fixtures: recorded, not committed".
(D29 and D30 are taken on sibling branches.)

**Slice, not the book.** `qbo-local slice` reads the replica read-only and
records a few hundred records instead of 67k: N customers and M vendors
picked by a hash of `--pick-seed` (or named with `--customer`/`--vendor`),
their `--max-docs` newest documents since `--since`, then closure — every
`…Ref` and `LinkedTxn` outward, recursively; every document that links *to*
an in-slice document (the payments applied to an invoice), so a balance can
be checked against what was applied; masters never pull documents. Deposits
are hubs (one deposit batches many customers' payments) and would drag in the
whole book: a deposit joins only when every document it links is already in,
and a payment's link to a deposit left out is removed from the sliced copy
(counted). A reference the replica itself cannot satisfy (US `TAX`/`NON`) is
left alone; any other reference outside the slice stops the run.
`--max-records` (default 1500) refuses a slice nobody can review. The slice is
recorded through the same `MockQbo` + `RecordingQbo` path as `record`, so
`replay` and the scrubber read it like any recording, plus
`deny-names.txt`: every customer and vendor name in the book, because a
memo in the slice can name a contact outside it.

**Amounts: scaled per entity, derived figures recomputed.** Each entity gets
its own factor f, HMAC(seed, "Type:Id") into [0.40, 0.80] ∪ [1.25, 2.50].
Leaves (line amounts, unit prices, unapplied amounts) are scaled and rounded
half-up to the cent (D9); UnitPrice keeps its own places and the line Amount
becomes Qty × UnitPrice. Everything derived is recomputed from the new leaves:
subtotal lines, percent discounts, NetAmountTaxable, each tax line at its
stored TaxPercent, TotalTax, TotalAmt; applications, Balance and
RemainingCredit; payment and deposit totals; contact Balance and
BalanceWithJobs; an account's CurrentBalanceWithSubAccounts. Applications
keep their shape — fully paid stays fully paid, unpaid stays unpaid, part
paid stays strictly part paid — and are spread across the paying lines by
largest remainder, so the cents add up exactly. Journal entries scale the
debit total and allocate both sides to it by largest remainder, so they
still balance.

Every derived figure is **residual-preserving**: new = formula(new leaves) +
round(f × (old − formula(old leaves))). A consistent original keeps a zero
residual and stays exactly consistent; a real-book oddity (D27's 3,775
entries that do not balance) stays odd by a scaled amount instead of being
silently fixed or left holding a real number. The scrubber checks this on
every run: any invariant the original satisfied and the output breaks stops
the run before anything is written.

**The seed is a secret** (`.local/fixtures/scrub-seed`, created on first
run). Fake names, emails, phones, streets and every factor are keyed with it,
so a known public price or a guessed customer name cannot be checked against
the fixture. Same seed, same slice: byte-identical output, so a re-record
diffs cleanly. `--keep-amounts` exists for invented data only (the committed
synthetic set, unchanged by this) and needs no seed; report files, whose
amounts no formula ties to the entities, are refused unless it is given.

**Leak detector, widened as D26 asked.** Names of three characters or fewer
match whole words only (no more `USD`); longer names still match as
substrings; digit-only names never match inside text; fakes are masked
before the check and never equal an original. It now also fails the run on
an original email, phone, street line or realm id anywhere; any email not at
example.com or phone-shaped number; an original amount of 10.00 or more
written as text; and any money field of 1.00 or more equal to its own
original. Any string that would trip it is replaced on the way through
(names) or redacted (`[scrubbed]`), so the detector stays a backstop.

*Rejected: one global factor.* One public price (a rescue tube's) would give
it away and with it every balance. *Rejected: integer multipliers* (exact,
no rounding repair): the gcd of the cents recovers the multiplier.
*Rejected: random fresh amounts.* Nothing would stay consistent without
re-deriving the whole book, which is the residual method anyway, minus the
real data's shape. *Rejected: loosening the detector for short names.*
Whole-word matching keeps them caught where they mean something.

Not run against the live replica: `tools/make-real-fixture.sh` is Dan's step,
and its output is read file by file before anything is committed.

## D32 — Practice parallel run, Q3 2026: what it found

17 Oct 2026, Claude Code session on Dan's Mac, read-only against the live
replica (ROADMAP §E). D30 and D31 are taken on other branches; this entry and
D33–D36 follow them. Write-up, commands and the per-month tables:
`docs/PRACTICE-RUN.md`.

**Shape.** D29's Q1 2027 run moved back two quarters: one opening-balance
entry from QBO's `TrialBalance` at 6/30/26, then 1,925 documents dated
7/1–10/16/26 replayed under the full posting gate (D28's fallback class only
before 7/1), into a scratch ledger. Trial balance diffed at 7/31, 8/31, 9/30
and 10/16; AR and AP aging against QBO's. A diagnostic second run let the
fallback class cover the window too, to see what is left once class is out of
the way.

**Four bugs, fixed, each with a test** (commits on
`claude/practice-parallel-run-0bdb38`):

1. *qbo-local reports ignored their as-of date.* QBO answers a
   `TrialBalance` with `end_date` and no `start_date` as "this
   month-to-date" ending today, and ages `AgedReceivables`/`AgedPayables`
   by `report_date`, not `end_date`. Every as-of report was today's.
   D27's baseline was unaffected only because it was taken for the day it
   ran; `ledger nightly` (as of yesterday), `ledger opening` and
   `qbo-local snapshots` for the boundary walk were all wrong.
2. *A card credit posted as a charge* (`Purchase` with `Credit: true`).
   One in the window, 23,590.98; 47,181.96 on one card in July.
3. *A deposit that sweeps a payment out of undeposited funds posted
   nothing.* QBO sends that line with a `LinkedTxn`, no `AccountRef` and no
   `DetailType`. 33 lines, 54,472.68: undeposited funds never drained and
   checking was short by the same amount.
4. *An item named "shipping…" on a bill vanished.* The translator's
   shipping-item name rule ran on every document type, and a shipping line on
   a bill posts nothing. One bill, 133.66, posted short of its total.

**After the fixes, with class out of the way**, AR (1200+2300), AP
(2000+2050) and undeposited funds tie to QBO to the cent on all four dates.
What is left:

| Must-tier line, 10/16 | Delta | Cause |
|---|---|---|
| Checking (1100) | 133,702.37 | Not mirrored: 135,758.35 of `CreditCardPayment` and a net 2,055.98 of `Transfer` |
| Credit cards (in skip rows) | −135,758.35 | The same card payments, the other side |
| Total equity | −1,000.00 | One `Transfer` to shareholder distributions, not mirrored |
| Total income | −10,322.98 | D34: items whose QBO "income" account is an expense or a liability |
| Sales tax payable (2200) | −405.32 | D35: QBO keeps NJ tax in its agency account; the mapping lands it elsewhere |

**Not policy, still to build:** qbo-local does not mirror `Transfer`,
`CreditCardPayment` or `InventoryAdjustment` (one 100.00 shrinkage in the
window), so the bank and card lines can never tie until it does. And the
ledger's own AR/AP aging misses every payment application: applications
target the bare QBO id while documents are keyed `qbo:<Type>:<id>` (0 of 349
window applications match; the slim-import branch already changes the
target to `qbo:<Type>:<id>`). Aging is to be re-checked after that merges.

Under the full gate, **1,323 of 1,925 documents (881,948.94) were
rejected, every one for no class**; D33. Code comments in
`import/translate.rs` cite "D29" and "D30" for decisions recorded under
other numbers; they predate this run and are left for whoever next edits it.

## D33 — Class in Q1 2027: QBO lines carry none

17 Oct 2026. **Question for Dan and Joel.** D29 holds Q1 2027 to the full
posting gate, so every income, COGS and class-consuming expense line needs a
class. On the practice window not one line has one: QBO's only classes are
"Store 1", "Store 2" and "Store 3", unused, and none of the 3,473 items has a
default class. Rejected: every invoice (188, 250,338.92), every sales receipt
(188, 42,052.47), 650 of 692 purchases (258,653.68), 181 of 192 bills
(184,316.53), 105 of 133 deposits (141,919.61, the channel payouts credited
straight to channel sales accounts), all 6 journal entries, both vendor
credits, both refund receipts, the one credit memo. Examples: invoices
18748 and 18749; purchase 326288; bill 00383608.

Meanwhile payments and bill payments post (they need no class), so the gated
run's AR goes negative and AP positive (10/16: AR −163,407.66 against
86,283.51; AP 151,068.76 against −30,867.79). A Q1 2027 night with unclassed
documents is red on every must-tier line, not only on the rejection count.

Options:

1. **Class in QBO from 1 January 2027.** Create the W15 classes in QBO
   (`foam`, `sign`, `chair`, `drop`, `cnc`, `uv`), turn on "warn if no class"
   and set a default class on each item there. Honest, and it is how the books
   should look after cutover anyway; it changes Dan's daily QBO entry for one
   quarter, and history before 1/1/27 is unaffected (D28).
2. **Item default classes in the ledger only** (W16's fallback, which today
   never fires because QBO items carry none): a one-off item→class map for
   the items sold in Q1. Expense-account lines (purchases, bills on
   accounts, deposits) still need a rule.
3. **Extend D28's fallback class through Q1 2027.** Every Q1 line would
   carry `unclassified` and class reporting starts at cutover. Simplest; it
   gives up per-class margin for the one quarter the parallel run covers.

*Leaning:* 1 for items, with 3 as the backstop for account-only lines, but it
is Dan's daily workflow and Joel's call on whether an unclassified quarter is
acceptable.

## D34 — Items whose QBO account is not an income account

17 Oct 2026. **Question for Joel.** The posting rules (§1) credit every
item line on an invoice or sales receipt to 4100 and every shipping line to
4300, whatever account the QBO item points at. In QBO several items point at
accounts that are not income, so QBO and the ledger disagree on total income
(must tier) by **−10,322.98 at 10/16** (−1,835.95, −5,605.68, −7,909.30 at
the three month ends), and the whole difference is these items:

| QBO item's account | Window | Ledger posts | Example invoice |
|---|---|---|---|
| Shipping expense accounts (UPS, FedEx, common carrier) | 8,202.01 | 4100 income | 18748, 18752, 18751 |
| Amazon sales tax payable (a liability) | 1,708.22 | 4100 income | sales receipt 113-9627800-1294647 |
| Advertising and promotion | 331.55 | 4100 income | sales receipt 113-0085564-0209871 |
| Purchasing (a cost account), net of a discount item booked to an expense | 81.20 | 4100 income | 18782, 18761 |

The questions:

1. **Shipping charged to a customer** (W6). QBO nets it against freight
   expense; the ledger books it as income. W6 already leans income. Does Joel
   agree, and if so is the parallel-run variance "explained" under W6 rather
   than a must-tier failure, or should Dan repoint the shipping items in QBO
   to an income account for Q1?
2. **Marketplace-collected sales tax on Amazon orders.** The tax Amazon
   collects and remits shows up as an item line pointed at a liability. The
   ledger books it as revenue, which is wrong under either reading. Should
   the ledger post an item line to the item's own account when that account
   is a liability (and only then), or should Amazon receipts arrive without
   that line?
3. **Discount and purchasing items** on invoices: contra income (W9, 4900)
   or follow the item's account?

## D35 — The chart mapping lands QBO accounts on the wrong seed accounts

17 Oct 2026. **Question for Joel** (part of D28's "185 `needs_mapping`
accounts resolved"). The automatic mapping puts a QBO account on a seeded §2
account by number or type, first come first served. On this run that put:

- **2300 customer deposits ← QBO "Loan Payable"** (its QBO account number is
  2300). AR is tested as 1200+2300, so a loan balance would land in the AR
  test. Today it holds 146.51 of unapplied customer payments (W8) and the
  loan is at zero, so it ties by luck.
- **5000 cost of goods sold ← QBO "Merchant Account Fees"**, so the §7 COGS
  line compares the ledger's COGS (23,409.43) with QBO's merchant fees
  (654.23). Same for **4100 sales income ← QBO "EBAY SALES"**.
- **2200 sales tax payable ← QBO "Sales Tax Payable"** (number 2200, zero
  balance), while QBO actually keeps NJ tax in "New Jersey Division of
  Taxation Payable". Sales tax must-tier: −405.32 at 10/16, exactly the tax
  posted in the window; the two accounts together tie.
- **QBO "Discounts given" is not joined to 4900** (155.56), and QBO's
  surcharge income (987.97) and its three product sales accounts collapse into
  4100. Inside total income, so may tier.

The question: does Joel want the mapping as an explicit table he signs off
(QBO account → §2 account, one row each, the 185 included), rather than the
importer's number-and-type guess? The recommendation is yes; the tbdiff
groups are only as good as this table.

## D36 — Inventory relief: which account, at what cost

17 Oct 2026. **Question for Joel** (W12, W22). QBO relieves an inventory item
on sale into that item's own expense account (here spread across "Cost of
Goods Sold", "Purchasing", "Aegis Raw Materials" and "Purchases") at FIFO.
The ledger relieves every item into 5000 at the item's purchase cost (W12
moving average, not yet maintained by the import). Over the window the cost
accounts and inventory net to the same total within 596.00, and that 596.00
is the "Purchasing" item of D34; inside it, the ledger relieved 1,763.01 more
inventory than QBO, and QBO posted a 100.00 shrinkage adjustment the replica
does not carry (D32). Credit memos and refund receipts debit 4950 (W10),
2,287.75, where QBO debits the original income account. Examples: invoice
18725 (inventory relief), credit memo 18626 and refund receipt 6122-R (W10).

The question: are these the "explained, may-differ" variances §7 allows,
under W10, W12 and W22, so they do not count against the four-week bar? Or
should COGS follow the item's own expense account, so the may-tier split
compares like with like?

## D37 — Mirror and post Transfer, CreditCardPayment, InventoryAdjustment

17 Oct 2026. Follows the practice parallel run (D32, docs/PRACTICE-RUN.md on
its branch): once posting bugs were fixed, the only must-tier bank variance
left was entities the replica never mirrored. As of 10/16/26: 135,758.35 of
CreditCardPayment, a net 2,055.98 of Transfer (checking +133,702.37, cards
−135,758.35, equity −1,000.00), and one 100.00 inventory shrinkage
adjustment. Without these, every Q1 2027 nightly run goes red on checking
and the cards.

**qbo-local.** `EntityType` and `DocumentType` gain the three, appended last
in the documents tier. CDC covers them (D25: "all API entities except" a
list that does not name them). Facts from Intuit's API reference, read
17 Oct 2026:

| Entity | Shape | Code |
|---|---|---|
| Transfer | `FromAccountRef`, `ToAccountRef`, `Amount`; no lines, no `TotalAmt` | projected total = `Amount` |
| CreditCardPayment | `BankAccountRef`, `CreditCardAccountRef`, `Amount`, optional `VendorRef`. Read and query responses wrap it as **`CreditCardPaymentTxn`** | `EntityType::response_key`; CDC accepts either name, since only the query shape is documented |
| InventoryAdjustment | `AdjustAccountRef`, lines of `ItemAdjustmentLineDetail { ItemRef, QtyDiff }`. **No amount anywhere**, and **no Query operation documented** (Create, Read, Update, Delete only) | projected total = 0; `QtyDiff` read as the line qty |

**Ledger.** No new `DocKind`:

- Transfer → `JournalEntry`, Dr `ToAccountRef` / Cr `FromAccountRef`.
- CreditCardPayment → `JournalEntry`, Dr `CreditCardAccountRef` /
  Cr `BankAccountRef`.
- InventoryAdjustment → `InventoryAdjustment`, the §1 row: each line is
  valued at `QtyDiff × item PurchaseCost`, against the item's mapped asset
  account (1300/1310) and 5150. `post_inventory_adjustment` now posts every
  line (it took one) and takes the line's class before the header's. A line
  it cannot value refuses the whole document, never a partial post.
- An Expense/COGS account named "shrinkage" or "inventory adjustment" now
  maps to 5150, so QBO's own Inventory Shrinkage account lands where §1
  posts. Any other `AdjustAccountRef` posts to 5150 anyway, with a warning.

**Open, for the first live sync (Dan; a rebuilt qbo-local triggers the
keychain prompt, D26):**

1. Whether `SELECT * FROM InventoryAdjustment` works. If QBO refuses it, the
   sweep fails for that type only (it syncs last), and the fix is a CDC-only
   path for it: CDC reaches back 30 days, enough for the Q1 2027 replay but
   not the Q3 2026 practice window.
2. Valuation. QBO values an adjustment itself, and the payload doesn't say at
   what cost. The 100.00 should tie if the item's `PurchaseCost` matches
   QBO's cost; if not, tbdiff shows the difference on 1300/1310 and 5150, and
   the source becomes the InventoryValuationDetail report.
3. Classes. QBO adjustments carry no `ClassRef`, so Q1 2027 lines need an
   item default class or are refused, the same rule as every other Q1 line
   (D29).

*Rejected: new `Transfer`/`CreditCardPayment` doc kinds.* A two-line journal
posts the same entry. New variants would spread into every exhaustive
`DocKind` match, the desktop app's included, and buy nothing until the own
books need their own transfer form.
