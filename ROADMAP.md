# ROADMAP — from `qbo-local` to a book of record that is not QuickBooks

Written 11 September 2026 against commit `c56af00`. Revised the same day after
Dan restated the goals. Companion to `HANDOFF.md` (how to finish M0) and
`DECISIONS.md` (why).

## The two goals, and the order they come in

1. **Speed and UI over the QBO book.** Every lookup local, every screen faster
   than the browser, QBO's own model as the guide. This is `qbo-local`, M0 and
   M1, and it pays off on its own.
2. **Own accounting software, decoupled from QBO.** The ledger project. The
   new books **open 1 January 2027** with opening balances as of 31 December
   2026; the **cutover** (the day the new books become the entry point) is
   **1 April 2027**, rolling quarterly (7/1, 10/1, 1/1/28) if the parallel run
   is not clean. FY2027 lives whole in the new books either way (D24, 28 Sep
   2026, replacing the 1/1/27-cutover / 1/1/28-fallback plan of 11 Sep).

The revision this document makes over the first draft: the ledger is the main
line, not a "later" item, and everything built for goal 1 has to be an on-ramp
to goal 2 rather than a detour. Three consequences run through the phases:

- **`LEDGER-DESIGN.md` gets written right after M0**, in parallel with M1, not
  at the end. Its posting-rules table and class taxonomy decide how the
  replica's raw JSON becomes opening balances and history in the new books.
- **The outbox is an export shim, not a destination.** From M3 on, local
  documents are the source of truth and QBO is a downstream mirror kept for the
  CPA and bank feeds. The outbox pushes to it. Decoupling is switching the
  outbox off. The code is the same; the direction of authority flips.
- **The prototype's ledger, period-close and manufacturing sections are the
  first draft of `LEDGER-DESIGN.md`**, not shelved design assets. They get
  promoted into the document, then the prototype stops growing.

---

## 0. Where it stands

`cargo test --workspace`: 677 passed, 5 ignored, all offline, all green (Mac, 14 Oct 2026). Clippy clean.

Everything that can be built **without Intuit credentials** has been built:
money and rounding, realm scoping, SQLite replica with projection and
quarantine, reference-first search, lineage, CDC planning with truncation
recovery (D12), the sync driver with cursor-in-transaction (D13), the outbox
state machine and drain worker, token generations with a file backend, the rate
limiter.

Everything **not** built needs credentials, Intuit's docs, or a UI toolchain:

| Gap | Phase | Needs |
|---|---|---|
| `KeychainTokenStore` | A | **built 28 Sep** (`keychain.rs`, default on macOS); real-keychain test runs on the Mac |
| OAuth loopback flow, or import of the `qbo_headless` refresh token | A | **flow built 13 Sep** (`oauth.rs`, `qbo-local auth`); running it needs the client id/secret and redirect URI on the Mac |
| `HttpQboClient` | A | **built 13 Sep**, proven against a fake Intuit server; first real contact needs credentials |
| First live sync, counts and wall-clock measured | A | **done 14 Oct** (D26): 67,747 rows, 225 s, sweep clean, quarantine 0; CDC daemon running live |
| CDC daemon loop, nightly snapshot | A | **built 11 Sep**, wired into the `qbo-local` binary (`daemon`, `sweep`, `snapshot`, `init`, `status`); runs with `--mock` until `HttpQboClient` exists |
| Reconciliation sweep (DESIGN §7) | A | **built 11 Sep**; TB diff (step 5) waits on the ledger |
| Fixture recorder and scrubber (HANDOFF §2.6) | A | **built 13 Sep**; a synthetic set is committed, real scrubbed ones need one live recording |
| Verify the four ⚠️ API facts (DESIGN §0, §12) | A, gates C | **done 28 Sep, D25**; RequestId Customer/Item scope only settles in a sandbox |
| Read-only query API over `Store` | B | **built 11 Sep** |
| UI shell and read screens | B | **UI built 13 Sep** in `apps/desktop` (every §B1 screen, 13 node tests, driven in headless Chromium); the Tauri shell compiles on the Mac |
| `LEDGER-DESIGN.md` | B | **drafted 11 Sep**; Dan's six gating items decided 12 Sep (D20, D21); Joel's sixteen open |
| Ledger engine, import from replica | B' | **built 12 Sep**: store, posting rules, reports, importer, opening balances, boundary walk, `ledger` CLI; first real run waits on a live replica |
| Write UI into own store, export via outbox | D | C |
| Statement import, sales-tax reports, accountant mode, Authorize.net, MCP server | G | **all built by 13 Sep** except the Authorize.net hosted payment link on the own invoice; nightly parallel-run command and `qbo-local report` built too |

