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
