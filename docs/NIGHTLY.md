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
    --qbo-csv qbo-tb.csv --out NIGHTLY_DIR [--from 2027-01-01] \
    --as-of YESTERDAY
```

The cutover book always carries `--from 2027-01-01` (see "The cutover book"
below).

The first pulls QBO's own `TrialBalance` report (the Reports API,
`LEDGER-DESIGN.md` §6/§7) as of yesterday and writes it as the
`qbo_account_id,name,balance` CSV `ledger` reads everywhere else this shape
is used (`tbdiff`, `opening`, `boundary`).

The second:

1. **Imports the replica** through `pipeline::import_replica` — the
   same posting function every other path in this codebase uses, not a
   parallel implementation. Without `--from` that is the whole history;
   with it, every document dated on or after it. Either way it is a full
   re-import of that window every night: a document whose payload has not
   changed since the last run is skipped, one that changed has its prior
   entries reversed and replaced, so this is cheap in the steady state
   despite touching every document every night.
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

## The cutover book

Under D28/D29 the cutover book is built by `ledger import-slim` as of 31
December 2026 (`docs/CUTOVER.md`): one opening entry carrying every 2026
balance, plus the documents open on that date saved as never-posted open
items. Only Q1 2027 is replayed. Its nightly therefore runs with
**`--from 2027-01-01`**:

```
ledger nightly --db LEDGER.sqlite --company aquamentor \
    --replica REPLICA.sqlite --realm ID \
    --qbo-csv qbo-tb.csv --out NIGHTLY_DIR --from 2027-01-01 \
    --as-of YESTERDAY
```

`--from` is required on such a book. When the ledger holds the slim opening
entry (oplog kind `import_slim_opening`), `ledger nightly` refuses to run
without a `--from`, or with one on or before the entry's date, and exits 1
naming the date to use. Without the window the carried open items would be
rejected ("opening entry" in the reason), but every other 2026 document —
everything paid before 31 December — would post on top of the opening entry
and double the books. A book without the slim opening entry (one built by a
full `ledger import`) may still run with no window.

The diff is the same: the ledger's trial balance as of yesterday (opening
entry plus the replayed 2027 documents) against QBO's.

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

`tools/nightly.sh` (below, "Switching it on") is how the parallel run is
scheduled: it wraps `ledger nightly` with the replica checks, the aging diff
and a report. `ledger nightly --plist` still prints the older, bare job (the
two commands above chained at 02:00, no freshness check, no aging, no
notification); install one or the other, never both, or the own book is
replayed twice a night. Its form, for reference:

```
ledger nightly --plist --from 2027-01-01 --db ~/.local/ledger.db \
    --company aquamentor --replica ~/.local/replica.db \
    --realm 1234567890123456 --qbo-csv ~/.local/qbo-tb.csv \
    --out ~/.local/nightly
