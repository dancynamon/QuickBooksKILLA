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

## D15 — QBO Payments is in use, so payments are a hard decoupling blocker

11 Sep 2026, Dan.

Customer payments run through QBO Payments today. Until a replacement processor
or payment-link path exists in the own system, invoices have to keep reaching
QBO, which means the outbox export shim (ROADMAP §C) cannot be switched off at
cutover even if every other blocker is clear. This goes on the 1 November
go/no-go list as a hard item, not a nice-to-have.

## D16 — Cutover target 1 January 2027, fallback 1 January 2028

11 Sep 2026, Dan.

Fiscal year is calendar year (D7), so cutover is a year boundary. 1/1/27 is the
target; the go/no-go is 1 November 2026 against the bar in ROADMAP §F; missing
any item moves the date to 1/1/28 without argument. The bar is not shortened to
hit the date.