**The Rust side now talks to Intuit** (13–14 Oct 2026, D26). The first live
contact found five things the fake server could not: a projection that drops
`MetaData`, a sync-order problem, paging on a tied sort key, name-list queries
that skip inactive records, and a daemon that exited on EOF. All fixed, each
with a test. Aquamentor is mirrored in full, the daemon runs on CDC, the token
lives in the keychain and survived a restart and a rotation, and `ledger
import` has run once: the tbdiff baseline is D27. The two-week read-only
window started **14 Oct 2026**.

Still open in M0: the 24-hour token idle check; scrubbed real fixtures
(blocked, D26: the scrubber needs a slice and amount scrubbing first);
registering `qbo-local-mcp` against the live replica.

---

## A. Finish M0 — one Cowork session on Dan's Mac

Specified in `HANDOFF.md` §2. Order:

1. **Verify the ⚠️ facts** (HANDOFF §4). Thirty minutes with access to
   `developer.intuit.com`. The batch limit is a config value; the query
   endpoint's `LastUpdatedTime` range support is what D12's backfill stands on.
   Record in `DECISIONS.md`, update DESIGN §0's confidence column.
2. **`KeychainTokenStore`** (HANDOFF §2.1), `security-framework` crate,
   `TokenGenerations` as the stored unit.
3. **Tokens.** If `qbo_headless` holds a valid refresh token, import it and skip
   the consent flow. Build the loopback flow only if it does not.
4. **`HttpQboClient`** (HANDOFF §2.3). `MockQbo` is the behavioural spec.
   Blocking transport; the driver is synchronous by design.
5. **First live sync** of Aquamentor. Rows and wall-clock, measured.
6. **CDC daemon** around `SyncDriver::poll`, 15 s focused, 5 min idle. Nightly
   SQLite backup, N-deep, outbox included.
7. **Record fixtures** into `.local/fixtures/`, scrubbed on the way in; commit
   synthetic equivalents.

Done when `HANDOFF.md` §5 is ticked. The two-week read-only window (DESIGN §8)
starts the day step 5 lands on production. It gates QBO writes only; it does
not gate anything written to your own books.

### Buildable now, offline, ahead of the Mac session

- ~~Reconciliation sweep~~ built: `reconcile.rs`, `store/index.rs`.
- ~~CDC daemon loop and snapshot rotation~~ built: `daemon.rs`, `clock.rs`,
  `store/backup.rs`.
- ~~Read-only query API~~ built: `store/query.rs`. Measured on the synthetic
  10,000-invoice book, debug build: AR aging ~59 ms, document detail ~0.3 ms.
- ~~Wire the daemon into `main.rs`~~ built: subcommand binary, `--mock` for the
  loop today, exit 2 with a clear message without it.
- ~~MCP server over the query API~~ built: `qbo-local-mcp`, 11 read tools,
  `docs/MCP.md`. A skill can be repointed for a read test as soon as a replica
  exists.
- ~~LEDGER-DESIGN.md first draft~~ written (D18), 30 open items for Dan and Joel.
- ~~Chaos test for `in_flight` recovery~~ built: `tests/chaos.rs` kills and
  restarts a real second process (`src/bin/chaos-child.rs`) against a
  `JournalledMock` (`src/client/journal.rs`) that survives the kill;
  `worker.rs`'s convergence property test gained a random restart point
  alongside it. A CI job (`.github/workflows/ci.yml`) also now runs
  `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo test --workspace` on every push and pull request.
- ~~Un-ignore the performance measurements as a thresholded CI job~~ built:
  each DESIGN §11 measurement asserts a 3x budget under `QBO_BENCH_ASSERT=1`
  (`tests/{commit_cost,query,replica}.rs`, `cargo bench-qbo`), and CI's
  `bench` job sets that variable — DESIGN §11's numbers are guarded now, not
  reported once.

---

## B. M1 read-only UI, and LEDGER-DESIGN.md, in parallel

Two tracks that touch different layers and can run as separate agents.

### B1. Read UI (goal 1)

Acceptance from the brief: Dan stops opening QBO in the browser to look things
up. Every DESIGN §11 read target met and measured. Build order, each one
replacing a real QBO trip:

