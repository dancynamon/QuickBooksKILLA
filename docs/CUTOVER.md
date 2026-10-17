# The slim cutover import

DECISIONS.md D28 (and its addendum), D29. This is the operator's runbook for
`ledger import-slim`: what it brings across, how to rehearse it against the
live replica, and what each check means when it fails. The code is
`apps/ledger/src/slim.rs`; the tests are `apps/ledger/tests/slim.rs`.

## Where it sits

- Books open **1 January 2027**; cutover is **1 April 2027** (D29).
- The slim import runs **as of 31 December 2026**, once the replica holds
  everything dated through that day, and is **re-run after Joel's FY2026
  close**. A re-run is safe: see "Re-running" below.
- Q1 2027 is replayed by `ledger import --from 2027-01-01` under the full
  posting gate (no fallback class for 2027). Never run `ledger import`
  without `--from` into the cutover book: it would post 2026 documents on top
  of the opening entry. The open items the slim import brought across are
  refused if it tries (see "Guards"), but everything else from 2026 would
  post. The same holds for `ledger nightly`: the cutover book's nightly runs
  with `--from 2027-01-01`, and refuses to run without a `--from` after the
  opening entry's date (`docs/NIGHTLY.md`, "The cutover book").

## What it brings across

1. **Masters**, keyed by their QBO id, with the id in `source_ref`:
   chart of accounts and classes (the same mapping `ledger import` uses, so
   the 185 `needs_mapping` accounts land in their x99N slots exactly as
   before), customers and vendors (terms folded into `terms_days`; a
   date-driven QBO term has no net-days equivalent and is reported, then
   treated as due on receipt), items (accounts through the chart mapping,
   default class from QBO's `ClassRef` when it has one, inventory items with
   their asset account). Categories are skipped; group items come across as
   non-inventory; a SKU QBO lets two items share comes across on the first
   one only.
2. **Numbering**: each numbered type (estimate, invoice, sales receipt,
   credit memo, refund receipt, purchase order) continues past QBO's
   highest numeric `DocNumber` in the replica, paid documents included.
3. **One opening entry**, dated the as-of date, from QBO's trial balance:
   one line per account, balanced by construction, plus one reclass: customer
   payments still unapplied on the date move from 1200 to 2300, where the
   ledger's own posting rule keeps them (W8). Income, COGS and expense lines
   take the import-only class `unclassified`.
4. **Documents open on the date**, saved and never posted:
   invoices and bills (full detail when nothing was paid; one line at the
   open balance, naming the original total and what was settled in QBO, when
   partly paid), credit memos and vendor credits, AR/AP lines that journal
   entries, deposits and checks or expenses left open for a named customer or
   vendor, unapplied customer payments and bill payments (a QBO bill payment
   states no unapplied amount: it is its total less its applications), open
   estimates and open purchase orders. "Open on the date" is worked out from the documents, not from
   QBO's `Balance` field, so an invoice paid on 5 January is still open as of
   31 December.

