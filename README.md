# ledger-workspace

Aquamentor / WaterLine CNC accounting build.

Two projects, one workspace:

- **`apps/qbo-local`** — the near-term project. A complete local SQLite replica
  of QuickBooks Online plus a fast UI over it. QBO stays the book of record; this
  removes the network from the interaction path so every read is a local query
  and every write commits locally and syncs in the background.
- **`apps/ledger`** — the long-term project. A double-entry general ledger that
  eventually replaces QBO. Stub only; not started.
- **`crates/ledger-core`** — money type and rounding policy, shared by both.
  Deliberately nothing else — see `DECISIONS.md` D3.

## Documents

| File | What it is |
|---|---|
| `DESIGN.md` | The `qbo-local` design. Replica schema, sync, token rotation, outbox, reconciliation, safety. |
| `DECISIONS.md` | Running log of review decisions and the reasoning behind them. |
| `HANDOFF.md` | What to build next to finish M0, and the constraints that go with it. |
| `docs/CLAUDE_CODE_KICKOFF_PROMPT.md` | Original brief for the ledger project. |
| `docs/CLAUDE_CODE_PROMPT_2_QBO_CACHE.md` | Original brief for `qbo-local`. |

`LEDGER-DESIGN.md` does not exist yet. It gets written when the ledger project
starts, and leads with the posting-rules table — document type to debit and
credit effects — because that encodes accounting policy that needs review rather
than engineering judgement.

## Build

```sh
cargo test          # 247 tests, all offline
cargo clippy --all-targets
cargo run --bin qbo-local

# The UI against a synthetic book — no Intuit, no real data
cargo run --release --bin qbo-local -- demo --db /tmp/demo.db --invoices 9000
(cd apps/qbo-local/ui && npm install && npm run build && npm test)
cargo run --release --bin qbo-local -- serve --db /tmp/demo.db   # http://127.0.0.1:8780/

# Desktop shell (own workspace; needs a webview toolchain — Xcode CLT on macOS)
(cd apps/qbo-local-desktop && cargo install tauri-cli --version "^2" && cargo tauri build)

# Live commands (need Intuit credentials; see HANDOFF.md §1.1)
cargo run --features live --bin qbo-local -- authorize
cargo run --features live --bin qbo-local -- sync --realm <id>
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
- HTTP client behind the trait: request building, Intuit fault mapping,
  one reactive token refresh on 401; real transport behind the `live` feature
- OAuth: authorize URL, loopback callback with state check, code exchange,
  refresh with generation fallback and persist-before-use
- Reconciliation sweep (§7): missing/stale healed, extras quarantined never
  deleted, orphaned-line check

- macOS keychain token store via `security -i` (secret never on argv)

- M1 read-only UI ("BunzBooks"): React registers for every document type
  (virtualized, sortable, filterable, keyboard-driven), document view with
  lines and the estimate→invoice→payment / PO→bill→payment chain, contact
  view, `⌘K` palette with match reasons and local timing, sync state in the
  chrome, light/dark, text scaling. Served by `qbo-local serve` or the Tauri
  shell in `apps/qbo-local-desktop`; both call one `api::dispatch`
- Synthetic book generator (`demo`) for UI work and §11 measurements
- §8 safety nets for M2: `AuditedClient` appends every QBO write (request,
  `RequestId`, result) to an fsynced JSONL file; `snapshot` writes a dated
  `VACUUM INTO` copy of the replica, outbox included, keeping the newest N

Not built yet: writes from the UI (M2, gated), the trial-balance diff (§7.5,
ledger territory), the Tauri shell, the React UI.

The first live authentication run should happen wherever the OAuth credentials
already live, rather than moving them onto another machine.

## Two things not to do later

**Never put the replica database in Dropbox.** Design for sync via the outbox,
not by syncing the file. A SQLite file in a folder-sync service is how you get a
corrupted database and a conflicted copy.

**Never regenerate a `RequestId` on retry.** It is generated once when the outbox
record is created and reused on every attempt. A fresh one per attempt gives no
idempotency at all, which is how a network timeout becomes a duplicate invoice.
