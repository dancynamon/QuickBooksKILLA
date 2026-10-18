# HANDOFF — finishing `qbo-local` M0 on Dan's machine

Written for a Cowork session on the machine where the OAuth credentials already
live (`~/code/aquamentor-mcp`, under `qbo_headless`). The credentials are the
only reason this work moves machines — everything else is already done and
tested here.

**Read `DESIGN.md` first.** This document assumes it.

---

## 0. Prerequisites — have these in hand before starting

QBO is OAuth 2.0, not a simple API key. An Intuit developer app gives you a
**Client ID** and **Client Secret**; those cannot call the API on their own. They
have to complete an authorization round trip first — browser to Intuit's consent
page, approval, redirect back to a registered URI — which yields the access and
refresh tokens the client actually uses.

| Need | Notes |
|---|---|
| Client ID + Client Secret | From the Intuit developer app |
| Registered redirect URI | `http://localhost:PORT/callback`, registered on the app; must match exactly |
| Realm id | Not recorded here — read it from the QBO deep link on any invoice, or from `qbo_headless`. See below. |
| Sandbox company | Only needed once write paths are built (M2). M0 is read-only and can point at production. |

**Check first:** if `~/code/aquamentor-mcp` / `qbo_headless` is already authorized,
it may hold a valid refresh token. Reusing it skips the consent flow entirely for
now. Confirm before building the authorization flow — it may not be on M0's
critical path.

**Getting the realm id.** It is deliberately not written down in this repository.
Two ways to read it on the machine that has access: it is the
`deeplinkcompanyid` parameter on the QBO web link of any invoice, and it is
already stored wherever `qbo_headless` keeps its authorization. Put it in the
local config the app reads, not in a tracked file — `.local/` is gitignored in
full and is the right home for it.

Whatever the source, tokens land in the macOS keychain via the `TokenStore`
backend in §2.1. Never in the repo, never in a dotfile Dropbox syncs.

---

## 1. Where the work stands

`cargo test` — 280 tests, all offline, all green. `cargo clippy --all-targets` —
clean.

Built and tested:

| Module | What it holds |
|---|---|
| `crates/ledger-core` | `Money` (i64 minor units, checked), `round_money` |
| `domain` | `RealmId`, `EntityType`, sync tiers |
| `store` | SQLite schema, migrations, realm-scoped access |
| `sync` | CDC planning: staleness fallback, truncation handling |
| `auth` | Token generations, atomic persistence, lockout detection |
| `ratelimit` | Per-realm token buckets, jittered backoff |
| `outbox` | State machine with enforced transitions |
| `client` | The QBO trait boundary + `MockQbo` double |
| `worker` | Drain loop, dependency resolution, local-id rewriting |
| `http` | `HttpQboClient`: request building, fault-code mapping, 401 → one refresh + resend; `live::UreqTransport` behind `--features live` |
| `oauth` | Authorize URL, loopback callback (state-checked), code exchange, `TokenManager` (persist-before-use, generation fallback on `invalid_grant` only, JSONL rotation log) |
| `driver` | Initial sweep, CDC poll, truncation backfill; `reconcile` (§7): heal missing/stale, quarantine extras, orphaned-line check |
| `keychain` | `KeychainTokenStore`: one login-keychain item per realm (service `qbo-local`), written via `security -i` with hex `-X` so the token is never on argv; default on macOS, `--file-tokens` for the 0700 file store |

| `api` | The UI's read-only command surface; one `dispatch` behind both front doors |
| `serve` | Loopback dev server over `api` for browser testing |
| `demo` | Seeded synthetic book; no real names, `.invalid` emails |
| `safety` | §8: `AuditedClient` write audit (JSONL, fsynced, failure is transient so the retry replays), dated snapshots with rotation that only touches its own `snapshot-YYYY-MM-DD.db` files |
| `ui/` | React UI (registers, document + chain, contact, `⌘K` palette, status bar) |
| `../qbo-local-desktop` | Tauri 2 shell: one `api` command, replica from `QBO_LOCAL_DB` or the app home |