**Not brought across**: QBO sales orders. They are not mirrored in the
replica (no SalesOrder entity; they live in QBO's Advanced Commerce API), so
open sales orders have to be re-entered in the ledger or carried as
estimates. Paid documents and all other history stay in the replica and QBO
(D28).

## Rehearsing against the live replica

Run this on the Mac, any time before cutover, as often as you like. It reads
three QBO report CSVs and a copy of the replica, and writes only to a scratch
book. The report pulls are read-only Reports API calls; the daemon keeps
running (it shares the keychain token and re-reads it on rotation).

Take the replica copy with SQLite's online backup, so the live file is only
read and the copy is consistent even while the daemon writes:

```bash
sqlite3 "file:.local/replica.db?mode=ro" ".backup .local/cutover/replica-copy.db"
```

```bash
qbo-local report --realm REALM --name trial-balance --as-of 2026-12-31 --out .local/cutover/qbo-tb.csv --live
```

```bash
qbo-local report --realm REALM --name ar-aging --as-of 2026-12-31 --out .local/cutover/qbo-ar.csv --live
```

```bash
qbo-local report --realm REALM --name ap-aging --as-of 2026-12-31 --out .local/cutover/qbo-ap.csv --live
```

```bash
ledger import-slim --dry-run --scratch .local/cutover/rehearsal.sqlite --db .local/ledger.sqlite --company aquamentor --replica .local/cutover/replica-copy.db --realm REALM --as-of 2026-12-31 --qbo-csv .local/cutover/qbo-tb.csv --qbo-ar .local/cutover/qbo-ar.csv --qbo-ap .local/cutover/qbo-ap.csv
```

- `--db` is optional for a dry run. Given, the scratch book starts as a copy
  of it (`VACUUM INTO`); not given, it starts blank with the seed chart.
- `--scratch` keeps the scratch book for inspection (`ledger tb`,
  `ledger gl`, the aging tools in `ledger-mcp` all work against it). It must
  not exist yet. Without it the scratch book is a temp file, removed after.
- Before 31 December 2026, use today's date as `--as-of` for every command
  above: the rehearsal is then "as if we cut over today".
- The output is a report: masters, the opening entry, open documents by kind,
  the checks, the accounts needing mapping, and warnings. Exit 0 means every
  check passed; exit 1 means at least one failed. Keep the output next to the
  scratch book under `.local/` (it names real customers, so it stays out of
  the repo, HANDOFF §3.5).

Without `--qbo-ar`/`--qbo-ap` the aging checks compare against the documents'
own `Balance` fields. That is only sound while nothing dated after the
as-of date has touched them, so once a payment dated after it exists in the
replica the check fails and asks for the report CSV. Use the CSVs.

## The real run

The same command without `--dry-run`, with `--db` the cutover book:

```bash
ledger import-slim --db .local/ledger.sqlite --company aquamentor --replica .local/cutover/replica-copy.db --realm REALM --as-of 2026-12-31 --qbo-csv .local/cutover/qbo-tb.csv --qbo-ar .local/cutover/qbo-ar.csv --qbo-ap .local/cutover/qbo-ap.csv
```

It always rehearses first, on a scratch copy of `--db`. If any check fails
there, it prints `nothing written` and exits 1 with the real book untouched.
Only a clean rehearsal is repeated against the real book, which is then
checked again. Back the book up first anyway.

## The checks

Every one fails the run.

| check | what it proves | when it fails |
|---|---|---|
| opening entry balances without a 3950 plug | QBO's trial balance rows net to zero | the CSV is incomplete or was pulled on a different date than the others |
| 3950 nets to zero | nothing parked in opening balance equity | QBO's own Opening Balance Equity carries a balance (Joel to clear in the FY2026 close), or the plug above |
| AR open items tie to 1200 | the open documents add up to the control account | an AR movement in QBO with no customer on it, or a second QBO AR account (mapped to 1201…) |
| AP open items tie to 2000 | as AR, for AP | as AR |
| AR aging by customer equals QBO's | each customer's open balance, bucket by bucket (current, 1–30, 31–60, 61–90, over 90), matches QBO's aging report, counting the unapplied payments the ledger holds in 2300 | a document the translation could not read (precision), a due date read differently, a payment dated after the as-of date applied to a document dated before it that QBO ages differently |
| AP aging by vendor equals QBO's | as AR | as AR |
| trial balance equals QBO's on every must-tier line | §7's must-tier groups: checking, card, AR+2300, AP+2050, sales tax, total income, total equity | an account mapped onto a must-tier number the QBO CSV does not carry; `needs_mapping` accounts are skipped by design |

The 185 `needs_mapping` accounts never fail the run on their own (tbdiff
skips them), but they are listed in every report: Joel maps them before
sign-off (§2).

## Re-running

The import is idempotent. Masters upsert. A document or opening entry whose
payload is unchanged is skipped. One that changed (Joel's adjusting entries
move the trial balance, a late payment closes an invoice) is replaced: the
opening entry is reversed and reposted as of the as-of date, an open item is
saved as a new version, and an open item from an earlier run that is no
longer open is saved again voided. Run it after Joel's close with fresh CSVs
and the book follows.

If 2026 has been closed in the ledger by then, the reversal of the old
opening entry is refused (`PeriodClosed`): reopen 2026, re-run, close again.

## Guards

An open item the slim import brought across has its balance in the opening
entry, so posting it would put it on the books twice. `ledger import` and
`ledger-mcp` (`create_document`, `save_and_post_document`) refuse to save a
new version of one and say why. Settle it with a payment or credit memo
(applications to it work like any other document's), or adjust it with a
journal entry.

`ledger nightly` refuses a book holding the slim opening entry (oplog kind
`import_slim_opening`) unless `--from` is after the entry's date, so an
unwindowed run can never post 2026 on top of it.

## The nightly after the slim import

Once the real run has written the cutover book, schedule its nightly with
the window baked in:

```bash
ledger nightly --plist --from 2027-01-01 --db .local/ledger.sqlite --company aquamentor --replica .local/replica.db --realm REALM --qbo-csv .local/nightly/qbo-tb.csv --out .local/nightly
```

Every night it replays Q1 2027 (only the documents dated from 1 January)
under the full posting gate and diffs the result against QBO's trial
balance as of yesterday. Re-running the slim import after Joel's close does
not change the window.

## Item classes (D33)

QBO items carry no default class and QBO's own classes are unused, so under
the full posting gate the Q1 2027 replay refuses nearly every income line
for want of a class. Dan decides each item's class once, on a worksheet; the
code only suggests and then records what he wrote. Nothing is defaulted.

**Where the map lives:** the `item_class_map` table in the ledger book
(migration 7; QBO item id → class), written by `ledger classes apply`. It
travels with `--db`, which every command that reads it already takes:
`ledger import` and `ledger nightly` (`pipeline::import_replica`) and
`ledger import-slim` (its rehearsal copies the book, so the map comes too).
An item in the map takes that class as its default, ahead of any `ClassRef`
QBO carries on the item; a class on the QBO line itself still wins. A fresh
scratch book (`import-slim --dry-run` without `--db`, the practice-run
example) has no map until one is applied to it.

1. **Generate the worksheet.** Read-only on the replica (it opens it
   `SQLITE_OPEN_READ_ONLY`), but take the copy anyway so the daemon is never
   contended:

   ```bash
   sqlite3 "file:.local/replica.db?mode=ro" ".backup .local/cutover/replica-copy.db"
   ```

   ```bash
   ledger classes suggest --replica .local/cutover/replica-copy.db --out .local/cutover/item-classes.csv
   ```

   One row per active item (categories and inactive items left out):
   `qbo_id, sku, name, type, category, income_account, sales_lines_12m,
   sales_amount_12m, suggested_class, rule, class`. Sales are Invoice and
   SalesReceipt lines in the twelve months to `--as-of` (default today),
   credits not netted. Sorted by sales amount, so the top of the sheet is what
   matters. `suggested_class` and `rule` come from
   `apps/ledger/item-class-rules.toml` (checked in, commented; `--rules` to
   try another file). The CSV holds item names: keep it under `.local/`,
   never commit it. The command prints only counts.

2. **Review.** Fill the `class` column (one of `foam`, `sign`, `chair`,
   `drop`, `cnc`, `uv`); copying the suggestion is a decision, leaving it is
   not. Work from the top. Rows you have not decided yet: delete them. Other
   columns may be reordered, hidden or added; only `qbo_id` and `class` are
   read.

3. **Apply** to the book the replay runs in (the cutover book once the slim
   import has created it; before that, `ledger init` it first):

   ```bash
   ledger classes apply --db .local/ledger.sqlite --company aquamentor --csv .local/cutover/item-classes.csv
   ```

   All or nothing: any row with no class, a class the book does not have,
   `unclassified` (D28's import-only fallback) or two classes for one item
   refuses the whole file and lists every such row. Items not in the file
   keep their mapped class, so apply in batches as the review goes; a later
   file changes an item's class by naming it again.

4. **Re-run** the slim import and the nightly as usual; their items now carry
   the class. Whatever is still refused for want of a class is an item not
   yet mapped, or an account-only line (purchases, deposits, journal entries)
   that D33 still has to settle.

**First dry run (17 October 2026, replica as of that morning, sales to
10/16):** 3,250 active items, 233 of them sold in the twelve months. The
rules suggest a class for 998 items covering 87.6% of the 806,954.57 of
item sales: foam 453 items (611,504.62), chair 34 (40,175.00), drop 478
(54,446.03), sign 33 (400.90); 2,252 items with no suggestion carry
100,428.02. Most of that remainder is shipping (no rule by design: its class
is a policy call), pool lifts and other resale without a telling category.

## Rehearsal 1 — 17 October 2026, as of 30 September 2026

The first dry run against the live replica, read-only: three Reports API
pulls, a SQLite backup copy of the replica, a scratch book outside the repo.
Nothing was written to QBO, the live replica or any real ledger file, and the
daemon kept running. Code at `31bcbd8` (every branch merged) plus the fixes
below. The reports and scratch books stay outside the repo; they name real
customers and vendors (HANDOFF §3.5), so contacts are cited here by QBO id.

**Timings** (Apple silicon, release build):

| step | wall clock |
|---|---:|
| `qbo-local report` trial balance (190 rows) | 9.6 s |
| `qbo-local report` AR aging (105 customers) | 5.0 s |
| `qbo-local report` AP aging (35 vendors) | 3.0 s |
| `sqlite3 .backup` of the replica (210 MB) | 1.6 s |
| `ledger import-slim --dry-run` | 3.2–4.0 s |

**What came across**: 10,128 customers, 1,280 vendors, 3,474 items, 317
accounts created (184 of them `needs_mapping`), 4 classes; an opening entry
of 120 lines with 5,177.83 of unapplied customer payments moved from 1200 to
2300; 698 open documents (123 invoices, 22 credit memos, 34 bills, 2 vendor
credits, 59 journal entry lines, 5 check or expense lines, 1 deposit line,
17 unapplied payments, 5 unapplied bill payments, 169 estimates, 261
purchase orders). Next numbers: invoice 18934, estimate 3651, purchase
order 8712, credit memo 512753, sales receipt 249284 (the last two follow
QBO's channel-imported numbers; see below).

**Checks, first run**:

| check | result |
|---|---|
| opening entry balances without a 3950 plug | ok |
| 3950 nets to zero | ok |
| AR open items tie to 1200 | FAIL, 623.02 |
| AP open items tie to 2000 | FAIL, 2,555.19 |
| AR aging by customer equals QBO's | FAIL, 2 customers |
| AP aging by vendor equals QBO's | FAIL, 8 vendors |
| trial balance equals QBO's on every must-tier line | ok |

Every failure was a bug in the import, each now fixed with a test in
`apps/ledger/tests/slim.rs`:

1. **Checks and expenses that post to AR or AP were ignored.** 46 Purchases
   in the replica hit AR or AP: a check refunding an unapplied customer
   payment (customer 18074, 537.25), card payments posted straight to a
   vendor's AP as a prepayment (vendors 19556, 19170, 19374, 19482). They are
   now open items like journal entry and deposit lines.
2. **QBO spells a deposit line's entity type `CUSTOMER` / `VENDOR`.** The
   import matched `Customer` only, so 6 deposits (2 AR, 4 AP) fell out of the
   aging (customer 17135, 1,160.27). Matched without regard to case now, in
   the slim import and in the translation's deposit entity, which
   `ledger import` uses too.
3. **A bill payment's unapplied amount was always zero.** QBO states none on
   a bill payment; what it paid beyond its applications is a vendor
   prepayment (vendor 19398, a bill payment with no bill, 154.40) or the cash
   side of one that also used a vendor credit (19587, 241.85; 20339, 315.53;
   19368, 16.09). The translation now works it out as total less
   applications, which also fixes how `ledger import` posts such a payment
   (it posted the payment short, or as an empty entry); the slim import works it out again after
   it re-signs applications to journal entry, deposit and check lines by the
   item they relieve.

**Checks after the fixes**: all seven pass, exit 0. AR open items
64,350.81 = 1200; AP 35,585.33 = 2000; all 105 customers and 35 vendors
equal QBO's aging bucket by bucket; must tier clean.

**Mapping and policy items for Joel** (none fails the run; each changes what
the passing numbers mean):

- **184 `needs_mapping` accounts, 66 of them with a balance on 9/30:** 5
  asset (402,419.93), 8 liability (−541,490.64, loans and payables), 53
  expense (460,309.18 year to date). §2 blocks sign-off while any carries a
  balance. The trial balance check passes because both sides use the
  importer's own mapping; D35 asks Joel to sign an explicit QBO → §2 table
  instead, and until he does a must-tier pass is only as good as that guess
  (2300 carries QBO "Loan Payable", 5000 "Merchant Account Fees", and so on).
- **Classes (D33):** none of the 3,474 items has a default class, and QBO's
  three classes are unused. Fine for the import (its lines take
  `unclassified`), a blocker for posting Q1 2027 under the full gate until
  Dan's item worksheet is applied ("Item classes (D33)" above).
- **Items pointing at non-income accounts (D34)** and **inventory relief
  (D36):** affect the Q1 replay, not the slim import.
- **20 date-driven payment terms** (due on the 1st or the last of the month)
  have no net-days equivalent; 3,126 customers and 49 vendors use one and
  came across as due on receipt. Policy: convert to net days, or teach the
  books a day-of-month term.
- **6 duplicate SKUs** in QBO; the second item of each came across without a
  SKU.
- **Sales receipt and credit memo numbering** jumps to 249284 and 512753
  because channel imports number those types with order numbers. Policy: let
  the ledger continue there, or restart its own series.

**What would block a real run on 31 December 2026**, as things stand:

1. **QBO sales orders** are not in the replica; open ones need re-entering or
   carrying as estimates (Dan).
2. **D35 / the 185 (now 184) accounts** need Joel's mapping table before
   the run's must-tier pass means what it says, and before sign-off (§2).
3. **QBO's Opening Balance Equity** must be zero at the FY2026 close, or the
   3950 check fails. It was zero on 9/30.
4. **Report CSVs pulled for the same date** as the run, after the replica
   holds every document dated through it; the year-end rerun after Joel's
   close needs a fresh set.
5. Nothing in the import code is known to block: the first live rehearsal
   found three bugs, all fixed, and the second ran clean.
