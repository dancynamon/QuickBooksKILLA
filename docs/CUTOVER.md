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
  post. `ledger nightly` currently imports the whole replica with no window,
  so it must not be pointed at the cutover book until it takes a `--from`.

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
   entries and deposits left open for a named customer or vendor, unapplied
   customer payments and bill payments, open estimates and open purchase
   orders. "Open on the date" is worked out from the documents, not from
   QBO's `Balance` field, so an invoice paid on 5 January is still open as of
   31 December.

**Not brought across**: QBO sales orders. They are not mirrored in the
replica (no SalesOrder entity; they live in QBO's Advanced Commerce API), so
open sales orders have to be re-entered in the ledger or carried as
estimates. Paid documents and all other history stay in the replica and QBO
(D28).

## Rehearsing against the live replica

Run this on the Mac, any time before cutover, as often as you like. It reads
the replica and three QBO report CSVs and writes only to a scratch book.

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
ledger import-slim --dry-run --scratch .local/cutover/rehearsal.sqlite --db .local/ledger.sqlite --company aquamentor --replica .local/replica.sqlite --realm REALM --as-of 2026-12-31 --qbo-csv .local/cutover/qbo-tb.csv --qbo-ar .local/cutover/qbo-ar.csv --qbo-ap .local/cutover/qbo-ap.csv
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
ledger import-slim --db .local/ledger.sqlite --company aquamentor --replica .local/replica.sqlite --realm REALM --as-of 2026-12-31 --qbo-csv .local/cutover/qbo-tb.csv --qbo-ar .local/cutover/qbo-ar.csv --qbo-ap .local/cutover/qbo-ap.csv
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
