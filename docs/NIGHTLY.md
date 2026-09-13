# The nightly parallel run

`LEDGER-DESIGN.md` §7, `ROADMAP.md` §E and §F. This is the operator's guide to
what runs every night, where its output lands, and what a red night means; the
policy (which accounts must match to the cent, which may differ with an
explanation) is in `LEDGER-DESIGN.md` §7 and is not repeated here.

The code is `apps/ledger/src/nightly.rs` (`ledger nightly`) and
`apps/qbo-local/src/reports.rs` (`qbo-local report`). Both books — QBO and
this ledger — post every document independently; the nightly run is what
proves, every night, that they still agree.

## What runs

Two commands, chained, once a night, at 02:00 local:

```
qbo-local report --realm ID --name trial-balance --as-of YESTERDAY \
    --out qbo-tb.csv --live

ledger nightly --db LEDGER.sqlite --company aquamentor \
    --replica REPLICA.sqlite --realm ID \
    --qbo-csv qbo-tb.csv --out NIGHTLY_DIR --as-of YESTERDAY
```

The first pulls QBO's own `TrialBalance` report (the Reports API,
`LEDGER-DESIGN.md` §6/§7) as of yesterday and writes it as the
`qbo_account_id,name,balance` CSV `ledger` reads everywhere else this shape
is used (`tbdiff`, `opening`, `boundary`).

The second:

1. **Imports the whole replica** through `pipeline::import_replica` — the
   same posting function every other path in this codebase uses, not a
   parallel implementation. This is a full re-import every night (no
   `--from`/`--to` window): a document whose payload has not changed since
   the last run is skipped, one that changed has its prior entries reversed
   and replaced, so this is cheap in the steady state despite touching every
   document every night.
2. **Reads the resulting trial balance** as of the same `--as-of` date.
3. **Diffs it against the QBO CSV** (`report::tb_diff`, the §7 tier rules)
   and writes the result as both `tbdiff-<as-of>.txt` (the fixed-width report
   in §7's own shape) and `tbdiff-<as-of>.csv`, into `--out`. Both are kept
   forever — nothing here ever deletes an old night's report.
4. **Prints a one-line verdict** — `nightly: green` or `nightly: RED — N
   must-tier failure(s), M rejected document(s)` — followed by the full
   report, so the tail of a cron log says everything a person needs before
   they open a file.

`ledger nightly` exits 0 when the run is green and 1 when it is not — a
must-tier disagreement (§7) or a document the importer could not post either
one. A usage error (a bad flag, a missing file) is 2, same as every other
`ledger` subcommand.

## Where the reports land

`--out NIGHTLY_DIR` — a plain directory, one pair of files per night:

```
NIGHTLY_DIR/
    tbdiff-2026-09-10.txt
    tbdiff-2026-09-10.csv
    tbdiff-2026-09-11.txt
    tbdiff-2026-09-11.csv
    ...
```

Nothing rotates or prunes this directory. A quarter of nightly runs is under
200 small text files; disk is not the constraint the design is optimizing
for, an auditable trail is. Back it up the same way the ledger and replica
files are backed up (it is not — today — inside either SQLite file).

## What a red night means

The verdict line names the two ways a night goes red, and they mean different
things:

- **A must-tier failure** (`diff.must_failures` non-empty) is a real
  disagreement between the two books on an account §7 says must match to the
  cent: a bank or credit-card account, AR total (1200+2300), AP total
  (2000+2050), sales tax payable, total income, or total equity. The `.txt`
  report names which group failed and, for the accounts-receivable line
  specifically, lists the entries on each side that make up the difference
  (`LEDGER-DESIGN.md` §7's own example). This is the one that needs a person
  before the next close: either a genuine posting bug, or a timing gap (an
  invoice saved in this ledger with the outbox mirror to QBO still pending)
  that should resolve itself within a day or two.
- **A rejected document** (`import.rejected` non-empty) is a document the
  importer could not translate or post at all — an unknown account, a
  missing class, a period the posting gate refused. It fails the run even if
  every must-tier account still happens to balance, because a document that
  never posted is a discrepancy the trial balance diff has no way to see.

A **may-differ** line going non-zero (`MAY-DIFFER UNEXPLAINED` in the `.txt`
report) does not fail the run. §7's second tier — inventory, COGS,
manufacturing variance, parts and labour applied, undeposited funds at a
period end, the income split across 4100/4300/4900/4950 — is expected to
differ for named, standing reasons (moving weighted average versus QBO's
FIFO, tax rounding mode, and so on), each recorded in `DECISIONS.md`. An
unexplained variance there is worth a look before it becomes routine, but it
is not the thing that blocks going live.

## Nightly and the reconciliation sweep are not the same check

`qbo-local sweep` (`DESIGN.md` §7) verifies that the **replica** — the raw
mirror of QBO's own records — has not drifted from QBO's index. `ledger
nightly` verifies that the **ledger**, derived from that replica by replaying
every document through the posting rules, agrees with what QBO itself
computed. The replica can be perfectly in sync with QBO while the posting
rules still produce a wrong number, and the reverse: run both, every night,
for as long as the parallel run lasts.

## The go/no-go bar

`ROADMAP.md` §F: go/no-go on 1 November 2026 requires the parallel run to
have gone at least four weeks with zero unexplained must-tier variance, on
track to reach a full quarter by 31 December. Every explained variance in
that run needs its policy recorded in `DECISIONS.md` before it counts as
"explained" rather than "unexplained" — a may-differ line with no entry in
`DECISIONS.md` is treated as a must-tier failure for the purposes of that
count. If the four-week bar (or the accompanying Phase G blockers — statement
import, the sales-tax report, accountant mode, Authorize.net) is not met on 1
November, the cutover date moves to 1 January 2028 without argument, and the
parallel run keeps going through all of 2027.

## Scheduling it

`ledger nightly --plist --db LEDGER.sqlite --company aquamentor --replica
REPLICA.sqlite --realm ID --qbo-csv qbo-tb.csv --out NIGHTLY_DIR` prints a
`launchd` plist (to stdout — redirect it to a `.plist` file) that runs both
commands above, chained with a shell `&&`, once a day at 02:00 local, on
`YESTERDAY`'s date computed at run time. Install it the ordinary `launchd`
way:

```
ledger nightly --plist --db ~/.local/ledger.db --company aquamentor \
    --replica ~/.local/replica.db --realm 1234567890123456 \
    --qbo-csv ~/.local/qbo-tb.csv --out ~/.local/nightly \
    > ~/Library/LaunchAgents/com.aquamentor.ledger.nightly.plist

launchctl load ~/Library/LaunchAgents/com.aquamentor.ledger.nightly.plist
```

`--plist` only ever prints; it never opens the ledger file, the replica, or
`--out`, so it is safe to run before any of those paths exist.
