# HANDOFF — finishing `qbo-local` M0 on Dan's machine

Written for a Cowork session on the machine where the OAuth credentials already
live (`~/code/aquamentor-mcp`, under `qbo_headless`). The credentials are the
only reason this work moves machines — everything else is already done and
tested here.

**Read `DESIGN.md` first.** This document assumes it.

---

## 1. Where the work stands

`cargo test` — 128 tests, all offline, all green. `cargo clippy --all-targets` —
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

**Not built:** the HTTP transport behind `QboClient`, the OAuth authorisation
flow, the keychain token backend, the initial-sync driver, the CDC poll loop,
the reconciliation sweep, the Tauri shell, the React UI.

M0 needs the first five of those. The last two are M1.

---

## 2. What to build, in order

### 2.1 `KeychainTokenStore`

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

### 2.2 OAuth authorisation flow

One-time, per realm. Local loopback redirect. Store the result via 2.1.

`auth::TokenSet::needs_refresh` already implements proactive refresh at ~50
minutes on a 60-minute token; wire it into the client so rotation happens while
idle rather than mid-batch. Reactive refresh on a 401 stays as the fallback.

Every rotation appends an `auth::RotationLogEntry` to a JSONL file. Log the
fingerprint, never the token — `TokenSet::fingerprint()` already does the right
thing.

### 2.3 `HttpQboClient`

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

### 2.4 Initial sync driver

Per realm, walk `EntityType::m0_scope()` — masters before documents, which the
ordering in `EntityType::ALL` already guarantees and a test enforces.

Page with `STARTPOSITION` / `MAXRESULTS` at 1000 rows. Write each page through
`Store::upsert_entity` and advance `sync_cursors` **in the same transaction** as
the entities it covers. A cursor advanced outside that transaction can skip
changes after a crash.

Report actual row counts and wall-clock time. The brief is explicit that these
are measured and reported, never asserted. Expect roughly roughly nine thousand invoices and
tens of thousands of documents total for Aquamentor, back to March 2012.

### 2.5 CDC poll loop

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