1. Command palette and reference search (`store::search` exists).
2. Document viewers with the lineage rail (`store::lineage` exists).
3. Customer, vendor, item pages with history and where-used.
4. AR and AP aging, open POs, open sales orders.
5. Sync state in the chrome: last sync, cursor age per entity, quarantine count.
6. Realm switcher.

**Stack decision (Dan's).** The brief says Tauri v2 + React + Vite. The
prototype is 3,900 lines of vanilla HTML/JS with no build step and already
encodes every M1 screen.

| Option | What it is | Cost | Risk |
|---|---|---|---|
| **A. Tauri v2 shell, vanilla TS grown from the prototype** | Tauri commands over the query API; prototype's fake data layer replaced with `invoke`; `tsc` only | Lowest; reuses the prototype; native window, no open port | Vanilla at 10k+ lines gets hard to keep coherent; a later framework move rewrites the view layer |
| B. Tauri v2 + React + Vite, as briefed | Rewrite every screen as components | Highest | None architectural |
| C. Rust HTTP server (axum) + prototype in a browser | No Tauri; JSON over localhost | Low; fastest first screen | Listening port on the machine with the book; browser chrome; not the desktop app briefed |

**Recommendation: A.** Given the 1/1/27 target, the cheapest path to a real
screen wins, and the prototype is most of that path. Move to a framework only
with evidence that vanilla is the bottleneck.

### B2. LEDGER-DESIGN.md (goal 2)

Leads with the **posting-rules table**, document type to debit and credit
effects, because that is accounting policy for Dan and the CPA to review, not
engineering judgement. The prototype's ledger section is the first draft.
Three policies it already encodes need an explicit yes or no:

1. Raw-material bills capitalise to inventory rather than expensing.
2. A vendor credit reduces material cost rather than posting to other income.
3. Build variance plugs against manufacturing variance rather than being spread
   back over unit cost.

Also in the document: the double-entry schema; the command/oplog design from the
original kickoff brief; period close as D7 specifies (reject, never warn; reopen
is loud); the class taxonomy, which is the one thing that cannot be backfilled;
the tax-rounding question (DESIGN §12 item 3); and **the import mapping from the
replica**, entity by entity, including what becomes an opening balance and what
becomes history.

Manufacturing costing (landed cost by board-foot, build sheets with yield as a
divisor, sensitivity, build variance) is designed here too, as the section of
the ledger QBO cannot do. It is built after cutover, not before.

### B'. Ledger engine and import

Starts as soon as the posting-rules table is approved. `apps/ledger` stops
being a stub.

1. Chart of accounts, classes, periods, the posting gate.
2. The one function that turns a document into a balanced entry, per the table.
3. Trial balance, P&L, balance sheet, aging, all computed, never stored.
4. **Import from the replica**: every mirrored document posted through the same
   function, so history in the new book is derived, not copied. Trial balance
   of the import diffed against QBO's `TrialBalance` report for the same date.
   Non-zero variance blocks everything downstream.

Step 4 is the first real test of whether the posting rules are right. Expect it
to find policy differences (QBO's own treatment of inventory, sales tax
liability, undeposited funds), and record each one in `DECISIONS.md`.

---

## C. M2 — outbox live against sandbox (optional, deferred — D24)

Under the quarterly cutover QBO stays the **entry point** until cutover, so it
is current by construction and nothing has to be pushed to it. The outbox-live
work below is no longer on the critical path; it is kept, specified, for the
case where some document class has to be entered in the own store first and
mirrored to QBO before cutover. It does not start unless that case appears.

What moves ahead of it, in order:

1. **Write UI into the own store** (§D, without the QBO leg): invoice, PO,
   estimate, bill, receive payment, posting to the own ledger.
2. **Skill repointing through `ledger-mcp`**: the Shopify, Amazon and
   wholesale skills read and write the own store, not QBO.
3. **Authorize.net**: hosted payment link on the own invoice, settlement
   import (built) exercised against the live account.
4. **Joel's sign-off** on accountant mode, in writing.

If C is ever built, its spec is unchanged:

- Gate: the ⚠️ facts verified. The `RequestId` scope on Customer and Item
  decides whether the adopt path is a guard or the whole mechanism.
- `HttpQboClient::create` and `update` against an Intuit sandbox company;
  `RequestId` reuse on retry checked against real responses; chaos and
  convergence tests against the sandbox; the failure inbox in the UI.
- Acceptance: zero duplicates under induced network failure.
  `is_write_enabled` stays 0 for both production realms throughout.

---

## D. M3 — write UI, own store first, QBO second

Invoice, PO, estimate, bill, receive payment. Keyboard-first. Class on every
line, enforced at the boundary.

The change from the first draft: **a save commits to your own document store
and posts to your own ledger**, then enqueues the outbox record that mirrors it
to QBO. The "not yet in QBO" state the prototype shows is now literally true and
stays true until the outbox drains. A QBO rejection is a mirror problem
surfaced in the failure inbox, not a reason the local document is wrong.

Production QBO writes: two-week read-only window elapsed, Dan's explicit go, one
realm at a time, Aquamentor first. No delete, no void, in either book.

---

## E. Parallel run

QBO is the entry point throughout; the own books are derived from it.

**Practice run, October–November 2026**, on Q3 and Q4 2026 data. Its only job
is to surface posting-policy differences early (ROADMAP risk 4), while there is
still time to decide them with Joel before the real run starts. Variances found
here become `DECISIONS.md` entries, not code patches to force a match.

**Real run, January–March 2027:**

- The new books open 1/1/27 with opening balances as of 12/31/26, taken from
  QBO (`ledger opening`).
- Nightly: `qbo-local` daemon keeps the replica current, `ledger import`
  backfills Q1 2027 into the new books, `ledger tbdiff` compares the own trial
  balance with QBO's `TrialBalance` report for the same date. Any variance is
  an unmissable error with the offending entries listed. AR and AP aging diffed
  by customer and vendor. The replica reconciliation sweep still runs.
- After Joel closes FY2026 in QBO, **re-apply the opening balances** (his
  adjusting entries change 12/31/26) and **lock the period** 12/31/26 and
  earlier in the new books.
- Q1 2027 sales tax is **filed from QBO**; the own four-line report (§G) is run
  alongside it as a check and its variance recorded.

Bar for cutover: the quarter closes with zero unexplained TB variance.
Explained variances are recorded in `DECISIONS.md` with the policy that
produced them.

---

## F. Cutover — 1 April 2027, rolling quarterly

**Go/no-go around 15 March 2027.** Go requires all of:

- Q1 2027 parallel run with zero unexplained TB variance to date, on track to
  close the quarter clean.
- FY2026 opening balances re-applied after Joel's close, period locked.
- Write UI into the own store in daily use for the document types Dan enters
  (§C's reprioritised list, item 1).
- Skills that write orders repointed through `ledger-mcp`.
- Authorize.net live on the own invoice.
- Accountant mode accepted by Joel in writing.

**If it is a no-go**, nothing is lost: the cutover moves to the next quarter
boundary — 1 July 2027, then 1 October 2027, then 1 January 2028 — and the
parallel run simply continues, with a go/no-go about two weeks before each
boundary. Because the new books opened on 1/1/27 and have been backfilled
nightly, **FY2027 lives whole in the new books** whichever quarter cutover
lands on; there is never a split fiscal year.

At cutover: QBO stops being the entry point and becomes read-only history. The
FY2026 1099-NEC and tax package come from QBO. Sales tax for every quarter
after cutover is filed from the own report. The outbox is not switched on at
cutover: under D24 QBO is not mirrored after it.

Honest read: the Rust side has still not made one HTTP call to Intuit as of 28
September. The practice run needs a live replica in October. That is the
dependency to protect; everything after it has two quarters of slack before
the first cutover date.

---

## G. Decoupling blockers that are not the ledger

The double-entry engine is the easy part. These are what actually keep the
books in QBO, each scoped as its own item with its own acceptance:

| Blocker | Realistic 2026 path | Later path |
|---|---|---|
| Bank and credit-card statements | **Statement import** (Dan, 11 Sep): the CSV/OFX/QFX files the bank and Chase already generate, parsed into a `bank_lines` table, then a match-and-clear screen that pairs each line to a payment, deposit, bill payment or expense and flags the unmatched. Reconciliation is a per-statement close: beginning balance + cleared lines = ending balance, or it does not close | Plaid, if manual import ever becomes the bottleneck |
| Sales tax | **Dan's actual quarterly method** (11 Sep): A = Total Income from the P&L for the quarter; B = total sales tax collected for the quarter (from the Sales Tax Liability Report); C = B / 6.625% = taxable sales; D = A - C = non-taxable sales. The own report reproduces exactly those four lines for a quarter and entity, from the ledger: A from income accounts, B from the sales-tax-payable account's credits, C and D derived. No tax engine. Line-level taxability stays in the projection for the day the derivation and the line totals disagree | Rate tables per jurisdiction if nexus ever widens |
| 1099-NEC | **Out of scope** (Dan, 11 Sep). The vendor 1099 flag stays in the design as a column; nothing is generated | |
| CPA handoff | **Accountant mode** (Dan, 11 Sep): a read-only role in the app with what QBO's accountant view gives Joel today. Scoped in `LEDGER-DESIGN.md`: period-locked view, GL detail and TB/P&L/BS exports, an adjusting-entry request queue Dan approves rather than direct posting, a reclassify tool, the close-history log from D7, and an audit trail of every change since the last close. Joel sees it before the ~15 March 2027 go/no-go and accepts it in writing | A hosted read-only login instead of a local install |
| Customer payments | **Authorize.net** (Dan, 11 Sep; D15 amended). QBO Payments is used sparingly and is not a hard blocker. The own invoice carries an Authorize.net hosted payment link; a settled payment posts as a customer payment against the invoice. Wire the account, the link, and the settlement import before the ~15 March 2027 go/no-go (D24) | Card-present or ACH if ever needed |
| Channel intake | **MCP server over the own store** (Dan, 11 Sep). The Shopify, Amazon and wholesale skills in `claude-config` write to QBO through the QuickBooks MCP today; they get repointed to the own platform's MCP server rather than rewritten. The server exists (`qbo-local-mcp`, `docs/MCP.md`) with the 11 read tools; document-write tools arrive in Phase D. This is also how every other skill that reads QBO keeps working after cutover | Native channel integrations |
| Payroll | Already SurePayroll; journal the summary. Not a blocker | |
| Inventory quantities | QBO's inventory tracking is not used for foam; the own system's manufacturing costing replaces it after cutover | |

---

## H. After cutover

Manufacturing costing as designed in LEDGER-DESIGN.md is **built** (13 Sep,
`apps/ledger/src/mfg`, `docs/MFG.md`); it is used in earnest only once the
ledger is the book of record. Orders as a shop-floor queue. Vendor credit
application and progress invoicing as the prototype shows them. WaterLine's
realm, same code path, own budget. These are the features QBO never had and are
the reason for goal 2; they wait only because they are worthless in a book that
is not yet the book of record.

---

## Prototype governance

The prototype stops growing once its ledger, period-close and manufacturing
sections have been lifted into `LEDGER-DESIGN.md`. From then on it is the spec
for B1's read screens. A new idea goes into `DECISIONS.md` or
`LEDGER-DESIGN.md` as a decision, not into `bunzbooks.html` as a screen. The
failure mode it guards against is an app that mocks a period close beautifully
and has never fetched an invoice.

---

## Risks, ranked

1. **The cutover date.** Mitigated entirely by the go/no-go and the rolling
   quarterly fallback (D24). The one way it goes wrong is skipping the
   parallel-run bar to hit a date.
2. **The four ⚠️ API facts**, baked into `RealmLimits::default`,
   `requires_query_before_create` and D12's backfill. Thirty minutes on a
   machine with access.
3. **184 tests prove behaviour against `MockQbo`, not Intuit.** Record fixtures
   at the first live sync so divergence becomes a test.
4. **Posting-policy differences from QBO surfacing at import** (B' step 4). Not
   a risk to avoid, a risk to schedule: budget time for it.
5. **CPA acceptance.** A cutover Joel will not work with is not a cutover.
6. **Performance numbers are `#[ignore]`d**, so a regression ships silently.
7. **Worker local-id rewriting** is where a bug double-bills a customer.
   Sandbox chaos testing in C is the mitigation.
8. **macOS-only** once the keychain backend lands. Acceptable; stated so it is
   not a surprise.

---

## Decisions Dan owns

| # | Decision | Status |
|---|---|---|
| 1 | Cutover date | **Books open 1/1/27; cutover 4/1/27, rolling quarterly; go/no-go ~15 Mar 2027** (D24, 28 Sep 2026; replaces 1/1/27 cutover, 1/1/28 fallback) |
| 2 | M1 UI stack | **A**, D14 |
| 3 | The three posting policies in B2 | Open; needed before B' starts |
| 4 | Reuse the `qbo_headless` refresh token or build the consent flow | Open; recommendation reuse if valid |
| 5 | Customer payments after cutover | **Authorize.net**, D15 amended; QBO Payments used sparingly |
| 6 | Start the offline-buildable Phase A items now, ahead of the Mac session | **Yes**, done 11 Sep |
| 7 | Sales-tax filing method | **Four-line quarterly derivation** from P&L total income and tax collected at 6.625%; see §G |
