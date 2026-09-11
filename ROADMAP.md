# ROADMAP — where `qbo-local` goes from here

Written 11 September 2026 against commit `c56af00`. Companion to `HANDOFF.md`
(which says how to finish M0) and `DECISIONS.md` (which says why). This one says
what comes after, in what order, and which items are Dan's to decide.

---

## 0. Where it stands

`cargo test --workspace`: 184 tests, all offline, all green. `cargo clippy
--all-targets`: clean. (`README.md` still says 128; it is stale.)

Last commit 23 August. Nineteen days idle.

Everything that can be built **without Intuit credentials** has been built:

| Layer | State |
|---|---|
| Money, rounding | done, `ledger-core` |
| Realm scoping, entity taxonomy, sync tiers | done |
| SQLite replica, migrations, projection, quarantine | done |
| Reference-first search, lineage walk | done |
| CDC planning, truncation recovery through the query endpoint | done, D12 |
| Sync driver (sweep, poll, backfill, cursor-in-transaction) | done, D13 |
| Outbox state machine, drain worker, adopt-on-retry | done |
| Token generations, atomic persistence, rotation log | done, file backend only |
| Rate limiter | done |

Everything **not** built needs either Intuit credentials, Intuit's docs, or a
UI toolchain:

| Gap | Milestone | Needs |
|---|---|---|
| `KeychainTokenStore` | M0 | macOS |
| OAuth loopback flow | M0 | client id/secret, redirect URI |
| `HttpQboClient` | M0 | credentials to test against |
| First live sync, counts and wall-clock reported | M0 | credentials |
| CDC daemon loop (cadence around `SyncDriver::poll`) | M0 | nothing, buildable offline |
| Reconciliation sweep (§7) | M0 per DESIGN, listed M4 in the brief | nothing, buildable offline |
| Nightly replica snapshot, N-deep | M0 | nothing |
| Fixture recorder + scrubber (`HANDOFF` §2.6) | M0 | one live response to record |
| Verify the four ⚠️ API facts (DESIGN §0, §12) | M2 gate, M1 for item 4 | `developer.intuit.com`, blocked from the cloud sandbox |
| UI shell + read-only screens | M1 | stack decision (§3) |
| Write UI | M3 | M2 + two-week clock |
| Ledger project | later | `LEDGER-DESIGN.md` |

The **prototype has run three milestones ahead of the backend.** BunzBooks
mocks manufacturing costing, a document-generated ledger, period close, vendor
credits and progress invoicing. None of that has a Rust counterpart, and the
Rust side has never made one HTTP call to Intuit. That is the risk this roadmap
is shaped around.

---

## 1. Phase A — finish M0 (needs Dan's Mac, one Cowork session)

Everything here is specified in `HANDOFF.md` §2. Order:

1. **Verify the ⚠️ facts first** (HANDOFF §4). Thirty minutes on a machine that
   can reach `developer.intuit.com`. Two of them change code if wrong: the batch
   limit is a config value, and the query endpoint's `LastUpdatedTime` range
   support is what the truncation backfill (D12) stands on. Record in
   `DECISIONS.md`, update DESIGN §0's confidence column.
2. **`KeychainTokenStore`** against `auth::TokenStore` (HANDOFF §2.1).
   `security-framework` crate. Keep `TokenGenerations` as the stored unit.
3. **OAuth.** Check whether `qbo_headless` already holds a valid refresh token.
   If it does, import it and skip the consent flow for now (HANDOFF §0).
   Otherwise build the loopback flow (§2.2).
4. **`HttpQboClient`** (§2.3). `MockQbo` is the behavioural spec. Blocking
   transport is fine: the driver is synchronous by design, and the UI must never
   share its thread anyway.
5. **First live sync** of Aquamentor. Report rows and wall-clock, measured.
6. **CDC daemon.** A cadence loop around `SyncDriver::poll`, 15 s focused, 5 min
   idle. Plus the nightly SQLite backup with rotation, outbox included.
