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

## D14 — M2 write path: what was decided while building it

18 Oct 2026, in build. Writes exist in code and stay off: no realm has them on.

- **The gate is enforced in code, not only by a disabled button.** `writes::check_enable`
  allows a sandbox company at any time; a production company only 14 days after its
  first complete sweep, only with `--confirm` (the explicit go), and only while no
  other production company has writes on. `queue_*` and `drain_once` refuse while a
  realm's writes are off, so the API returns 403 rather than queueing something that
  will never be sent.
- **Realms carry their environment** (migration 3). A replica cannot be pointed at the
  other Intuit host than the one it was first synced from; the CLI refuses.
- **A human "discard" state.** §6 said failed writes are never silently dropped. They
  still are not: `Discarded` is reachable only from `conflicted` or `rejected`, only by
  a person in the sync queue, and the record is kept.
- **Conflict retry re-bases, and only on the newer version.** Retrying a conflicted
  update takes the replica's current `SyncToken`, and is refused until the replica has
  actually picked up the other edit — so the person retrying has seen it. This is the
  opposite of last-writer-wins: nothing is overwritten that nobody looked at.
- **Updates are sparse.** QuickBooks changes only the fields sent, so fields the editor
  does not know about survive an edit.
- **Tax is QuickBooks'.** The editor sends a tax code per line and shows a pre-tax
  subtotal; the replica shows QuickBooks' tax once the document lands. Line amounts are
  computed once, with `LineExtension` rounding, and the UI uses the same rule in exact
  integer arithmetic so the amount on screen is the amount sent.
- **The customer PO goes in the book's own custom field**, whose `DefinitionId` is read
  from existing documents. A PO with no such field is refused, never dropped.
- **Queued creates are shown, not mirrored.** A saved document lists in its register and
  opens from its outbox payload, marked "Not yet in QuickBooks"; nothing is written to
  the replica's mirror until QuickBooks has it. A page left open on a queued document
  follows it to its real id when it lands.
- **The dev server refuses cross-site requests** now that it can queue writes: JSON
  content type required (a form post or `no-cors` fetch cannot set it without a
  preflight), and a browser `Origin` must be its own.
- **The drain runs in the sync process.** The desktop app only queues; `sync --watch`
  drains before each poll. Both open the same file, so the store waits up to 5 s on the
  other's lock rather than failing a save.

## D15 — Manufacturing costing: what was decided while building it

18 Oct 2026, in build. Phase 2's first slice: landed cost, build sheets, builds.

- **The replica's own data, outside the write gate.** Materials, deliveries, build sheets
  and builds live in tables QuickBooks never sees (migration 4). Recording them changes
  nothing in the book of record, so they are usable today, before any QuickBooks write
  is switched on.
- **Freight follows board-feet, to the cent.** Shares are allocated by largest remainder,
  so the freight in the costs always equals the freight on the bill.
- **A material's cost is its most recent delivery's landed $/bf.** Simple, explainable
  and matches how a quote is priced. A moving average over stock on hand needs
  quantities on hand, which this slice does not track. Recorded here because it is the
  first thing to revisit when inventory quantities arrive.
- **Yield divides.** Net board-feet over yield is what is bought; the difference is priced
  as waste and shown, not buried.
- **Rates carry four places; sums of money are rounded once.** $/bf and unit costs are
  rates, kept to four places; a run's standard and variance are money, rounded to the
  cent at the end.
- **A build keeps the sheet it was measured against.** Editing a build sheet later does
  not rewrite a past run's standard.
- **Nothing is deleted.** A wrong delivery is corrected by recording another.
- **No posting.** Builds are costed, not journalled. Moving inventory value between raw
  material and finished goods is ledger work (Phase 3) and the posting rules there need
  your CPA's review first.

### D15 addendum — stock on hand (21 Oct 2026)

- **Stock is replayed, never stored.** On-hand board-feet, the moving average and shelf
  value are computed from deliveries, builds and counts in date order each time they are
  read. A corrected record changes the history it belongs to; there is no running
  balance to drift.
- **Same-day order:** deliveries, then the day's cutting, then a count taken at the end of
  the day.
- **A count sets the shelf;** the difference is shrinkage, priced at the average.
- **Cutting past zero is flagged, not hidden.** A delivery or count is missing; the next
  delivery fills the gap at its own price rather than averaging against a hole.
- **Build sheets cost on the latest delivery by default** (how a quote is priced), with the
  shelf average a per-viewer choice. When nothing is on the shelf the average means
  nothing, so the latest stands in.

### D15 addendum — finished goods and spreadsheet import (21 Oct 2026)

- **A product's stock starts at its first count or first build.** Sales before that are
  not replayed: nothing before it says how many were on the shelf, so the history would
  start negative and mean nothing.
- **Builds come in at their actual cost; invoices and sales receipts take units off;
  credit memos and refund receipts put them back** at the average.
- **An opening count is valued at the build sheet's cost** (latest delivery prices) when
  no build has set an average yet, rather than at zero.
- **Import is preview, then commit, all or nothing.** Every row is checked and every
  problem reported by line before anything is written; the commit runs inside one
  savepoint, so a file never lands half in.

### D14 addendum — receiving payments (22 Oct 2026)

- **A payment is spread oldest due first** unless a person types the split; nothing goes
  past an invoice's balance or past the amount received. What is left over stays on the
  customer as a credit, shown before saving.
- **No deposit account is named.** QuickBooks puts the payment in Undeposited Funds, as its
  own Receive Payment screen does by default; grouping into a bank deposit stays in
  QuickBooks until deposits are built here.
- **Queued payments count against open balances.** An invoice already being paid by a
  payment on its way is shown with that amount netted off, so the same cheque cannot be
  applied twice before the first lands.
- **Only invoices QuickBooks already holds can be paid.** A queued invoice has no tax yet,
  so its balance is not known until it lands.