```

`--plist` only ever prints; it never opens the ledger file, the replica, or
`--out`, so it is safe to run before any of those paths exist.

## Switching it on

`tools/nightly.sh` is the whole nightly run as one command, plus the two
launchd jobs that keep it going: one keeps the `qbo-local` daemon up, one
runs the night at 02:30. Installing them is a persistent change to this
Mac, so `install` is **Dan's step**: it refuses unless run from a terminal
and answered `yes`, and nothing else (no session, no other script) runs it.

### Once

From the main checkout (the plists point at the checkout they are rendered
in, so not from a worktree):

```
cd ~/code/QuickBooksKILLA
cargo build --release --bin qbo-local --bin ledger --bin ledger-aging
cp tools/nightly.example.conf .local/nightly.conf   # edit if a default is wrong
tools/nightly.sh plists          # read both plists and the launchctl commands
kill "$(cat .local/m0-run/daemon.pid)"   # stop the hand-run daemon (pgrep -fl 'qbo-local daemon' to check)
tools/nightly.sh install         # the one command: writes and loads both jobs
tools/nightly.sh status          # both jobs loaded, daemon running, cursor fresh
tools/nightly.sh run             # one night now, rather than waiting for 02:30
```

`.local/nightly.conf` is shell assignments; every line in the example is the
default, so an empty file works when the realm id is in `.local/config.toml`
and the replica and ledger are `.local/replica.db` and `.local/ledger.db`.
The defaults that matter: `FROM=2027-01-01` (D29), reports under
`~/Library/Application Support/QuickBooksKILLA/nightly` (outside the repo
and outside Dropbox), `KEEP=90` nights, stale after `MAX_CURSOR_AGE_MIN=360`.

`install` stops before changing anything if a binary, the config, the
replica or the ledger is missing, or if a `qbo-local daemon` is running that
launchd did not start — the KeepAlive job beside a hand-run daemon would be
two writers on one replica. It never stops a daemon itself. Re-running
`install` after a rebuild or a config change reloads both jobs.

| Command | Does |
|---|---|
| `tools/nightly.sh run` | one night, as below; what the 02:30 job runs |
| `tools/nightly.sh plists` (= `install --dry-run`) | prints both plists, the launchctl commands and a preflight; changes nothing |
| `tools/nightly.sh install` | Dan's step: writes `~/Library/LaunchAgents/com.aquamentor.qbo-local.plist` and `com.aquamentor.nightly.plist`, lints them, `launchctl bootstrap`s both |
| `tools/nightly.sh uninstall [--dry-run]` | `launchctl bootout` both (the daemon stops) and moves the plists to `REPORT_DIR/launchd-archive/`; the replica, ledger and reports stay |
| `tools/nightly.sh status` | both jobs (loaded, pid, last exit), the daemon, the live replica's cursor age (read-only), and last night's verdict; exit 1 if any is wrong |

The templates are `launchd/qbo-local-daemon.plist.in` (KeepAlive, restarted
at most once a minute, logs in `REPORT_DIR/logs/qbo-local-daemon.*.log`) and
`launchd/nightly.plist.in` (02:30 local; a slot missed while the Mac sleeps
fires at the next wake; logs in `REPORT_DIR/logs/nightly.*.log`). Only the
rendered copies carry the realm id, and those live in `~/Library/LaunchAgents`.

### What one night does

1. **config** — the binaries, replica and ledger exist.
2. **daemon** — a process matches `pgrep -f 'qbo-local daemon'`.
3. **snapshot** — `qbo-local snapshot` copies the replica into
   `REPORT_DIR/snapshots/` (newest 7 kept). This is the only read of the live
   file; every later step reads the snapshot, so the daemon keeps writing
   undisturbed and the night is one consistent point in time.
4. **freshness** — the snapshot's *oldest* CDC cursor (per entity type, so
   one type that stopped syncing is caught) is within `MAX_CURSOR_AGE_MIN`.
5. **qbo-tb, qbo-ar, qbo-ap** — `qbo-local report --live` for QBO's trial
   balance and AR/AP aging as of yesterday.
6. **ledger-nightly** — `ledger nightly --from 2027-01-01` replays the
   snapshot into the own book and diffs the trial balance (the tbdiff pair
   still lands in `REPORT_DIR/tbdiff/`, never pruned).
7. **ledger-aging** — the own book's AR/AP aging in QBO's CSV shape.
8. **verdict** — `tools/nightly-verdict.py` diffs the aging by contact and
   bucket, applies the explanations, writes the report and prunes.

A failed gate (daemon down, replica stale, snapshot or config failed) stops
the night there; it still writes a report saying why.

### Exit codes and the notification

| Exit | Means |
|---|---|
| 0 | green: every step ran, every non-zero line is explained, nothing rejected |
| 1 | an unexplained trial-balance or aging variance, or a rejected document |
| 3 | the daemon is not running, or the replica is stale |
| 4 | a step failed (a command errored, or left no output) |
| 2 | bad config (no realm) |

When two apply, the exit names the first of 3, 4, 1: a night with a dead
daemon or a failed step proves nothing about the books. Any non-zero exit
posts one macOS notification naming the first reason; a green night is
silent, the same rule as fleet-watch.

### Explaining a variance

The go/no-go bar treats a variance with no recorded policy as unexplained,
so every non-zero line is red until `.local/nightly-explained.csv` names the
decision that covers it — must tier and may-differ alike, and every aging
contact that differs:

```
kind,key,delta,decision,note
tb,Total income (4xxx),*,D34,items pointed at non-income accounts
tb,Sales tax payable (2200),-405.32,D35,mapping
ar,*,*,D40,customer credits held in 2300 are not in the own AR aging
```

`kind` is `tb`, `ar` or `ap`. `key` is the tbdiff account label exactly as
the CSV prints it, or an aging contact's QBO id; `*` matches any. `delta` is
the exact ledger − QBO amount, or `*` for any amount — prefer an exact amount
where the variance is static, so a new difference on the same line is still
red. `decision` must be a heading in `DECISIONS.md` (`## D35 — …`); a row
naming no recorded decision is reported as invalid and explains nothing. A
rejected document is never explainable here: it is always red. `ar,*,*` is a
blunt instrument — it silences every AR contact — so use it only while D40
is open.

