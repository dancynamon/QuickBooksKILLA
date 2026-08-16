# Claude Code Kickoff Prompt — Local-First Accounting System

Paste everything below the line into a fresh Claude Code session in an empty repo directory.

---

TIER: Opus. Decided upstream — do not re-classify.

# PROJECT: `ledger` — a local-first, double-entry accounting system

You are the lead engineer. I am Dan Cynamon: 40+ years manufacturing, C/C++/Linux
background, I read code and I will review your architecture decisions. Do not
explain what a tool does. Do not pad. Show me code, schemas, and trade-offs.

## 1. Why this exists

I run two businesses on QuickBooks Online: **Aquamentor** (aquatic safety
equipment, Garwood NJ — manufacturer + wholesaler + dropshipper) and
**WaterLine CNC** (CNC cutting / UV printing job shop). QBO is unusable at speed
because every keystroke round-trips to Intuit's servers. Entering a 12-line
invoice takes minutes. Looking up a customer takes seconds. There is no offline.

I want an accounting system that feels like a local text editor: every read is a
memory-speed SQLite query, every write is an fsync, and the network is never in
the interaction path. Evernote's old local-store model is the feel I'm after —
the data lives on my disk, sync is a background reconciliation, not a dependency.

**Target: this fully replaces QBO as my book of record.** Not a viewer, not a
front-end. Real double-entry general ledger, real financial statements, real
year-end handoff to my CPA.

## 2. Non-negotiable constraints

1. **Offline-first, always.** The app must be fully functional with the network
   interface down. No feature may block on a remote call. Ever.
2. **Correctness over features.** This holds money. A bug that silently
   corrupts a balance is catastrophic and unacceptable. Every ledger invariant
   is enforced at the database layer, not just in application code.
3. **Money is never a float.** Store all monetary amounts as signed 64-bit
   integers in minor units (cents). Use `rust_decimal` for rates, quantities,
   and tax percentages, and round explicitly with a documented rounding policy
   (banker's rounding on tax, half-up on line extensions — but make this a named
   constant, not scattered magic).
4. **The ledger is append-only and immutable.** Nothing is ever edited or
   deleted. Corrections are reversing entries. This is both correct accounting
   and the foundation for future multi-device sync.
5. **Data outlives the app.** The SQLite file must be readable and meaningful
   with plain `sqlite3` and no application code. Plus a documented, versioned
   JSON/CSV export of everything that can rebuild the DB from scratch.
6. **Single-user now, two-user later.** I am the only user today. My partner
   John (john@aquamentor.com) will need access eventually. Do not build sync
   now, but do not architect anything that forecloses it. See §6.
7. **Multi-company from day one.** Two legal entities, separate books, one app,
   one login, hard isolation between their data. Retrofitting this is misery.
   **The two companies share nothing** — separate customer master, separate
   vendor master, separate item master, separate chart of accounts, separate
   inventory and separate costing. There is no cross-company join anywhere in
   the system. The only shared thing is the application binary.

## 3. Stack (decided — do not re-litigate)

- **Shell:** Tauri v2
- **Core / business logic:** Rust
- **Database:** SQLite via `rusqlite` (bundled, WAL mode, `foreign_keys=ON`,
  `synchronous=FULL` for ledger writes)
- **UI:** TypeScript + React + Vite
- **Dev machine:** macOS (Apple Silicon). Must also build clean on Linux.
- **No cloud services, no Postgres, no Docker, no Electron.**

Choose UI libs yourself but justify each in one line. Bias toward boring:
TanStack Table for grids, TanStack Query for the Rust IPC boundary, a keyboard
command palette. No component framework that fights me on dense data entry.

## 4. Domain model — get this right before writing UI

### Core ledger (the only source of financial truth)

```
companies        (id, name, fiscal_year_start, home_currency, ...)
accounts         (id, company_id, number, name, type, subtype, parent_id,
                  is_active, normal_balance)
journal_entries  (id, company_id, entry_date, posted_at, memo, source_type,
                  source_id, reversal_of_id, is_posted)
classes          (id, company_id, name, parent_id, is_active)
journal_lines    (id, entry_id, line_no, account_id, debit_minor, credit_minor,
                  memo, entity_type, entity_id, class_id NULL, ...)
```

**`class_id` is nullable but present from day one.** I manufacture across foam
products, signs, lifeguard chairs, and CNC job work, and those have very
different margins. I want per-product-line P&L. Every document line and every
journal line carries an optional class, documents default it from the item, and
every report in §8 takes an optional class filter. Do not defer this — adding a
class dimension to five years of imported history later is not a refactor, it is
a re-import.

Enforce at the DB level with triggers or a strict insert path:
- Sum(debit_minor) == Sum(credit_minor) per entry. No exceptions.
- Exactly one of debit_minor / credit_minor is non-zero per line.
- A posted entry can never be UPDATEd or DELETEd. Enforce with triggers.
- Every line's account belongs to the entry's company.

