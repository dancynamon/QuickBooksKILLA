# ledger-workspace

Aquamentor / WaterLine CNC accounting build.

Two projects, one workspace:

- **`apps/qbo-local`** — the near-term project. A complete local SQLite replica
  of QuickBooks Online plus a fast UI over it. QBO stays the book of record; this
  removes the network from the interaction path so every read is a local query
  and every write commits locally and syncs in the background.
- **`apps/ledger`** — the long-term project. A double-entry general ledger that
  replaces QBO at cutover (`ROADMAP.md` §F). Engine, posting rules, reports and
  the replica importer are built; the UI, statement import and accountant mode
  are not. Designed in `LEDGER-DESIGN.md`.
- **`crates/ledger-core`** — money type and rounding policy, shared by both.
  Deliberately nothing else — see `DECISIONS.md` D3.

## Documents

| File | What it is |
|---|---|
| `DESIGN.md` | The `qbo-local` design. Replica schema, sync, token rotation, outbox, reconciliation, safety. |
| `LEDGER-DESIGN.md` | The ledger design. Posting rules, chart of accounts, class taxonomy, double-entry schema and oplog, period close, import from the replica, parallel run, accountant mode, sales tax, bank reconciliation, manufacturing costing. First draft, 30 open policy items for Dan and the CPA. |
| `DECISIONS.md` | Running log of review decisions and the reasoning behind them. |
| `HANDOFF.md` | What to build next to finish M0, and the constraints that go with it. |
| `ROADMAP.md` | From `qbo-local` to a book of record that is not QBO: phases, 1/1/27 cutover plan with fallback, decoupling blockers, ranked risks. |
| `docs/CLAUDE_CODE_KICKOFF_PROMPT.md` | Original brief for the ledger project. |
| `docs/CLAUDE_CODE_PROMPT_2_QBO_CACHE.md` | Original brief for `qbo-local`. |

`LEDGER-DESIGN.md` leads with the posting-rules table — document type to debit
and credit effects — because that encodes accounting policy that needs review
rather than engineering judgement. It is a first draft: the ledger engine does
not start until the three ROADMAP §B2 policies and the other gating open items
in its §13 come back answered.

## Build

```sh
cargo test          # 451 tests, all offline
cargo clippy --all-targets
cargo run --bin qbo-local -- --help
cargo run --bin ledger -- --help
```

The test suite never touches the network. That is a requirement rather than a
convenience: QBO API behaviour is covered by recorded fixtures so the suite runs
anywhere, including CI with no Intuit credentials.

## Current state — M0 foundations

Built and tested:

- Money type and rounding policy (`ledger-core`)
- Realm scoping, entity taxonomy, sync tiers
- Outbox state machine with enforced transitions and dependency blocking
- Token rotation: atomic fsynced persistence, retained generations, lockout detection
- Per-realm token-bucket rate limiting with jittered backoff
- CDC cursor planning: staleness fallback, truncation handling
- SQLite replica schema with forward-only migrations, `STRICT` tables, WAL
- QBO client boundary plus an in-memory double that reproduces `SyncToken`
  conflicts, `RequestId` replay, and the Customer/Item idempotency gap
- The drain worker: per-entity ordering, dependency resolution, local-id
  rewriting, query-before-create adoption, convergence property test
- The sync driver: full sweep, CDC poll, truncation backfill through the query
  endpoint, cursor advanced only in the transaction that wrote its entities
- The reconciliation sweep: id + timestamp index diff, heal missing and stale,
  quarantine extra, never delete
- The CDC daemon loop: focused/idle cadence, jittered backoff, nightly SQLite
  snapshots with rotation
- The read-only query API over the replica: document, contact and item detail,
  AR/AP aging, open documents, sync status, classes, chart of accounts

Not built yet: the HTTP transport behind the client trait, the OAuth flow, the
keychain token backend, the Tauri shell and its front-end. See `ROADMAP.md`.

## Current state — the ledger

`apps/ledger`, built against `LEDGER-DESIGN.md`:

- The §2 chart as constants and a seed; the §3 class list
- The §1 posting-rules table as one pure function, every document kind except
  the three manufacturing rows, which wait for cutover (§11)
- The §4 schema with its invariants enforced in SQLite: one non-zero side per
  line, balanced-on-post trigger, posted entries immutable except `cleared_at`
- The §5 period gate: a closed period rejects, reopening is loud (D7)
- Trial balance, P&L by class, balance sheet, the §9 four-line sales tax
  report with the ST-50 mapping, and the §7 tiered trial balance diff
- The §6 importer: replica accounts and classes mapped onto the chart
  (unmapped ones created and flagged, never dropped), every document
  translated from raw JSON and posted through the same function the UI will
  use, idempotent and re-runnable, changed documents reversed and reposted
- Opening balances and the §6 boundary-year walk, run on scratch ledgers
  against QBO trial balance snapshots
- Per-line taxability on journal lines, so the §9 report's line E and its
  variance against the derived taxable figure exist
- §10 bank statements: Chase CSV and OFX/QFX parsers, exact and settlement
  matching, proposals that never post unconfirmed, the per-statement close
- §8 accountant mode read side: GL detail with running balances, audit trail,
  CSV export pack, the adjusting-entry request queue, reclassify as a proposal
- A `ledger` binary: `init`, `import`, `opening`, `boundary`, `tb`, `pnl`,
  `bs`, `tax`, `close`, `reopen`, `tbdiff`, `gl`, `audit`, `export`,
  `adjust`, `reclass`, `bank import|match|confirm|close|status`

Not built yet: manufacturing costing (§11), Authorize.net settlement import,
PDF exports, the UI. `docs/BANK.md` and `docs/ACCOUNTANT.md` describe the two
workflows.

The first live authentication run should happen wherever the OAuth credentials
already live, rather than moving them onto another machine.

## Two things not to do later

**Never put the replica database in Dropbox.** Design for sync via the outbox,
not by syncing the file. A SQLite file in a folder-sync service is how you get a
corrupted database and a conflicted copy.

**Never regenerate a `RequestId` on retry.** It is generated once when the outbox
record is created and reused on every attempt. A fresh one per attempt gives no
idempotency at all, which is how a network timeout becomes a duplicate invoice.
