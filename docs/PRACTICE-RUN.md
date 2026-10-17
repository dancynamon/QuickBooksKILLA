# The practice parallel run, Q3 2026

`ROADMAP.md` §E: a run on 2026 data whose only job is to surface posting-policy
differences while there is time to decide them with Joel before the real Q1
2027 run. First run: 17 October 2026. Decisions it produced: `DECISIONS.md`
D32 (what it found, four bug fixes) and D33–D36 (questions for Dan and Joel).

## What it mimics

D29's Q1 2027 run, moved back two quarters:

| Real run (D29) | Practice run |
|---|---|
| Opening balances from QBO's `TrialBalance` at 12/31/26 | at **6/30/26** |
| Q1 2027 replayed nightly under the full posting gate | **7/1/26 – 10/16/26** replayed once, full gate |
| D28 fallback class only before 1/1/27 | only before **7/1/26**: an unclassed line in the window is rejected |
| `ledger tbdiff` and AR/AP aging against QBO | at 7/31, 8/31, 9/30 and 10/16 |

A second, diagnostic run lets the fallback class cover the window too
(`--fallback-before 2026-10-17`). Its diff is what the posting rules produce
once class is out of the way, which is the part that is about policy.

Everything was read-only: QBO through the Reports API and a few `SELECT`
queries; the replica through a `sqlite3 .backup` copy taken at 06:40 local on
17 Oct; the ledger was a fresh scratch file. No write to QBO, no outbox, the
real `.local/ledger.db` untouched.

## Commands

Scratch directory `S`, outside the repo (this run used
`~/Library/Application Support/QuickBooksKILLA/practice-run-2026-10-17`).
`REALM` read from `.local/config.toml`, never written down.

```
# 1. A consistent copy of the live replica (the daemon keeps running).
sqlite3 -readonly .local/replica.db ".backup '$S/replica.db'"
sqlite3 "$S/replica.db" "PRAGMA journal_mode=DELETE;"   # so it opens read-only

# 2. QBO's trial balance for the opening date and each diff date.
for d in 2026-06-30 2026-07-31 2026-08-31 2026-09-30 2026-10-16; do
  target/release/qbo-local report --realm "$REALM" --name trial-balance \
      --as-of $d --out "$S/qbo/tb-$d.csv" --live --config .local/config.toml
done
#    and ar-aging / ap-aging the same way for 7/31, 8/31, 9/30, 10/16.

# 3. Opening entry, replay, diffs, aging, rejection list.
cargo build --release -p ledger --example practice_run
target/release/examples/practice_run \
    --db "$S/ledger.db" --replica "$S/replica.db" --realm "$REALM" \
    --opening 2026-06-30 --from 2026-07-01 --to 2026-10-16 \
    --qbo-dir "$S/qbo" --out "$S/out" \
    --as-of 2026-07-31 --as-of 2026-08-31 --as-of 2026-09-30 --as-of 2026-10-16
#    diagnostic: add --fallback-before 2026-10-17, with a different --db/--out.
```

`apps/ledger/examples/practice_run.rs` is the driver: `ledger init`, then
`pipeline::import_replica` with the window and the fallback cut-off, then
`pipeline::apply_opening_balance`, then for each date `report::tb_diff`
and `books::aging`. It writes `import.txt`, `rejected.csv`,
`tbdiff-<date>.{txt,csv}`, the ledger's own per-account `tb-<date>.csv` and
`aging-{ar,ap}-<date>.csv` into `--out`, and refuses an existing `--db`.
`ledger import` itself always applies D28's 1/1/27 cut-off, which is right
for the real run and would hide every class finding here, so the practice
run needs the example rather than the CLI.

**Before the fix in D32 item 1**, step 2 needed `--start 2026-01-01`:
without it QBO returns today's month-to-date for every `--as-of`. The fixed
`qbo-local` sends the start date itself. This run's aging reports came from
the QuickBooks MCP server (`report_aged_receivables` / `report_aged_payables`
with `report_date`), because a rebuilt `qbo-local` needs a person at the
keychain prompt (D26).

## Timings

| Step | Wall clock |
|---|---|
| Replica backup (210 MB) | 1 s |
| One QBO `TrialBalance` or aging report | under 2 s each, 13 in all |
| `cargo build --release -p ledger --example practice_run` | 22 s (incremental) |
| Import of the window, 1,925 documents | 2.1 s |
| Opening entry, four diffs, eight agings | under 0.1 s |

A Q1 night will be the same order of size: the replay is not the constraint.

## Numbers

### The full-gate run (what Q1 2027 would look like today)

1,925 documents translated: **287 posted, 315 non-posting** (estimates and
POs), **1,323 rejected, every one "no class on the line, its item, or the
document header"** (881,948.94). 318 accounts created, 185 needing mapping,
4 classes created. By month, rejected documents: July 332 (321,223.61),
August 378 (223,786.70), September 392 (212,504.37), October to the 16th
221 (124,434.26). By type: D33.

Must-tier deltas (ledger − QBO):