Account types: Asset, Liability, Equity, Income, COGS, Expense — with subtypes
that map cleanly onto QBO's taxonomy so my CPA and any future export is sane.

### Subledgers / documents (these *generate* journal entries, they are not the ledger)

- `customers`, `vendors`, `items`
- `estimates` → `sales_orders` → `invoices` → `payments` → `deposits`
- `purchase_orders` → `bills` → `bill_payments`
- `credit_memos`, `vendor_credits`, `refunds`
- `bank_accounts`, `bank_transactions`, `reconciliations`

Every document that has financial effect posts a journal entry via one code path.
There must be exactly one function in the codebase that writes to `journal_lines`.

### Inventory & manufacturing (do not treat as an afterthought)

I am a manufacturer, not a reseller. The system must handle:
- Items with type: inventory, non-inventory, service, **assembly/BOM**, discount
- Weighted-average costing (implement FIFO behind a trait so it can swap later)
- Build/assembly transactions that consume components and produce finished goods
- Landed cost allocation: inbound freight distributed across a receipt by value
  or by a physical measure (I cost foam by **board-foot**, so support a
  per-item allocation basis)
- Scrap/yield loss on cut parts
- Inventory asset account and COGS posting on every sale, automatically

### Tax

- NJ sales tax, destination-based, with per-customer exemption certificates and
  per-item taxability. Out-of-state wholesale is usually exempt.
- Tax rates are effective-dated. Historical transactions must reprint with the
  rate in force at the time.
- 1099-NEC vendor tracking.

### Multi-entity

Company scoping on every table and every query. Add a debug assertion or a
query-builder guard that makes an unscoped query on a company-scoped table a
compile-time or startup error, not a runtime surprise.

## 5. Architecture

```
crates/
  ledger-core/     pure Rust domain: types, money, posting rules, invariants.
                   Zero I/O. Zero SQLite. 100% unit-testable.
  ledger-db/       SQLite schema, migrations, repositories, transactions.
  ledger-import/   QBO import, CSV/OFX bank import, mapping rules.
  ledger-report/   P&L, balance sheet, trial balance, GL, AR/AP aging, cash flow.
  ledger-app/      Tauri commands. Thin. Translates IPC <-> ledger-core.
ui/                React app.
```

`ledger-core` must have no knowledge of the database. All posting rules and money
math live there and are tested without touching disk.

**Migrations:** forward-only, numbered SQL files, applied in a transaction,
version recorded in the DB. Never edit a shipped migration.

**Backups:** automatic. Before every migration and on a timer, snapshot the DB
via SQLite's backup API to a dated file, and keep an N-deep rotation. Also write
an append-only transaction journal in plain JSONL alongside the DB, so a
corrupted SQLite file is a recoverable event, not a business-ending one.

## 6. Sync (design for it, do not build it)

Future state: me on a Mac, John on his machine, both offline-capable.

Architect now so this is additive later:
- Every mutating operation is a serializable **command** with a UUIDv7 id, an
  actor id, and a hybrid-logical-clock timestamp.
- Persist those commands to an append-only `oplog` table from day one, even
  though nothing reads it yet. The SQLite tables are a projection of the oplog.
- Document (do not implement) how a second node would replay and merge: which
  entities are last-writer-wins, which are append-only-no-conflict (ledger
  entries), and which need a real merge (item master, customer master).
- Never put the SQLite file itself in Dropbox. Design for oplog shipping instead.
  Say so explicitly in the README so I do not do it by accident later.

## 7. Data migration from QBO

**Decision made: full transaction history, every year, line-level detail.** Not
opening trial-balance entries per year. This is a real replacement, and a system
that cannot answer "what did I pay per board-foot for blue XLPE in 2023" or
produce a prior-year comparative P&L is not a replacement. Accept the heavier
import and reconciliation work; it is a one-time cost.

Two paths, build both:

1. **Primary:** QBO Accountant's export / report exports (CSV) —
   Chart of Accounts, General Ledger detail (all years), Customer list, Vendor
   list, Item list, Open Invoices, Open POs, Unpaid Bills, Trial Balance as of
   each fiscal year end.
2. **Secondary:** QBO REST API pull (I have OAuth access) for anything the CSVs
   mangle.

The import must be:
- **Idempotent.** Re-running produces the same result, no duplicates.
- **Reconciled.** After import, generate a trial balance and diff it line-by-line
  against QBO's trial balance for the same date. Any variance over $0.00 is a
  hard failure that prints the offending accounts, not a warning.
- **Auditable.** Every imported record keeps its QBO id in a `source_ref` column.

Plan a **parallel-run period**: I keep entering in both systems for a while and
you build a nightly diff report. Do not tell me the cutover is safe until three
consecutive months reconcile to the penny.

## 8. Reports (v1 must produce, all as-of-date and date-range aware)

Profit & Loss (with comparison periods and % of income), Balance Sheet,
Trial Balance, General Ledger detail, AR Aging summary + detail, AP Aging
summary + detail, Statement of Cash Flows (indirect), Sales by Customer,
Sales by Item, Inventory Valuation, Open POs, Open Sales Orders.