7. **Record fixtures** from the live responses into `.local/fixtures/`, scrubbed
   on the way in, and commit synthetic equivalents so `cargo test` gains real
   response shapes without gaining real names.

Done when `HANDOFF.md` §5 is all ticked. **The two-week read-only clock
(DESIGN §8) starts the day step 5 lands on production**, not when M1 ships.
Nothing in M1 shortens it.

### Buildable now, offline, before the Mac session

These do not need credentials and unblock Phase A rather than wait on it:

- **Reconciliation sweep** (DESIGN §7) against `MockQbo`: id + `LastUpdatedTime`
  diff, missing/extra/stale/orphaned classification, auto-heal missing and
  stale, quarantine extra. This is also the recovery path for a stale cursor and
  for uncovered entity types, which is why DESIGN puts it in M0.
- **CDC daemon loop and snapshot rotation**, generic over `QboClient`, tested
  against the mock with a fake clock.
- **Chaos test for `in_flight` recovery** (DESIGN §10): kill mid-write, restart,
  assert exactly-once. Needs a process-boundary harness; the state machine
  already supports it.
- **Un-ignore the performance measurements** as a separate `cargo test --
  --ignored` CI job with thresholds, so §11's numbers are guarded rather than
  reported once.
- **README test count.**

---

## 2. Phase B — M1, read-only UI

Acceptance from the brief: Dan stops opening QBO in the browser to look things
up. Every §11 read target met and measured.

Scope, in build order, each one replacing a real QBO trip:

1. Command palette and reference search (the backend exists: `store::search`).
2. Document viewers: invoice, estimate, PO, bill, payment, with the lineage rail
   (`store::lineage` exists).
3. Customer, vendor, item pages with full transaction history and where-used.
4. AR and AP aging, open POs, open sales orders.
5. Sync state in the chrome: last sync, cursor age per entity, quarantine count.
6. Realm switcher.

Not in M1, whatever the prototype shows: orders as a shop-floor queue, build
sheets, builds, manufacturing costing, ledger, period close, vendor credit
application, progress invoicing UI. Those are M3 or ledger-project work and
several depend on the write path.

### The stack decision (Dan's)

The brief says Tauri v2 + React + Vite. The prototype is 3,900 lines of vanilla
HTML/JS with no build step, and it already encodes every M1 screen. Three ways
to go:

| Option | What it is | Cost | Risk |
|---|---|---|---|
| **A. Tauri v2 shell, vanilla TS front-end grown from the prototype** | Tauri commands over `Store`; prototype's fake data layer replaced with `invoke` calls; no React, no bundler beyond `tsc` | Lowest. Reuses the prototype. Native window, no open port. | Vanilla JS at 10k+ lines gets hard to keep coherent; a framework migration later is a rewrite of the view layer. |
| B. Tauri v2 + React + Vite, as briefed | Rewrite the prototype's screens as components | Highest. Every screen rebuilt. | None architecturally; it is the conventional path. |
| C. Rust local HTTP server (axum) + the prototype HTML in a browser | No Tauri at all; JSON over localhost | Low. Fastest to first screen. | A listening port on the machine that holds the book; browser chrome around it; not the desktop app the brief asks for. |

**Recommendation: A.** The prototype is the M1 spec and most of its code is
the view layer M1 needs. Tauri gives the native shell and the no-network
guarantee the brief wants (the front-end has no network capability at all; only
the Rust side talks to Intuit). Move to a framework only if the vanilla code
becomes the bottleneck, and decide that with evidence rather than up front.

Either way, the boundary is the same: a **read-only query API in Rust** over
`Store` (list, get, search, lineage, aging), typed, with the Tauri command layer
as a thin adapter. That API is buildable offline now and is the same one an HTTP
server or a React app would call, so it is the first M1 task regardless of the
stack answer.

---

## 3. Phase C — M2, outbox live against sandbox

Gate: the ⚠️ facts verified (Phase A step 1). Do not start without them; the
`RequestId` scope on Customer/Item decides whether the adopt path is a guard or
the whole mechanism.