### The report

`REPORT_DIR/nightly-<as-of>.md` is for reading; `nightly-<as-of>.json` holds
the same facts for tools. Both are named for the as-of date (yesterday), and
a rerun for the same date replaces them. The newest `KEEP` nights are kept,
with each night's working files in `REPORT_DIR/runs/<as-of>/` (QBO's CSVs,
the ledger's aging, every step's log).

| JSON field | Meaning |
|---|---|
| `verdict` | `green` or `red` |
| `exit_code` | the script's exit, as in the table above |
| `reasons` | one line per thing that made it red, most useful first in the notification |
| `as_of`, `from`, `company` | the date diffed, the replay window, the book |
| `started_at`, `finished_at` | UTC |
| `steps[]` | `name`, `status` (`ok`, `failed`, `skipped`, `down`, `stale`), `exit`, `seconds`, `log`, `detail` (the step's last line) |
| `daemon` | `alive`, `pids`, the `pattern` matched |
| `replica` | `snapshot` path, `oldest_cursor` and `oldest_entity`, `newest_cursor`, `age_minutes`, `max_age_minutes`, `fresh`, `problem` |
| `tb.rows[]` | every tbdiff line: `account`, `tier` (`must`, `may`, `skip`), `ledger`, `qbo`, `delta` (ledger − QBO), `explained_by` (a decision id) |
| `tb.must_failures`, `tb.may_differ`, `tb.unexplained` | non-zero must lines, non-zero may lines, and how many of those have no explanation |
| `rejected` | `count` and `documents[]` (`document_id`, `reason`) the importer refused |
| `aging.ar`, `aging.ap` | `ledger_total`, `qbo_total`, `contacts`, `differing`, `unexplained`, and `rows[]` for each differing contact: `contact` (QBO id), `name`, `ledger`, `qbo`, `delta` (totals), `buckets_differ` (which of current, 1-30, 31-60, 61-90, over_90, total), `explained_by` |
| `unexplained` | trial balance plus aging lines with no explanation |
| `explanations` | the `file`, rows `loaded`, rows `invalid` (and why), rows `unused` tonight |
| `artifacts` | `run_dir`, `markdown`, `json` |

### Known limits

- `qbo-local report --live` and the daemon both refresh the realm's OAuth
  tokens. They share the keychain store, which keeps prior generations, but
  two processes rotating in the same second is not proven safe; the 02:30
  slot keeps the reports to three calls a night.
- The plists run the release binaries in place: rebuilding while the daemon
  runs is fine, but restart it (`launchctl kickstart -k
  gui/$(id -u)/com.aquamentor.qbo-local`) to pick up the new one.
