# Bank statement import and reconciliation

`LEDGER-DESIGN.md` §10, D17 ("Bank and card feeds: import the statements the
bank and Chase already generate; no aggregator. Reconciliation is a
per-statement close."). This is the how-to; the policy and the schema are in
the design document, this file is the operator's guide.

The code is `apps/ledger/src/bank.rs` and its `csv`/`ofx` submodules. Nothing
here calls out to a bank's API or Plaid — every statement arrives as a file
Dan (or the bank's own portal) already produces.

## Exporting a statement

**Chase.** From the account's Activity page, "Download account activity" as
CSV, any date range that does not overlap a statement already imported (a
little overlap is harmless — see Re-importing, below). Chase's card and
checking export carries the same seven columns either way: `Transaction
Date, Post Date, Description, Category, Type, Amount, Memo`. Use
`--format csv --profile chase`.

**The bank** (anything that is not Chase). Most banks offer an OFX or QFX
export from the same activity/download page — `--format ofx`, no profile
needed, since OFX is self-describing. If only CSV is offered and it does not
match Chase's column order, describe it with a custom `CsvProfile` (date
column, description column, amount column or a debit/credit pair, the date
format, and whether an id column exists) rather than forcing it through
`chase()`; `generic()` is the built-in default for a plain `date,description,
amount,external_id` export.

## The two CSV profiles

| | `CsvProfile::chase()` | `CsvProfile::generic()` |
|---|---|---|
| Columns | Transaction Date, **Post Date**, Description, Category, Type, Amount, Memo | date, description, amount, external_id |
| Date read | Post Date — the date that actually lands on Chase's own statement, which is what the match window (below) compares against | the one date column |
| Amount | one signed column; `(24.99)` reads as `-24.99` | one signed column |
| Its own transaction id | none — see "Where the id comes from" below | the `external_id` column |

Every amount is read through `RoundingPolicy::MirroredAmount` — the bank
already rounded it, so this crate does not round again — and an amount with
more than two decimal places quarantines the whole import with a
`ParseError::Precision` rather than being silently rounded (D9). No file has
ever actually done this; it exists as a guard, not a workaround.

### Where the id comes from, when the file has none

Chase's own export carries no id per row. `parse_csv` synthesizes one from
the row's own content — the date, the amount, a hash of the description, and
how many identical rows came before it in the same file — so re-parsing the
same file twice produces the same ids in the same order. That is what makes
the dedupe below work even without a bank-supplied id; two genuinely
identical transactions on the same day still each get their own id and both
import once.

OFX/QFX files carry a real id, `FITID`, per `<STMTTRN>`; that is used as-is.

## Importing

```
ledger bank import --db LEDGER.sqlite --company aquamentor \
    --account 1100 --file chase-september.csv \
    --format csv --profile chase \
    --opening 41288.12 --closing 42884.19 \
    --from 2026-09-01 --to 2026-09-30
```

`--opening`/`--closing` are the statement's own stated beginning and ending
balance — type them from the paper or PDF statement, or the account summary
at the top of the export; nothing here parses them out of a balance
`LEDGERBAL` tag automatically today, though `bank::parse_ofx_ledger_balance`
exists for a caller that wants to.

**Re-importing is always safe.** A line whose `(account, external_id)` this
ledger has already seen is skipped, never duplicated — this is what lets
statements overlap at the edges, which they always do, without creating two
copies of the same transaction. Importing the same file twice reports
`inserted 0`.

## Matching

```
ledger bank match --db LEDGER.sqlite --company aquamentor --statement STMT_ID
```

Each unmatched line on the statement is tried against these rules, in order,
and stops at the first one that fires:

1. **Exact.** A posted, uncleared journal line on the same bank account, for
   the same signed amount, dated within the match window (default 3 days
   either side — a check clears when it clears, not the day it was written)
   — restricted to a Payment, Deposit, BillPayment or Purchase document. A
   bank-in line (money added to the account) matches a *debit* to the
   account; a bank-out line matches a *credit*. Marked `exact`, and the
   journal line's `cleared_at` is stamped — the one field a posted line may
   still have written.

2. **Settlement.** A bank-in line with no exact match, whose amount equals
   the sum of a Settlement (or Deposit) entry's credits to 1160 Authorize.net
   clearing, within the same window. The settlement entry already exists —
   this rule matches to it rather than proposing a new document — and it is
   that entry's own leg on the statement's bank account that gets
   `cleared_at`. Marked `settlement`.

3. **Proposed.** Neither of the above: the line is marked `proposed` and a
   suggested account and reason come back from a small keyword table over
   the description (shipping carriers and unrecognized vendors to 6900,
   channel/software names to 6600, `SUREPAYROLL` to 2400, an
   `AUTHORIZE`/`AUTHNET` mention that did not resolve via rule 2 flagged back
   toward 1160 for a human to chase down, `CHASE` + `PAYMENT` to 2100). This
   is a suggestion only — see below.

A proposal is not a transaction. Nothing is posted, nothing is created, and
running `match` again leaves every already-resolved line — `exact`,
`settlement`, `manual`, or a standing `proposed` — untouched; only lines that
have never been looked at get tried against the rules.

## What a proposal is, and confirming it

`bank match` never posts anything for a line it cannot match to something
already on the books. It proposes: an account, and a one-line reason. Dan (or
whoever is reconciling) reviews it —

```
ledger bank status --db LEDGER.sqlite --company aquamentor --statement STMT_ID
```

— and either accepts the suggestion or picks a different account, then:

```
ledger bank confirm --db LEDGER.sqlite --company aquamentor \
    --line LINE_ID --account 6600 --class foam
```

only then does this build a `BankLine` document (one line, naming the
account and class given) and post it through the same posting function every
other document in this ledger goes through. The line is then marked `manual`
and its bank-side journal line clears, exactly as an `exact` or `settlement`
match would. A line can be confirmed exactly once — confirming an
already-resolved line is refused.

`--class` is required only when the chosen account is income, COGS or a
class-consuming expense (4xxx/5xxx/6xxx/7xxx, §3); a balance-sheet account
takes none.

## The close

```
ledger bank close --db LEDGER.sqlite --company aquamentor --statement STMT_ID
```

A statement closes when **both** hold:

- `opening_balance + sum(amounts of every matched line) == closing_balance`
- every line on the statement is matched (`sum(amounts of every unmatched
  line) == 0`, which in practice means there are none left)

If either fails, nothing changes — no lines are touched, the statement stays
open — and the command exits 1, printing the exact shortfall and how many
lines remain unmatched:

```
statement STMT_ID does not close: off by -45.00 with 1 unmatched line(s)
```

That is the signal to go back to `bank status`, find the unmatched line (or
lines), and either match them by hand or confirm a proposal for them.

Once a statement closes, it is immutable: its lines cannot be re-matched
(`bank match` on a closed statement is refused), and `close` on an
already-closed statement is a harmless no-op that reports the original close
time rather than an error. An unreconciled statement covering a period
blocks that period's close (`LEDGER-DESIGN.md` §5, checklist item 1) —
closing every statement is not optional busywork, it is what makes the
period close mean something.