1. `HttpQboClient::create` and `update` against an Intuit sandbox company.
2. `RequestId` reuse on retry, checked against real responses.
3. Chaos test passes against the sandbox, not only the mock.
4. Convergence property test passes against the sandbox.
5. Failure inbox surfaced in the UI (`conflicted`, `rejected` records with the
   QBO response, resolution actions: retry, edit, abandon).

Acceptance: zero duplicates under induced network failure. `is_write_enabled`
stays 0 for both production realms throughout.

---

## 4. Phase D — M3, write UI, production behind the flag

Invoice, PO, estimate, bill, receive payment. Keyboard-first. Class on every
line, enforced at the boundary. Optimistic commit with the honest "not yet in
QBO" state the prototype shows.

Production writes: two weeks of daily read-only use elapsed, Dan's explicit go,
one realm at a time, Aquamentor first. No delete, no void.

The prototype's vendor-credit application and progress invoicing belong here,
after the five core documents, not before.

---

## 5. Phase E — M4, reconciliation and hardening

- Nightly sweep in production, with the report visible in the chrome.
- Trial-balance diff: QBO's `TrialBalance` report against one computed from the
  replica. Needs the reports bucket in `HttpQboClient`. Non-zero variance is an
  unmissable error.
- Backup and restore drill: restore last night's snapshot into a fresh path,
  point the app at it, confirm the outbox survived.
- Audit log review tooling over the JSONL write log.
- WaterLine realm switched on, same code path, own budget.

---

## 6. Later — the ledger project

Starts with `LEDGER-DESIGN.md`, leading with the posting-rules table. The
prototype already carries a first draft of that table and encodes three policy
decisions that need Dan's review, not engineering judgement:

1. Raw-material bills capitalise to inventory rather than expensing.
2. A vendor credit reduces material cost rather than posting to other income.
3. Build variance plugs against manufacturing variance rather than being spread
   back over unit cost.

Plus D7 (calendar fiscal year, closed periods reject) and the open tax-rounding
question (DESIGN §12 item 3). Manufacturing costing (landed cost by board-foot,
build sheets with yield as a divisor, sensitivity) lives here too. The replica's
complete history and retained raw JSON are the import source, which is the only
accommodation `qbo-local` makes for it.

---

## 7. Prototype governance

Freeze new prototype features until M1 ships. From here the prototype is the
**spec for M1's read screens**, and its M3/ledger sections are **design assets
for later phases**, not build targets. A new idea goes into `DECISIONS.md` or
`LEDGER-DESIGN.md` as a decision to make, not into `bunzbooks.html` as a screen
to admire. The failure mode is obvious in hindsight: an app that mocks a period
close beautifully and has never fetched an invoice.

---

## 8. Risks, ranked

1. **The four ⚠️ API facts.** Baked into `RealmLimits::default`,
   `requires_query_before_create`, and the D12 backfill. Verification is thirty
   minutes on a machine with access and gates M2.
2. **184 tests prove behaviour against `MockQbo`, not Intuit.** The first live
   sync is where the mock and reality diverge. Record fixtures immediately so the
   divergence becomes a test rather than a memory.
3. **Prototype scope creep**, above.
4. **Performance numbers are `#[ignore]`d**, so a regression ships silently.
5. **Worker local-id rewriting** is the one place a bug double-bills a customer.
   Sandbox chaos testing in M2 is the mitigation; do not skip it for the mock.
6. **macOS-only** once the keychain backend lands. Acceptable; stated so it is
   not a surprise.

---

## 9. Decisions Dan owns

| # | Decision | Recommendation |
|---|---|---|
| 1 | M1 UI stack (§2) | Option A |
| 2 | Freeze the prototype (§7) | Yes |
| 3 | Reuse the `qbo_headless` refresh token or build the consent flow now | Reuse if valid; consent flow is a later hardening item |
| 4 | Start the offline-buildable Phase A items in the cloud sandbox now, ahead of the Mac session | Yes: sweep, daemon loop, query API |