| `writes`, `compose`, `outbox_store` | M2: durable outbox, drain, gate, editor payloads (DECISIONS D14) |

**Not built:** the §7.5 trial-balance diff. M0 code is complete; what remains is
the live run. M2 is built and off.

### 1.3 Turning writes on (after the soak)

```sh
qbo-local writes --db <replica> --realm <id>                     # state, and what enabling needs
qbo-local writes --db <replica> --realm <id> --enable --confirm  # production: 14 days after first sync
cargo run --release --features live --bin qbo-local -- sync --realm <id> --watch 30   # drains, then mirrors
```

Sandbox first: sync a sandbox company with `--sandbox` (its realm is recorded as
sandbox and can take writes at once), use BunzBooks against it, then repeat for
production after the soak. Every write is appended to `audit-<realm>.jsonl` in
the app home. `--disable` turns writes off; anything queued stays queued.

### 1.2 Running the UI on the Mac

After `sync` has written `replica-<realm>.db` to the app home:

```sh
cd apps/qbo-local/ui && npm install && npm run build
cd ../../qbo-local-desktop && cargo install tauri-cli --version "^2" --locked
cargo tauri dev        # hot reload; or `cargo tauri build` for BunzBooks.app
```

The shell opens the first `replica-*.db` in the app home, or `QBO_LOCAL_DB`.
A bare `cargo build --release` produces a dev-mode binary that looks for the
Vite server; build through `cargo tauri build`, or add
`--features tauri/custom-protocol`. Verified on Linux under Xvfb against the
synthetic book: 9,000-row register over IPC in ~200 ms with software rendering.
It is read-only by construction: `api::dispatch` has no write command.

### 1.1 Running M0 on the Mac

```sh
export QBO_CLIENT_ID=...  QBO_CLIENT_SECRET=...      # from the Intuit app; never in a file in the repo
cargo run --release --features live --bin qbo-local -- authorize            # redirect URI http://localhost:8765/callback must be registered
cargo run --release --features live --bin qbo-local -- sync --realm <id>    # initial sweep; prints per-type counts and wall clock
cargo run --release --features live --bin qbo-local -- sync --realm <id> --watch 300   # CDC poll loop
cargo run --release --features live --bin qbo-local -- reconcile --realm <id>         # nightly / on-demand verification
```

App home is `~/Library/Application Support/qbo-local` (override with
`QBO_LOCAL_HOME`; any path containing "dropbox" is refused). It holds
`rotation-log.jsonl` and `replica-<realm>.db`; tokens go to the login keychain.

**Unverified mappings in `http.rs`, check against the first live run:** fault
codes 5010 (stale), 610 (not found), 3200 (auth), 003001 (throttle);
`ORDERBY MetaData.LastUpdatedTime` tie ordering under `STARTPOSITION` paging;
CDC response nesting `CDCResponse[].QueryResponse[].<Entity>[]`. Each is one
constant or one function; the tests pin today's assumption.

---

## 2. What to build, in order

### 2.1 `KeychainTokenStore` — BUILT (`keychain.rs`); verify on the Mac that the first save raises no keychain prompt loop

Implement the existing `auth::TokenStore` trait against the macOS keychain.

```rust
pub trait TokenStore {
    fn load(&self, realm: &RealmId) -> Result<Option<TokenGenerations>, AuthError>;
    fn save(&self, generations: &TokenGenerations) -> Result<(), AuthError>;
}
```

`FileTokenStore` already implements it and is the reference for behaviour —
including the `write → fsync → rename → fsync directory` sequence in
`auth::atomic_write`. Keep `TokenGenerations` as the stored unit: current plus
two previous generations in one value, so a rotation is a single atomic write
rather than several files that can disagree after a crash.

