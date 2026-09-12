# ROADMAP — from `qbo-local` to a book of record that is not QuickBooks

Written 11 September 2026 against commit `c56af00`. Revised the same day after
Dan restated the goals. Companion to `HANDOFF.md` (how to finish M0) and
`DECISIONS.md` (why).

## The two goals, and the order they come in

1. **Speed and UI over the QBO book.** Every lookup local, every screen faster
   than the browser, QBO's own model as the guide. This is `qbo-local`, M0 and
   M1, and it pays off on its own.
2. **Own accounting software, decoupled from QBO.** The ledger project. Target
   cutover **1 January 2027**, fallback **1 January 2028** (Dan, 11 Sep 2026;
   fiscal year is calendar year per D7, so cutover is a year boundary).

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

`cargo test --workspace`: 288 tests, all offline, all green. Clippy clean.

Everything that can be built **without Intuit credentials** has been built:
money and rounding, realm scoping, SQLite replica with projection and
quarantine, reference-first search, lineage, CDC planning with truncation
recovery (D12), the sync driver with cursor-in-transaction (D13), the outbox
state machine and drain worker, token generations with a file backend, the rate
limiter.

Everything **not** built needs credentials, Intuit's docs, or a UI toolchain:

| Gap | Phase | Needs |
|---|---|---|
| `KeychainTokenStore` | A | macOS |
| OAuth loopback flow, or import of the `qbo_headless` refresh token | A | client id/secret, redirect URI |
| `HttpQboClient` | A | credentials |
| First live sync, counts and wall-clock measured | A | credentials |
| CDC daemon loop, nightly snapshot | A | **built 11 Sep**, wired into the `qbo-local` binary (`daemon`, `sweep`, `snapshot`, `init`, `status`); runs with `--mock` until `HttpQboClient` exists |
| Reconciliation sweep (DESIGN §7) | A | **built 11 Sep**; TB diff (step 5) waits on the ledger |
| Fixture recorder and scrubber (HANDOFF §2.6) | A | one live response |
| Verify the four ⚠️ API facts (DESIGN §0, §12) | A, gates C | `developer.intuit.com`, blocked from the cloud sandbox |
| Read-only query API over `Store` | B | **built 11 Sep** |
| UI shell and read screens | B | stack decision, §B |
| `LEDGER-DESIGN.md` | B | **drafted 11 Sep**; Dan's six gating items decided 12 Sep (D20, D21); Joel's sixteen open |
| Ledger engine, import from replica | B' | **unblocked 12 Sep**; buildable offline against the replica |
| Write UI into own store, export via outbox | D | C |
| Statement import, sales-tax reports, accountant mode, Authorize.net, MCP server | G | scoped in §G |

The Rust side has never made one HTTP call to Intuit. That is still the first
thing to fix.

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
- **Chaos test for `in_flight` recovery** (DESIGN §10) with a process-boundary
  harness.
- **Un-ignore the performance measurements** as a thresholded `--ignored` CI
  job, so DESIGN §11's numbers are guarded rather than reported once.

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

## C. M2 — outbox live against sandbox

Gate: the ⚠️ facts verified. The `RequestId` scope on Customer and Item decides
whether the adopt path is a guard or the whole mechanism.

1. `HttpQboClient::create` and `update` against an Intuit sandbox company.
2. `RequestId` reuse on retry checked against real responses.
3. Chaos and convergence tests pass against the sandbox, not only the mock.
4. Failure inbox in the UI: `conflicted` and `rejected` records with the QBO
   response and the resolution actions.

Acceptance: zero duplicates under induced network failure. `is_write_enabled`
stays 0 for both production realms throughout.

Framing: this is the **export shim**. It is worth building only because QBO has
to stay current for the CPA and bank feeds until cutover. Nothing here is the
destination.

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

Both books post every document. Nightly:

- Trial balance, own ledger versus QBO `TrialBalance` report, same date. Any
  variance is an unmissable error with the offending entries listed.
- AR and AP aging diffed by customer and vendor.
- Reconciliation sweep of the replica (Phase A) still runs, because the mirror
  can drift independently of the ledger.

Minimum duration before cutover: **one full quarter with zero unexplained
variance.** Explained variances are recorded in `DECISIONS.md` with the policy
that produced them.

---

## F. Cutover — 1 January 2027, fallback 1 January 2028

**Go/no-go on 1 November 2026.** Go requires all of:

- Phase E has been running with zero unexplained TB variance for at least four
  weeks, and will reach a full quarter by 31 December.
- Phase G's blockers for the first quarter of 2027 are done: statement import
  with match-and-clear, the sales-tax reports Dan actually files from, an
  accountant mode Joel has seen and accepted in writing, and Authorize.net
  wired for customer payments.