| | 7/31 | 8/31 | 9/30 | 10/16 |
|---|---|---|---|---|
| Checking (1100) | 34,888.20 | 104,449.10 | 144,621.39 | 147,986.61 |
| AR (1200+2300) | −87,175.61 | −161,041.56 | −198,148.83 | −249,691.17 |
| AP (2000+2050) | 77,674.19 | 110,256.31 | 164,734.13 | 181,936.55 |
| Total income | 141,655.13 | 251,947.40 | 321,750.69 | 386,203.86 |
| Total equity | 0.00 | 0.00 | 0.00 | −1,000.00 |

Payments and bill payments need no class and post; the invoices and bills
they settle are rejected. So AR goes negative and AP positive, and the whole
P&L stays at the opening entry. Red on every line, every night.

### The diagnostic run (class out of the way), after the four fixes

1,585 posted, 340 non-posting, 0 rejected.

| Must tier | 7/31 | 8/31 | 9/30 | 10/16 | Cause |
|---|---|---|---|---|---|
| Checking (1100) | 41,311.11 | 104,383.09 | 132,702.37 | 133,702.37 | card payments and transfers not mirrored (D32) |
| AR (1200+2300) | 0.00 | 0.00 | 0.00 | 0.00 | |
| AP (2000+2050) | 0.00 | 0.00 | 0.00 | 0.00 | |
| Sales tax payable (2200) | −4.64 | −4.64 | −150.32 | −405.32 | mapping (D35) |
| Total income | −1,835.95 | −5,605.68 | −7,909.30 | −10,322.98 | items pointed at non-income accounts (D34) |
| Total equity | 0.00 | 0.00 | 0.00 | −1,000.00 | transfer not mirrored (D32) |

| May tier, 10/16 | Delta | Cause |
|---|---|---|
| Undeposited funds (1150) | 0.00 | ties on every date |
| Inventory, finished goods (1310) | −1,810.48 | relief at purchase cost vs FIFO (D36) |
| "COGS (5000)" | 22,755.20 | ledger COGS against QBO's merchant fees (D35, D36) |
| "Manufacturing variance (5100)" | −15,447.91 | QBO relieves into the item's account, here "Purchasing" (D36) |
| Sales income (4100) | −288,573.12 | the income split collapses into 4100 (D35) |
| Returns and allowances (4950) | 2,287.75 | W10 |
| Discounts given (4900) | 155.56 | QBO's account not joined (D35) |

Every dollar of the must tier is accounted for: 133,702.37 on checking is
135,758.35 of `CreditCardPayment` less 2,055.98 of net `Transfer`, the card
accounts carry the other side, equity's 1,000.00 is the one transfer to
distributions, and total income is the D34 items to the cent. The cost
accounts and inventory together net to 596.00, the "Purchasing" item of D34.

### What the four fixes moved (diagnostic run, 10/16)

| Fix | Effect |
|---|---|
| Report as-of dates | every TB and aging above; before it, all five QBO trial balances were 10/17's |
| Card credit | one card: −47,181.96 in July to 0; checking net of it |
| Deposit sweeps undeposited funds | 1150: 54,472.68 to 0.00; checking by the same |
| Shipping-item rule off bills | AP: 133.66 to 0.00 |

### AR and AP aging

QBO's aging totals: AR 89,996.20, 94,703.49, 59,172.98, 86,283.51; AP
46,786.45, 8,469.92, 35,585.33, 30,867.79 (7/31 → 10/16). The QBO AR totals
equal its AR account on the trial balance.

The ledger's own aging (`books::aging`) does not tie and cannot yet: it sees
none of the payment applications, because they target the bare QBO id and
the documents are keyed `qbo:<Type>:<id>` (D32). Its 10/16 AR is 249,691.17
against 86,283.51; the 176,411.08 of window payment applications it missed
accounts for most of that, the rest is invoices open at 6/30 that the
practice run does not carry (the slim import's open-item load does). The
balances themselves tie: with classes out of the way the trial balance's
AR and AP equal QBO's on every date. Re-run the aging after the slim-import
branch merges.

## What is left to decide

| | For | Question |
|---|---|---|
| D33 | Dan, Joel | Where Q1 2027's classes come from: QBO, a ledger item map, or the fallback through Q1 |
| D34 | Joel | Shipping charged (W6), Amazon-collected tax, discount items: follow the item's account or not |
| D35 | Joel | An explicit, signed-off QBO→§2 mapping table instead of the importer's guess |
| D36 | Joel | Inventory relief account and cost: explained may-differ under W10/W12/W22, or match QBO's split |

## What is left to build (not policy)

- qbo-local mirrors `Transfer`, `CreditCardPayment` and `InventoryAdjustment`,
  and the ledger posts them. Until then checking and the cards are red every
  night.
- The ledger's aging joins applications to documents (on the slim-import
  branch).
- Point `ledger nightly` at a window (`--from 2027-01-01`), as the slim-import
  hand-over already notes.

## Running it again

The same commands, a fresh `--db` and `--out`, and the date range moved
forward. A run that finds nothing new is the goal: every variance in it is
either a D-entry Joel has accepted or a bug with a test. The scratch
directory of the 17 Oct run is kept; nothing in it is tracked, because it
carries names and balances (HANDOFF §3.5).