Do not drop the generation retention. It exists for the case where the process
dies between Intuit invalidating the old refresh token and the new one reaching
disk, and that case is the difference between a hiccup and re-authorising by
hand at 3am.

### 2.2 OAuth authorisation flow — BUILT (`oauth.rs`)

One-time, per realm. Local loopback redirect. Store the result via 2.1.

`auth::TokenSet::needs_refresh` already implements proactive refresh at ~50
minutes on a 60-minute token; wire it into the client so rotation happens while
idle rather than mid-batch. Reactive refresh on a 401 stays as the fallback.

Every rotation appends an `auth::RotationLogEntry` to a JSONL file. Log the
fingerprint, never the token — `TokenSet::fingerprint()` already does the right
thing.

### 2.3 `HttpQboClient` — BUILT (`http.rs`)

Implement `client::QboClient`. The trait is the seam — nothing above it changes.

```rust
pub trait QboClient {
    fn query(&mut self, realm, entity_type, start_position, max_results)
        -> Result<Vec<EntityPayload>, QboError>;
    fn cdc(&mut self, realm, entity_types, changed_since)
        -> Result<Vec<EntityPayload>, QboError>;
    fn create(&mut self, realm, entity_type, payload, request_id)
        -> Result<EntityPayload, QboError>;
    fn update(&mut self, realm, entity_type, qbo_id, payload, base_sync_token, request_id)
        -> Result<EntityPayload, QboError>;
    fn find_by_name(&mut self, realm, entity_type, name)
        -> Result<Option<EntityPayload>, QboError>;
}
```

Mapping obligations:

- Every call passes through `ratelimit::RealmLimiter` first. Refusal means
  wait, never proceed anyway.
- HTTP 429 and 5xx → `QboError::RateLimited` / `QboError::Network`. These are
  the only variants `is_transient()` returns true for, and that flag is what
  the worker uses to decide retry versus failure inbox.
- A stale-`SyncToken` response → `QboError::StaleSyncToken`, nothing else. The
  worker routes that straight to `conflicted`, bypassing the retry budget, and
  that behaviour is deliberate: retrying a stale token is last-writer-wins by
  another name.
- Everything else QBO refuses → `QboError::Validation`.
- Reuse `record.request_id` on retries. Never regenerate it.

`MockQbo` in `client.rs` is the behavioural spec. If the real client and the
mock disagree about any of the above, the mock is what the worker's 20-odd
tests were written against — reconcile deliberately, don't just change the mock.

### 2.4 Initial sync driver — BUILT (`driver.rs`)

Per realm, walk `EntityType::m0_scope()` — masters before documents, which the
ordering in `EntityType::ALL` already guarantees and a test enforces.

Page with `STARTPOSITION` / `MAXRESULTS` at 1000 rows. Write each page through
`Store::upsert_entity` and advance `sync_cursors` **in the same transaction** as
the entities it covers. A cursor advanced outside that transaction can skip
changes after a crash.

Report actual row counts and wall-clock time. The brief is explicit that these
are measured and reported, never asserted. Expect roughly nine thousand invoices
and tens of thousands of documents in total, going back to 2012.

### 2.5 CDC poll loop — BUILT (`driver.rs`, `sync --watch`)

15 seconds focused, 5 minutes idle, both configurable.

`sync::plan_sync` already makes the incremental-versus-sweep decision, and
`sync::classify_response` plus `sync::halve_window` handle truncation. Wire them
up rather than reimplementing:

- A response at the 1000-object cap is **possibly truncated**. Halve the window
  and re-poll. Do not advance the cursor — `SyncCursor::advance` takes the
  outcome specifically so advancing on a truncated response isn't expressible.
- A cursor older than 25 days falls back to a full sweep. CDC does not error
  past its 30-day window; it silently under-reports, which is why the margin
  exists.
- `halve_window` returning `None` means the window can't narrow further — fall
  back to a sweep rather than looping.

---

## 2.6 Test fixtures from the real book