- Channel intake (Shopify, Amazon, wholesale POs) lands in the own store
  through the MCP server, and the skills that write orders have been repointed.

If any of those is missing on 1 November, the target moves to 1/1/28 without
argument, and the parallel run continues for all of 2027. That is the whole
point of a fallback: a bad cutover costs a year of books, a late one costs
nothing.

At cutover: outbox off; QBO kept read-only for history and the FY2026 close; the
replica retained as the import source it always was; FY2026 1099-NEC and the
FY2026 tax package come from QBO, since the 2026 books live there.

Honest read on 1/1/27: today is 11 September. Phase A has not started, needs
the Mac, and a full quarter of parallel run means both books posting by
1 October. That is not going to happen in three weeks. The realistic 1/1/27
case is a parallel run from early November, checked at the go/no-go with the
four-week bar rather than the quarter bar, and the quarter bar reached by
31 December. It is a stretch, it is not impossible, and the fallback makes
missing it cheap.

---

## G. Decoupling blockers that are not the ledger

The double-entry engine is the easy part. These are what actually keep the
books in QBO, each scoped as its own item with its own acceptance:

| Blocker | Realistic 2026 path | Later path |
|---|---|---|
| Bank and credit-card statements | **Statement import** (Dan, 11 Sep): the CSV/OFX/QFX files the bank and Chase already generate, parsed into a `bank_lines` table, then a match-and-clear screen that pairs each line to a payment, deposit, bill payment or expense and flags the unmatched. Reconciliation is a per-statement close: beginning balance + cleared lines = ending balance, or it does not close | Plaid, if manual import ever becomes the bottleneck |
| Sales tax | **Dan's actual quarterly method** (11 Sep): A = Total Income from the P&L for the quarter; B = total sales tax collected for the quarter (from the Sales Tax Liability Report); C = B / 6.625% = taxable sales; D = A - C = non-taxable sales. The own report reproduces exactly those four lines for a quarter and entity, from the ledger: A from income accounts, B from the sales-tax-payable account's credits, C and D derived. No tax engine. Line-level taxability stays in the projection for the day the derivation and the line totals disagree | Rate tables per jurisdiction if nexus ever widens |
| 1099-NEC | **Out of scope** (Dan, 11 Sep). The vendor 1099 flag stays in the design as a column; nothing is generated | |
| CPA handoff | **Accountant mode** (Dan, 11 Sep): a read-only role in the app with what QBO's accountant view gives Joel today. Scoped in `LEDGER-DESIGN.md`: period-locked view, GL detail and TB/P&L/BS exports, an adjusting-entry request queue Dan approves rather than direct posting, a reclassify tool, the close-history log from D7, and an audit trail of every change since the last close. Joel sees it before 1 November and accepts it in writing | A hosted read-only login instead of a local install |
| Customer payments | **Authorize.net** (Dan, 11 Sep; D15 amended). QBO Payments is used sparingly and is not a hard blocker. The own invoice carries an Authorize.net hosted payment link; a settled payment posts as a customer payment against the invoice. Wire the account, the link, and the settlement import before the 1 November go/no-go | Card-present or ACH if ever needed |
| Channel intake | **MCP server over the own store** (Dan, 11 Sep). The Shopify, Amazon and wholesale skills in `claude-config` write to QBO through the QuickBooks MCP today; they get repointed to the own platform's MCP server rather than rewritten. The server exists (`qbo-local-mcp`, `docs/MCP.md`) with the 11 read tools; document-write tools arrive in Phase D. This is also how every other skill that reads QBO keeps working after cutover | Native channel integrations |
| Payroll | Already SurePayroll; journal the summary. Not a blocker | |
| Inventory quantities | QBO's inventory tracking is not used for foam; the own system's manufacturing costing replaces it after cutover | |

---

## H. After cutover

Manufacturing costing as designed in LEDGER-DESIGN.md: landed cost, build
sheets, variance, sensitivity. Orders as a shop-floor queue. Vendor credit
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

1. **The 1/1/27 date.** Mitigated entirely by the go/no-go and the fallback.
   The one way it goes wrong is skipping the parallel-run bar to hit the date.
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
| 1 | Cutover date | 1/1/27 target, 1/1/28 fallback, go/no-go 1 Nov 2026 |
| 2 | M1 UI stack | **A**, D14 |
| 3 | The three posting policies in B2 | Open; needed before B' starts |
| 4 | Reuse the `qbo_headless` refresh token or build the consent flow | Open; recommendation reuse if valid |
| 5 | Customer payments after cutover | **Authorize.net**, D15 amended; QBO Payments used sparingly |
| 6 | Start the offline-buildable Phase A items now, ahead of the Mac session | **Yes**, done 11 Sep |
| 7 | Sales-tax filing method | **Four-line quarterly derivation** from P&L total income and tax collected at 6.625%; see §G |
