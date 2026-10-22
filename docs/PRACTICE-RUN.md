# The practice parallel run, Q3 2026

`ROADMAP.md` §E: a run on 2026 data whose only job is to surface posting-policy
differences while there is time to decide them with Joel before the real Q1
2027 run. Two runs on 17 October 2026: the first replayed the window into a
fresh book (below, "First run"); the second, after D37 and the slim import
merged, ran the real D29 shape: `ledger import-slim` at 6/30, then `ledger
nightly --from 2026-07-01` ("Second run"). Decisions: `DECISIONS.md` D32
(what both found, ten bug fixes) and D33–D36, D40 (questions for Dan and
Joel).

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

## First run

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

### AR and AP aging, first run

QBO's aging totals: AR 89,996.20, 94,703.49, 59,172.98, 86,283.51; AP
46,786.45, 8,469.92, 35,585.33, 30,867.79 (7/31 → 10/16). The QBO AR totals
equal its AR account on the trial balance. The first run's own aging could
not be compared: it carried no documents open at 6/30, and its applications
targeted the bare QBO id. Both are fixed by the slim import; see the second
run.

## Second run: the D29 shape, after D37

### Commands

```
# D37's entity types were not in the replica yet (the live daemon is the
# 14 Oct binary). Read read-only from QBO (the QuickBooks MCP server's
# run_query: Transfer, CreditCardPayment, InventoryAdjustment dated 7/1 on),
# saved as JSON, loaded into a copy of the morning's replica backup:
cp "$S/replica.db" "$S/replica-d37.db"
target/release/examples/load_replica_entities --replica "$S/replica-d37.db" \
    --realm "$REALM" --file "$S/d37-entities.json"

# QBO's aging as of 6/30 and each diff date, in qbo-local's aging CSV shape
# (entity_id,name,current,1-30,31-60,61-90,over_90,total), from the MCP's
# report_aged_receivables / report_aged_payables with report_date.

ledger init --db "$S/ledger.db" --company aquamentor --name "Aquamentor, Inc." --realm "$REALM"
ledger import-slim --db "$S/ledger.db" --company aquamentor \
    --replica "$S/replica-d37.db" --realm "$REALM" --as-of 2026-06-30 \
    --qbo-csv "$S/qbo/tb-2026-06-30.csv" \
    --qbo-ar "$S/aging/ar-2026-06-30.csv" --qbo-ap "$S/aging/ap-2026-06-30.csv"
for d in 2026-07-31 2026-08-31 2026-09-30 2026-10-16; do
  ledger nightly --db "$S/ledger.db" --company aquamentor \
      --replica "$S/replica-d37.db" --realm "$REALM" \
      --qbo-csv "$S/qbo/tb-$d.csv" --out "$S/nightly" --from 2026-07-01 --as-of $d
  target/release/ledger-aging --db "$S/ledger.db" --company aquamentor \
      --as-of $d --out "$S/ledger-aging"
done
```

`ledger nightly` applies D28's fallback class to anything dated before
1/1/27, so in this window it is the first run's diagnostic mode: class is
out of the way. The full-gate numbers above still stand for what an
unclassed Q1 2027 would look like.

Timings: slim import 5 s (10,128 customers, 1,280 vendors, 3,474 items, 585
open documents); each nightly 1–2 s.

### The slim import at 6/30

On the merged code its checks failed: AR open items 623.02 over 1200, AP
open items 2,555.19 over 2000, two customers and eight vendors off QBO's
aging. Each difference traced to a QBO shape the import did not read (D32
items 5–8). After the fixes every check passes: no 3950 plug, open items
tie to 1200 and 2000, AR and AP aging match QBO's by contact and by bucket,
and the trial balance matches on every must-tier line.

### Trial balance, nightly

0 rejected on every night. Must tier (ledger − QBO):

| | 7/31 | 8/31 | 9/30 | 10/16 | Cause |
|---|---:|---:|---:|---:|---|
| Checking (1100) | 0.00 | 0.00 | 0.00 | 0.00 | D37 closed it (was 133,702.37 at 10/16) |
| AR (1200+2300) | 0.00 | 0.00 | 0.00 | 0.00 | |
| AP (2000+2050) | 0.00 | 0.00 | 0.00 | 0.00 | |
| Sales tax payable (2200) | −4.64 | −4.64 | −150.32 | −405.32 | mapping (D35) |
| Total income | −1,835.95 | −5,605.68 | −7,909.30 | −10,322.98 | items pointed at non-income accounts (D34) |
| Total equity | 0.00 | 0.00 | 0.00 | 0.00 | D37 closed it (was −1,000.00) |

Per account, every bank, card, loan and distribution account now ties to the
cent on every date. The may tier and the income split are as in the first
run's diagnostic table (D35, D36). The one valued inventory adjustment now
posts; it lands in 5150 where QBO has its own "Inventory Shrinkage" (5151
in the ledger), 100.00 each way (D35).

### AR and AP aging by contact

| | 7/31 | 8/31 | 9/30 | 10/16 |
|---|---:|---:|---:|---:|
| QBO AP aging | 46,786.45 | 8,469.92 | 35,585.33 | 30,867.79 |
| Ledger AP aging | 46,786.45 | 8,469.92 | 35,585.33 | 30,867.79 |
| Vendors with a balance / differing | 32 / 0 | 26 / 0 | 30 / 0 | 28 / 0 |
| QBO AR aging | 89,996.20 | 94,703.49 | 59,172.98 | 86,283.51 |
| Ledger AR aging | 94,838.51 | 99,545.80 | 64,390.80 | 91,201.41 |
| Customers with a balance / differing | 68 / 12 | 69 / 12 | 74 / 15 | 78 / 14 |
| Difference | 4,842.31 | 4,842.31 | 5,217.82 | 4,917.90 |

**AP matches QBO for every vendor on every date.** Before D32 item 10 it was
off 631.51 (one vendor) at 7/31 and 781.51 (two) at 8/31.

**AR differs only by customer credits** the ledger holds in 2300 under W8,
which its aging does not show (D40). That is 4,771.39 of payments unapplied
at 6/30 (the slim import's reclass), the window's own unapplied payments,
and at 9/30 one payment dated before its invoice (299.92). AR + 2300 ties
to QBO's AR on every date.

## What is left to decide

| | For | Question |
|---|---|---|
| D33 | Dan, Joel | Where Q1 2027's classes come from: QBO, a ledger item map, or the fallback through Q1 |
| D34 | Joel | Shipping charged (W6), Amazon-collected tax, discount items: follow the item's account or not |
| D35 | Joel | An explicit, signed-off QBO→§2 mapping table instead of the importer's guess |
| D36 | Joel | Inventory relief account and cost: explained may-differ under W10/W12/W22, or match QBO's split |
| D40 | Dan, Joel | Should the own AR aging show unapplied customer money as a credit, as QBO does |

## What is left to build (not policy)

- A live `qbo-local` sync of D37's three types (Dan, at the keychain prompt).
  This run loaded them by hand into a scratch copy; the live replica still
  lacks them.
- Valuing inventory adjustments. 27 of the 28 in the window were for items
  with no purchase cost and posted nothing; QBO's shrinkage account moved by
  only the one valued adjustment's 100.00, so it ties, but the ledger is
  using the item's purchase cost where QBO uses its own (D37 open item 2).

## Running it again

The second run's commands, a fresh `--db` and `--out`, and the dates moved
forward. A run that finds nothing new is the goal: every variance in it is
either a D-entry Joel has accepted or a bug with a test. The scratch
directory of the 17 Oct run is kept; nothing in it is tracked, because it
carries names and balances (HANDOFF §3.5).