Recorded HTTP fixtures are what let the suite run offline, and the honest ones
come from real responses rather than hand-written guesses. Pull them into
`.local/fixtures/` — gitignored in full — and keep the committed fixtures
synthetic.

The rule that matters: **a fixture that reaches `git add` must not contain a
real customer, vendor, balance or realm id.** Scrub on the way in, not on the
way out. This repository has already had to be cleaned once, and finding real
names in a test file after the fact is much more work than inventing them up
front.

---

## 3. Constraints that must not be relaxed

These come from the kickoff brief and are not open for local optimisation.

1. **Read-only until explicitly enabled, per realm.** `realms.is_write_enabled`
   defaults to 0 and `Store::set_write_enabled` is the only way to change it.
   M0 does not enable it for anything.
2. **Two full weeks of read-only daily use** on production before any write path
   is turned on, then only on Dan's explicit go, then one realm at a time. Do
   not compress this and do not ask to.
3. **Sandbox first** for anything write-shaped.
4. **No delete or void path.** Destructive operations happen in QBO's web UI.
5. **Secrets never in the repo.** Keychain only. `.gitignore` covers the obvious
   shapes but it's a backstop, not permission to be casual.
6. **Never put the replica DB in Dropbox.** Sync via the outbox, not the file.
7. **The test suite stays fully offline.** Record fixtures; never let CI or
   `cargo test` require Intuit.

---

## 4. Verify these against Intuit's live docs

`developer.intuit.com` was blocked by network policy in the session that wrote
this, so these came from secondary sources and are marked ⚠️ in `DESIGN.md` §0.
Confirm each — a Cowork session on Dan's machine can reach the docs.

| Claim | Used where | If wrong |
|---|---|---|
| Batch endpoint is 120 req/min per realm (changed Oct 2025) | `ratelimit::RealmLimits::default` | One config value |
| `RequestId` does **not** deduplicate Customer or Item | `EntityType::requires_query_before_create`, worker adopt path | If it *does* cover them, the guard becomes a harmless extra query — keep it anyway |
| CDC entity exclusion list | `plan_sync`'s `coverage_confirmed` argument | Pass `false` for uncovered types; the sweep path is already the correctness baseline |
| CDC lookback is 30 days | `sync::CDC_LOOKBACK_DAYS` | Adjust, keeping `DEFAULT_CDC_MAX_AGE_DAYS` below it — a compile-time assertion enforces the margin |

Record findings in `DECISIONS.md` and update `DESIGN.md` §0's confidence column.

---

## 5. Done when

From the kickoff brief's M0 acceptance, adjusted — WaterLine is out of scope
per Dan, so Aquamentor alone:

- [ ] Aquamentor fully mirrored, complete history, actual row counts reported
- [ ] Initial sync wall-clock time reported (target < 10 min, measured not claimed)
- [ ] CDC poll keeps the replica current
- [ ] Token survives an app restart
- [ ] Token survives a 24-hour idle, including a rotation
- [ ] `cargo test` still fully offline and green
- [ ] Writes still disabled for every realm

WaterLine runs the same code path with its own realm-scoped budget whenever Dan
wants it turned on — no design accommodation needed.

---

## 6. Two things that will bite

**The retried customer create.** A create reaches Intuit and succeeds, the
response is lost on the way back, the record retries. Because `RequestId`
reportedly doesn't cover Customer, a naive retry produces a second customer
master in the real book. The worker handles this by querying by `DisplayName`
first and adopting the existing record. The test is
`worker::tests::a_retried_customer_create_adopts_rather_than_duplicating`. If
you refactor the drain path, that test is the one to keep passing.

**Cursor advance on a truncated CDC response.** Advancing to the newest returned
record silently drops everything the response didn't include, and nothing
detects it until a reconciliation sweep months later. `SyncCursor::advance`
takes a `CdcOutcome` rather than a bare timestamp specifically to make that
mistake unrepresentable. Don't add a convenience method that takes the timestamp
directly.