**Every report supports both cash basis and accrual basis.** Accrual is the
internal default; my CPA works in cash. `basis: Accrual | Cash` is a first-class
parameter on every report function signature from the first report you write —
not a flag bolted on later, because retrofitting it means rewriting the report
layer. Document precisely how cash basis re-attributes an invoice to its payment
date, how it handles partial payments across periods, and how unpaid AR/AP
simply vanishes from a cash P&L.

**Every report also takes an optional class filter** (see §4) so I can pull a
P&L for foam products alone.

Every report is a pure function over the ledger. No cached balances that can
drift. If a report is slow, fix it with indexes and materialized views that are
rebuilt from the ledger, never with hand-maintained running totals.

## 9. Performance targets (measure, do not assume)

- App cold start to usable UI: < 1000 ms
- Any customer/item/transaction lookup: < 16 ms
- Keystroke to rendered character in any input: < 16 ms, always
- Save an invoice: < 50 ms including fsync
- Full P&L over 5 years of data: < 500 ms

Write a benchmark harness and a synthetic data generator (100k transactions,
5k customers, 2k items) in the first milestone. I want these numbers on a graph,
not in a paragraph.

## 10. Testing — non-negotiable

- Property-based tests (`proptest`) asserting the ledger *always* balances after
  any sequence of valid operations.
- Golden-file tests for every report against a fixed seeded dataset.
- A fuzz/soak test that hammers random valid transactions for 10 minutes and then
  verifies the trial balance from scratch.
- Round-trip test: export everything → wipe → import → byte-identical export.

CI runs all of this. A red test blocks a milestone.

## 11. Milestones — deliver in this order, stop at each for my review

**M0 — Skeleton.** Repo, crates, Tauri shell, migration runner, backup system,
benchmark harness, synthetic data generator, CI. No business logic.
*Acceptance:* `cargo test` green, app opens an empty company, 100k-row benchmark
prints numbers.

**M1 — The ledger.** Accounts, journal entries, the single posting path, DB-level
invariants, trial balance report, property tests.
*Acceptance:* I can hand-post journal entries in the UI, the TB always balances,
and it is impossible to post an unbalanced entry through any route including raw
SQL.

**M2 — Masters + AR.** Customers, items, estimates → sales orders → invoices →
payments, with automatic posting. Sales tax engine.
*Acceptance:* I enter a real Aquamentor invoice, the GL is correct, AR aging is
correct, and it is faster than QBO by an order of magnitude.

**M3 — AP + inventory.** Vendors, POs, bills, bill payments, inventory receipts,
weighted-average costing, COGS posting, assemblies/BOM, landed cost by board-foot.

**M4 — QBO import + reconciliation.** Full history import, TB diff harness,
nightly parallel-run diff report.

**M5 — Reports + banking.** All reports in §8, CSV/OFX bank import, matching
rules engine, bank reconciliation.

**M6 — Hardening.** Backup/restore drills, export/import round-trip, CPA export
package, performance pass against the §9 targets.

## 12. Explicitly out of scope for v1

Payroll (I use SurePayroll), multi-currency, budgeting, time tracking,
a web/mobile client, any hosted service, e-invoicing, ACH origination.
Do not build these. Do not add abstraction layers "for later" for these.

## 13. How I want you to work

- **Plan before code.** Start by reading nothing and writing a `DESIGN.md`
  covering the schema, the posting-rules table (document type → debit/credit
  effects), the money type, the rounding policy, and the sync-forward design.
  Show me that first. I will mark it up.
- Ask me a question the moment a decision is genuinely mine to make. Do not
  guess at accounting policy.
- One concern per commit. Conventional commits.
- When you make a trade-off, state the alternative you rejected in one line.
- No "successfully", no "let me know if", no summarizing what you just did.

**Your first output is `DESIGN.md` and nothing else.** Do not scaffold the repo
until I approve the schema.

---

## Decisions already made — do not re-open these

- Companies share **nothing**. Separate customer, vendor, item masters, separate
  COA, separate inventory, **inventory costing is per-company**. No cross-company
  joins.
- Reporting supports **both cash and accrual basis**, as a parameter on every
  report from day one.
- **Class tracking is in from day one** as a nullable dimension for
  per-product-line P&L.
- QBO migration is **full transaction history**, not opening balances.

## Open items I have NOT decided — raise these in DESIGN.md with a recommendation

1. Fiscal year start for each entity, and whether prior years are locked/closed
   (and what "locked" enforces — reject posting, or warn?).
2. Class list itself: propose a starting taxonomy for Aquamentor (foam products,
   signs, lifeguard chairs, dropship/resale) and WaterLine CNC (CNC cutting, UV
   printing) and I will correct it.
3. Whether cash-basis reports need to be exactly reconcilable to QBO's cash-basis
   output during parallel run, or "materially equal" is acceptable — this is a
   meaningful difference in effort and I want your read on it.
