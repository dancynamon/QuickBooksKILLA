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

`cargo test` — 498 tests, all offline, all green. `cargo clippy --all-targets` —
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
| `driver` | Initial sync, CDC poll, truncation backfill (D12, D13) |
| `reconcile` | Verification sweep against QBO's index (DESIGN §7 steps 1-4) |
| `daemon`, `clock` | Cadence loop around the driver, backoff, nightly snapshots |
| `store::query` | Read-only query API for the UI and the ledger import |
| `http`, `oauth`, `config` | Live transport, OAuth loopback flow, `.local/config.toml` (§2.2, §2.3) |

**Not built:** the macOS keychain token backend (§2.1 — `FileTokenStore` is
the store everywhere else, `--live` included), wiring the daemon into a Tauri
shell and its front-end.

The keychain backend is the one item left from M0's original list. The Tauri
shell is M1. `ROADMAP.md` has the phases.

**Built in the cloud on 13 Sep** — `apps/qbo-local/src/http.rs`
(`HttpQboClient`, implementing `client::QboClient` over real HTTP),
`apps/qbo-local/src/oauth.rs` (the authorization-code loopback flow:
`run_loopback`, `exchange_code`, `refresh`), `apps/qbo-local/src/config.rs`
(`.local/config.toml`, templated by `.local/config.example.toml` at the repo
root), and `--live` on `qbo-local daemon` / `qbo-local sweep` plus the new
`qbo-local auth` subcommand in `main.rs`. Proven offline against a hand-rolled
fake Intuit server (`apps/qbo-local/tests/support/mod.rs`) — `tests/http_client.rs`
covers every mapping in §2.3 below plus `SyncDriver` running end to end over
HTTP, `tests/oauth.rs` covers the loopback and token exchange. `developer.intuit.com`
was still blocked in this session, so the §0 and §4 confidence levels are
unchanged — verify those against Intuit's live docs before M0 is called done.

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

### 2.2 OAuth authorisation flow — built; run it

`oauth.rs`: `OAuthConfig::intuit`, `authorize_url`, `run_loopback` (a one-shot
loopback listener), `exchange_code`, `refresh`. `TokenSource` in `http.rs`
wires `TokenSet::needs_refresh` into every `HttpQboClient` call — proactive
refresh at ~50 minutes on a 60-minute token, reactive refresh on a 401 as the
fallback — rotates `TokenGenerations`, saves through `TokenStore`, and appends
a fingerprint-only `auth::RotationLogEntry` to the configured JSONL log.
Tested offline against a fake token endpoint in `tests/http_client.rs` and
`tests/oauth.rs`.

Run it once per realm, after copying `.local/config.example.toml` to
`.local/config.toml` and filling in the client id/secret and realm id (§0):

```
qbo-local auth --realm <REALM_ID>
```

This opens the consent URL (paste it into a browser by hand — the flow
doesn't launch one for you), waits for the loopback redirect, and saves
generation zero through `FileTokenStore`. The keychain backend (§2.1) is a
drop-in replacement for `FileTokenStore` whenever it lands; nothing above
`TokenStore` changes.

### 2.3 `HttpQboClient` — built; run it

Implements `client::QboClient` exactly as this section originally specified —
the full mapping (rate limiting, HTTP-status-to-`QboError`, `RequestId`
reuse) now lives in `apps/qbo-local/src/http.rs`'s module doc, and
`tests/http_client.rs` proves it against a hand-rolled fake Intuit server,
one behaviour per test: paging and the 1-based
`STARTPOSITION` conversion, CDC's `Deleted` status, `RequestId` replay on a
retry after a 500, a stale-`SyncToken` fault, 429, a plain 400, 404, and both
the proactive and reactive refresh paths. The same test file also runs
`SyncDriver` end to end against that fake server, so the driver is proven
against HTTP and not only `MockQbo`.

`MockQbo` in `client.rs` remains the behavioural spec for anything above the
trait — if `HttpQboClient` and the mock ever disagree, reconcile deliberately
rather than just changing the mock.

Run a sweep or the daemon against a real realm once `auth` (§2.2) has saved a
token:

```
qbo-local sweep --db replica.db --realm <REALM_ID> --live
qbo-local daemon --db replica.db --realm <REALM_ID> --live --once
```

Both default to `.local/config.toml`; pass `--config PATH` to point elsewhere.
`--mock` still works everywhere it did before, for exercising the loop without
credentials at all.

### 2.4 Initial sync driver — built (`driver.rs`, D12, D13)

The section below is kept as the specification the driver was built to; the
live-sync step that remains is running it against `HttpQboClient` and reporting
the measured counts and wall-clock.

Per realm, walk `EntityType::m0_scope()` — masters before documents, which the
ordering in `EntityType::ALL` already guarantees and a test enforces.

Page with `STARTPOSITION` / `MAXRESULTS` at 1000 rows. Write each page through
`Store::upsert_entity` and advance `sync_cursors` **in the same transaction** as
the entities it covers. A cursor advanced outside that transaction can skip
changes after a crash.

Report actual row counts and wall-clock time. The brief is explicit that these
are measured and reported, never asserted. Expect roughly nine thousand invoices
and tens of thousands of documents in total, going back to 2012.

### 2.5 CDC poll loop — built (`daemon.rs`), not yet wired into a binary

`Daemon::run` loops `SyncDriver::sync_realm` on a focused/idle cadence with
backoff, and takes the nightly snapshot. What remains is the binary that
constructs it with the real client and keychain store.

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

**The three commands, in the order a real recording session uses them:**

1. `qbo-local record --db PATH --realm ID --dir .local/fixtures/<realm> --live`
   — records every response a full sweep makes into `--dir`, via
   `fixture::RecordingQbo` wrapping `HttpQboClient`. Errors are recorded too
   (`{"error": "..."}`) and still raised, so a recording session fails exactly
   the way a live sync would. `--mock` records from an empty `MockQbo`
   instead — it never touches the network and proves the plumbing, which is
   what `qbo-local`'s own CLI tests use it for; it is not how real fixtures
   get made.
2. `python3 tools/scrub-fixtures.py .local/fixtures/<realm> <scrubbed-dir>` —
   the only path from a recording to something safe to `git add`. Writes a
   fresh directory (never in place), deterministically, and refuses to write
   anything at all if its own leak detector finds an original name surviving
   somewhere in the output — the backstop the rule above depends on. Read the
   module docstring for the exact field rules (names, addresses, phone,
   email, `DocNumber`'s offset, `realmId`).
3. `qbo-local replay --db PATH --realm ID --dir <scrubbed-dir>` — syncs a
   realm entirely from a fixture directory, via `fixture::FixtureQbo`, touching
   no network at all. This is what the offline suite exercises against —
   `apps/qbo-local/tests/fixtures/synthetic/` is a small committed example,
   and `apps/qbo-local/src/fixture.rs` has the property test proving that
   replaying a recording reproduces a direct sync byte-for-byte, entity by
   entity.

**Only the scrubbed directory is ever `git add`ed.** `.local/fixtures/` stays
gitignored in full (§0), same as every other extract of the real book — one
ignored directory rather than a list of filenames someone will forget to add
to. The leak detector inside `tools/scrub-fixtures.py` is the backstop for
this rule, not a replacement for following it: it fails the whole run, before
anything is written, the moment a real name survives scrubbing anywhere in
the output.

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
