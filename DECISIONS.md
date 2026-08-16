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

**Why:** measured against the live Aquamentor realm — ~roughly nine thousand invoices spanning
March 2012 to August 2026, with total documents across all types in the tens of
thousands. That is a few hundred MB of raw JSON and a one-time sync measured in
minutes. A recent-window replica would save almost nothing and would send lookups
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
