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
cargo test          # 672 tests, all offline
cargo clippy --all-targets
cargo run --bin qbo-local -- --help
cargo run --bin ledger -- --help

# The UI against a synthetic book — no Intuit, no real data
cargo run --release --bin qbo-local -- demo --db /tmp/demo.db --invoices 9000
(cd apps/qbo-local/ui && npm install && npm run build && npm test)
cargo run --release --bin qbo-local -- serve --db /tmp/demo.db   # http://127.0.0.1:8780/
# ... or with writes landing in a stand-in QuickBooks (synthetic book only)
cargo run --release --bin qbo-local -- serve --db /tmp/demo.db --demo-writes

# Desktop shell (own workspace; needs a webview toolchain — Xcode CLT on macOS)
(cd apps/qbo-local-desktop && cargo install tauri-cli --version "^2" && cargo tauri build)

# Live sync goes through `qbo-local auth` / `daemon --live`, which read
# BunzBooks' own Intuit app only (DECISIONS D43)
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

- `HttpQboClient` over real HTTP, the OAuth loopback flow, `.local/config.toml`
  and `--live`, proven against a fake Intuit server; fixture record/replay with
  a deterministic scrubber; an exactly-once chaos test across process kills;
  CI with thresholded benchmarks; the `qbo-local-mcp` and `ledger-mcp` servers
  over a shared stdio crate

- `apps/desktop`: the M1 read UI split out of the prototype as ES modules
  over a provider layer (fixture data in a browser today, Tauri `invoke`
  on the Mac), a commands crate that is the whole Tauri command body, and the
  Tauri v2 scaffold, compiled on the Mac only

- M1 read-only UI ("BunzBooks"): React registers for every document type
  (virtualized, sortable, filterable, keyboard-driven), document view with
  lines and the estimate→invoice→payment / PO→bill→payment chain, contact
  view, `⌘K` palette with match reasons and local timing, sync state in the
  chrome, light/dark, text scaling. Served by `qbo-local serve` or the Tauri
  shell in `apps/qbo-local-desktop`; both call one `api::dispatch`
- Read-only reports and lists: A/R and A/P aging (as-of date, five buckets),
  sales by customer (line amounts before tax, net of credits), products and
  services with price, cost, margin, on-hand and where-used; a statement
  strip (open balance by days past due) on every customer and vendor
- Synthetic book generator (`demo`) for UI work and §11 measurements
- §8 safety nets for M2: `AuditedClient` appends every QBO write (request,
  `RequestId`, result) to an fsynced JSONL file; `snapshot` writes a dated
  `VACUUM INTO` copy of the replica, outbox included, keeping the newest N

- M2 write path, built and switched off: editor for estimates, invoices,
  sales receipts, credit memos, purchase orders and bills, new customers usable before QuickBooks has them, a sync queue to
  retry or discard what QuickBooks refused, all through the durable outbox
  and the write gate (`qbo-local writes`; DECISIONS D44). Try it end to end
  on the synthetic book with `serve --demo-writes`

- Manufacturing costing (Phase 2, first slice): materials with landed cost
  per board-foot (freight allocated by board-foot, to the cent), build sheets
  per product (material over yield, parts, labour) with margin and a
  price-by-yield sensitivity table, and builds with priced variance against
  the sheet; material stock on hand replayed from deliveries, builds and
  counts, with moving-average cost, shrinkage, reorder points and a choice
  of latest or average cost on build sheets. The replica's own data; never sent to QuickBooks (DECISIONS D45)

Not built yet: the first live sync, the Tauri shell compiled. See `ROADMAP.md` and `docs/cowork-mac-session/`.

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

- §11 manufacturing costing: landed cost allocated by board-foot, build
  sheets with yield as a divisor, builds with priced variance, sensitivity,
  and the three posting rows that were unsupported (`docs/MFG.md`)
- Authorize.net settlement import with fee-aware bank matching (D15,
  `docs/AUTHNET.md`)
- `ledger verify` (invariants sweep) and `ledger nightly` (the §E parallel-run
  command, `docs/NIGHTLY.md`); `qbo-local report` pulls QBO's trial balance
  and aging through the Reports API into the CSVs `tbdiff` and the boundary
  walk read
- `tools/nightly.sh`: the nightly parallel run as one command (daemon and
  cursor checks, snapshot, replay, trial balance and aging diffs, a dated
  report, a notification only on failure) and its two launchd jobs
  (`docs/NIGHTLY.md`, "Switching it on")
- `ledger-mcp` over the shared stdio crate, and the desktop app's ledger
  screens (trial balance, P&L, balance sheet, sales tax with the ST-50
  mapping, GL, audit trail, adjustments, bank reconciliation, period close)

Not built yet: PDF exports, the Authorize.net hosted payment link, the order
queue and progress invoicing UI. `docs/BANK.md`, `docs/ACCOUNTANT.md`,
`docs/AUTHNET.md`, `docs/MFG.md` and `docs/NIGHTLY.md` describe the workflows.

The first live authentication run should happen wherever the OAuth credentials
already live, rather than moving them onto another machine.

## Two things not to do later

**Never put the replica database in Dropbox.** Design for sync via the outbox,
not by syncing the file. A SQLite file in a folder-sync service is how you get a
corrupted database and a conflicted copy.

**Never regenerate a `RequestId` on retry.** It is generated once when the outbox
record is created and reused on every attempt. A fresh one per attempt gives no
idempotency at all, which is how a network timeout becomes a duplicate invoice.
