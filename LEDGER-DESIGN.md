# LEDGER-DESIGN.md, the book of record that is not QuickBooks

First draft, 11 September 2026, for Dan's and Joel's review. Companion to
`DESIGN.md` (the `qbo-local` replica this ledger imports from), `ROADMAP.md`
(the phases and the cutover) and `DECISIONS.md` (why).

This document leads with the posting-rules table because that table is
accounting policy rather than engineering judgement. Everything marked
**⚠️ Dan/CPA to confirm** is a decision that is not mine to make, and each one
states what changes if the answer is no. They are collected in §13.

All names and figures used as examples are the prototype's fictional ones.

---

## 0. Scope, and the cutover it serves

Two legal entities, Aquamentor and WaterLine CNC, separate books, one binary,
no cross entity join anywhere. Fiscal year is the calendar year for both (D7).
Cutover target 1 January 2027, fallback 1 January 2028, go/no-go 1 November
2026 (D16, ROADMAP §F).

Until cutover, QBO is the book of record and this ledger runs in parallel
(ROADMAP §E). From Phase D the local document store is the source of truth and
QBO is a downstream mirror kept current through the outbox for the CPA and the
bank feeds (ROADMAP §C, §D). Decoupling is switching the outbox off. The
direction of authority flips; the code does not change.

**Basis.** The books are kept on the **accrual** basis. Revenue posts when a
document that transfers goods or services is saved, cost posts when the
liability is incurred, and nothing waits for cash. Cash basis is a **report
time transformation** over the same ledger: a report run with `basis: Cash`
re-attributes each accrual entry to the date money moved, following the payment
and bill payment links, and drops unpaid AR and AP entirely. There is one
ledger, not two.

⚠️ **Dan/CPA to confirm** (W1): the books are accrual and cash basis is a
report time transformation, with cash basis output expected to be *materially*
equal to QBO's rather than reconcilable to the cent. If no, and cash basis must
tie to QBO exactly, the report layer needs a second attribution path with its
own golden file tests for partial payments spanning a period end, which is a
milestone of work rather than a parameter.

The kickoff brief made both bases first class parameters on every report from
the first report written (`docs/CLAUDE_CODE_KICKOFF_PROMPT.md` §8). That holds:
`basis` is in every report signature from day one. What W1 settles is the
accuracy bar for the cash side, not whether it exists.

---

## 1. The posting-rules table

One function turns a document into a balanced entry. There is no other writer
to `journal_lines` (§4). If a rule here is wrong it is wrong everywhere and
consistently, which is at least findable.

Account numbers refer to §2. Class on every line follows the rule in §3.

| Document | Debit | Credit | When it posts | Tax treatment | Class carried from | Notes |
|---|---|---|---|---|---|---|
| **Estimate** | none | none | never | quoted tax is display only | header, defaulted from item | Non posting. The contract, not a transaction. Progress invoicing bills it across several invoices and it stays open until fully billed. |
| **Invoice** | 1200 AR, total including tax | 4100 sales by line, 4300 shipping, 2200 tax collected | on save | tax on each line where `is_taxable`, at the rate in force on `txn_date` | line, defaulted from item | Inventory lines also post 5000 Dr and 1310 Cr at unit cost (§11). Service lines post no COGS. |
| **Sales receipt** | 1150 undeposited funds, or the named bank account | 4100, 4300, 2200 | on save | as invoice | line | Paid at the point of sale, so no AR leg. Shopify and Amazon orders arrive this way. |
| **Credit memo** | 4950 returns and allowances, 2200 tax reversed | 1200 AR | on save | mirrors the invoice it credits, at the original rate | line, copied from the source document | If goods physically return, also 1310 Dr and 5000 Cr at the original unit cost. Application to an invoice is an allocation inside AR and posts nothing further. |
| **Refund receipt** | 4950, 2200 tax reversed | 1100 bank, or 1160 for a card refund | on save | as above | line | Money out with no AR leg. |
| **Payment** | 1150 undeposited funds, or the named bank account, or 1160 for a card | 1200 AR per applied link, 2300 customer deposits for any unapplied remainder | on receipt | none, tax was recognised on the invoice | inherited from the invoices it applies to, pro rata | An unapplied remainder is never spread across invoices by guess. It sits in 2300 until Dan applies it. |
| **Purchase order** | none | none | never | none | header | Non posting. Receipt against it is what posts, below. |
| **Bill** | 1300 raw materials, 1310 goods for resale, or the line's expense or COGS account | 2000 AP | on the bill date, when the bill is entered | vendor charged tax is part of the cost and is not recoverable, so no 2200 leg | line, defaulted from item, else from the expense account's default | W2. Where a raw material receipt already posted, the bill debits 2050 instead of 1300 and clears the accrual. |
| **Bill payment** | 2000 AP | 1100 bank or 2100 credit card | on save | none | inherited from the bills paid | Method (ACH, check, card) selects the credit account. |
| **Vendor credit** | 2000 AP | 1300 material cost | on the credit date, when issued | none | inherited from the bill or PO it relates to | W3. Application to a specific bill is an allocation inside AP and posts nothing, which corrects the prototype. |
| **Purchase (expense, check, card charge)** | 6100 to 6900 by account, or 1300 where it is material | 1100 bank or 2100 credit card | on save | none | line, optional on overhead, required on anything a product line consumes | Non PO spend. Fuel, packaging, licences, refuse removal. |
| **Deposit** | 1100 bank | 1150 undeposited funds per grouped payment, 1160 per settlement batch, 4990 other income for a direct line | on save | none | inherited from the payments grouped | A fee netted inside the deposit posts 6700 Dr in the same entry so the bank line matches to the cent. |
| **Journal entry** | as stated on the entry | as stated on the entry | on approval | none | required on every line touching 4xxx, 5xxx or 6xxx | There is no new journal entry button. The only two sources are a QBO import (§6) and an approved adjusting entry request (§8). Both are flagged `source_type = 'manual'` and appear in every audit list. |
| **Raw material receipt** | 1300 raw materials at landed cost | 2050 inventory received not billed | on receipt of the goods | none | line, from the PO | Landed cost is allocated by board foot (§11). Goods on the floor before the bill arrives are inventory, not nothing. |
| **Build / assembly** | 1310 finished goods, quantity at build sheet unit cost | 1300 materials at landed cost for board feet actually consumed, 5300 parts and labour applied at standard | on completion of the build | none | from the finished item | §11. |
| **Build variance** | 5100 manufacturing variance, when the build consumed more than the sheet allowed | 5100, when it consumed less | same entry as the build | none | as the build | W4. It is the plug line that makes the build entry balance, not a separate document. |
| **Inventory adjustment** | 1300 or 1310 on a count up, 5150 on a count down | 5150 on a count up, 1300 or 1310 on a count down | on save | none | required, from the item | Shrinkage, damage, a physical count. A reason code on every line. |
| **Bank statement line** | none | none | never | none | n/a | Non posting. A matched line stamps `cleared_at` on the entry it matched. An unmatched line proposes a Purchase or a Deposit which posts only when Dan confirms it (§10). |
| **Authorize.net settlement** | 1100 bank, net payout | 1160 Authorize.net clearing, gross of the batch | on settlement, from the settlement import | none | inherited from the payments in the batch | The customer payment already debited 1160 when it settled. This entry moves the batch to the bank. |
| **Authorize.net fee** | 6700 merchant and bank fees | 1160 Authorize.net clearing | on settlement, same entry as the payout | none | none, it is overhead | W13. Gross revenue with a separate fee expense, never revenue booked net. |

### The three policies from ROADMAP §B2

⚠️ **Dan/CPA to confirm** (W2): **raw material bills capitalise to inventory
(1300) rather than expensing to COGS on purchase.** Foam bought in August and
cut in October is an asset in August. If no, and raw material is expensed on
purchase, 1300 holds only work in progress, gross margin swings with buying
rather than with selling, and the costing in §11 loses the account it relieves.

⚠️ **Dan/CPA to confirm** (W3): **a vendor credit reduces material cost (Cr
1300) rather than posting to other income.** Two delaminated sheets on receipt
EC-11902 means the sheets really did cost less, not that the shop earned
something. If no, credits post to 4990 and every landed cost per board foot in
§11 is overstated by the credits taken against it.

⚠️ **Dan/CPA to confirm** (W4): **build variance plugs to 5100 manufacturing
variance rather than being spread back over unit cost.** A build that runs over
is a signal, and averaging it into the unit cost hides the signal inside the
number it corrupts. If no, and variance is absorbed, finished goods carry
actual rather than standard cost, 5100 and 5300 both disappear, and the build
entry becomes a straight transfer at actual, which is simpler and tells you
nothing about whether a recipe is lying.

*Rejected on all three: settling them in code and describing them in a comment.
These are the three numbers a CPA would change first, so they are written where
a CPA can read them.*

### The policies the table forces

⚠️ **Dan/CPA to confirm** (W5): **sales tax collected is a liability (2200) at
the invoice date, not at the payment date.** If no, and the filing is on a cash
basis, the report in §9 needs a second attribution path and B stops being a
simple sum over 2200.

⚠️ **Dan/CPA to confirm** (W6): **shipping charged to a customer is income
(4300), not a contra to freight expense.** It shows what the shop charged and
what it paid as two numbers rather than one net figure that hides both. If no,
freight charged credits 5050 freight in, gross revenue falls by the shipping
billed, and the A line in §9 moves.

⚠️ **Dan/CPA to confirm** (W7): **freight charged to a customer is taxable in
New Jersey when the goods on the invoice are taxable.** If no, the shipping
line is always non taxable and the taxable sales check line in §9 moves by the
freight on every taxable order.

⚠️ **Dan/CPA to confirm** (W8): **customer payments land in undeposited funds
(1150) by default, not directly in the bank.** The bank statement shows one
deposit of four checks, and 1150 is what lets one statement line match one
ledger entry (§10). If no, and payments go straight to the bank, every deposit
becomes four statement lines to match and the per statement close gets harder
rather than easier.

⚠️ **Dan/CPA to confirm** (W9): **a discount is a contra income account (4900),
not a reduction of the revenue line.** Gross sales stay visible, which the A
line in §9 depends on. If no, discounts net against 4100 and total income falls
by the discounts given.

⚠️ **Dan/CPA to confirm** (W10): **credit memos and refunds debit returns and
allowances (4950) rather than the original income account.** A returns figure
you can read off the P&L is worth an account. If no, they debit 4100 by class
and returns become a query rather than a line.

⚠️ **Dan/CPA to confirm** (W11): **bad debt is written off direct to expense
(6800) on Dan's explicit instruction, with no allowance account.** If no, 1250
allowance for doubtful accounts comes into use and a periodic provision entry
joins the close checklist in §5.

⚠️ **Dan/CPA to confirm** (W12): **inventory is valued at moving weighted
average, not FIFO.** Recommended, and here is why for foam specifically. Blue
XLPE arrives in lots of a few sheets at a time from Continental Foam at prices
that move with resin and with freight, so a sheet in the rack has no identity
once it is in the rack. Board feet are consumed out of a pile, not out of a
lot, and a cut part is nested across whatever is on the floor. FIFO would
require lot tracking on a material that is physically fungible, to produce a
cost that differs from the average by a rounding error, and it would make the
landed cost per board foot in §11 a lot level figure rather than the single
number every build sheet reads. Weighted average is one number per material per
entity, recomputed on each receipt, and it is the number the shop already
thinks in. If no, and FIFO is required, `material_lots` gains a consumption
order, every build records which lots it drew from, and the costing engine in
§11 becomes lot aware, which is the largest single cost item in this document.
The kickoff brief asked for FIFO to sit behind a trait so it can be swapped;
that stands, and the trait has one implementation until W12 says otherwise.

⚠️ **Dan/CPA to confirm** (W13): **merchant fees are recorded gross, as 6700
expense, rather than netted against revenue.** If no, revenue is booked net of
fees, the A line in §9 falls by the fees, and the deposit no longer ties to the
settlement batch gross.

---

## 2. Chart of accounts

The minimum set the table above needs. Numbers are four digits in classification
bands, with room between them, because a chart that has to renumber to admit an
account is a chart people work around.

| No. | Account | Classification | Notes |
|---|---|---|---|
| 1100 | Checking | Asset | One per real bank account, 1101, 1102, and so on. |
| 1150 | Undeposited funds | Asset | W8. The clearing account between a payment and a deposit. |
| 1160 | Authorize.net clearing | Asset | Settled but not yet paid out. Nets to zero after each batch. |
| 1200 | Accounts receivable | Asset | Subledger is the documents, never posted to by hand. |
| 1250 | Allowance for doubtful accounts | Asset, contra | Unused unless W11 says otherwise. |
| 1300 | Inventory, raw materials | Asset | Foam, aluminium, HDPE, hardware. |
| 1310 | Inventory, finished goods | Asset | Built product waiting to ship. |
| 1320 | Work in progress | Asset | After cutover, when builds span a period end (§11). |
| 1400 | Prepaid expenses | Asset | |
| 1500 | Machinery and equipment | Asset | |
| 1590 | Accumulated depreciation | Asset, contra | Joel's annual entry. |
| 2000 | Accounts payable | Liability | |
| 2050 | Inventory received not billed | Liability | Goods on the floor, bill not yet arrived. |
| 2100 | Credit card | Liability | One per card. |
| 2200 | Sales tax payable | Liability | The §9 B line reads this account. |
| 2300 | Customer deposits and unapplied payments | Liability | Money received that no invoice claims yet. |
| 2400 | Payroll liabilities | Liability | SurePayroll summary journal only (§12). |
| 2900 | Notes and loans payable | Liability | |
| 3000 | Owner capital | Equity | |
| 3100 | Owner draws | Equity, contra | |
| 3900 | Retained earnings | Equity | |
| 3950 | Opening balance equity | Equity | Import only (§6). Must be zero before the first close after import. |
| 4100 | Sales income | Income | Split by class, not by account. |
| 4300 | Shipping income | Income | W6. |
| 4900 | Discounts given | Income, contra | W9. |
| 4950 | Returns and allowances | Income, contra | W10. |
| 4990 | Other income | Income | |
| 5000 | Cost of goods sold | COGS | |
| 5050 | Freight in | COGS | Only where freight is not capitalised into landed cost. |
| 5100 | Manufacturing variance | COGS | W4. |
| 5150 | Inventory adjustment and shrinkage | COGS | |
| 5300 | Parts and labour applied | COGS, contra | Only ever carries a credit balance, so the trial balance carries a contra flag and does not warn on it. |
| 5400 | Direct labour | COGS | The wages side that 5300 relieves. |
| 6100 | Shop supplies and packaging | Expense | |
| 6200 | Vehicle and fuel | Expense | |
| 6300 | Rent and utilities | Expense | |
| 6400 | Insurance | Expense | |
| 6500 | Professional fees | Expense | |
| 6600 | Advertising and channel fees | Expense | Amazon, Shopify, Google. |
| 6700 | Merchant and bank fees | Expense | W13. |
| 6800 | Bad debt | Expense | W11. |
| 6900 | Other operating expense | Expense | |
| 7100 | Interest expense | Expense | |

`normal_balance` follows from the classification, except where `is_contra` is
set, which flips it. Without that flag 5300 and 1590 would be reported as wrong
on every trial balance, and a warning that is always on is noise.

### Mapping the QBO chart onto it

The replica's `accounts` table already carries `acct_num`, `account_type`,
`account_subtype` and `classification` from `AccountRef` payloads
(`apps/qbo-local/src/project.rs`, `ParsedAccount`). The mapping is a table, not
code:

| QBO field | Use |
|---|---|
| `classification` | Selects the band. Asset to 1xxx, Liability to 2xxx, Equity to 3xxx, Revenue to 4xxx, Expense to 5xxx or 6xxx by subtype. |
| `account_type` and `account_subtype` | Selects the account inside the band. `AccountsReceivable` to 1200, `UndepositedFunds` to 1150, `CostOfGoodsSold` to 5000, and so on. |
| `acct_num` | Preferred when the QBO chart already numbers an account, so a hand mapping is only needed where it does not. |
| `qbo_id` | Kept as `accounts.source_ref` forever. It is the join key for the trial balance diff in §7 and it is how a 2014 invoice still resolves its accounts. |

**The rule for a QBO account with no counterpart: it is created, never
dropped.** An unmapped QBO account is materialised in the x990 to x999 slot of
its classification band, named as QBO names it, with `source_ref` set and
`needs_mapping = 1`. The import does not stop, because stopping on the first
oddity in a chart built over fourteen years means never finishing an import.
What does stop is the sign off: the §6 import cannot be accepted while any
account with `needs_mapping = 1` carries a non-zero balance. Either it gets a
real mapping or Joel accepts it as its own account.

⚠️ **Dan/CPA to confirm** (W14): the numbering scheme above, and whether Joel
wants QBO's account names preserved verbatim for comparability with the FY2026
books rather than renamed. If no, the mapping table gains a rename column and
the §7 diff report prints both names side by side.

*Rejected: adopting QBO's chart wholesale. It carries accounts that exist only
because QBO's automated sales tax and undeposited funds machinery put them
there, and importing those is importing the machinery's assumptions.*

---

## 3. Class taxonomy

`DESIGN.md` §2.4 captures class on every document header and every document
line, precisely so this section has something to work with. A class not
captured in the replica is a class lost here, and it cannot be backfilled
without a re-import.

### The list

Aquamentor, six classes, as the prototype uses them:

| Class | Covers |
|---|---|
| `foam` | Foam products. Rescue tubes, floating mats, kickboards, dock bumpers. |
| `sign` | Signs. Aluminium pool rules signs and custom signage. |
| `chair` | Lifeguard chairs. |
| `drop` | Dropship and resale. Goods bought and shipped by a vendor against a customer order. |
| `cnc` | CNC cutting sold as job work. |
| `uv` | UV printing sold as job work. |

WaterLine CNC, two classes, `cnc` and `uv`, in its own book with its own ids.
The two entities share no class table, as they share nothing else.

⚠️ **Dan/CPA to confirm** (W15): the six classes above are the list, and `cnc`
and `uv` appear in both books rather than only in WaterLine's. If no, the class
table changes before import rather than after, because re-classing history is a
re-import.

### The rule

**Every line that touches income, COGS or a class consuming expense carries
exactly one class.** Balance sheet lines (AR, AP, bank, inventory, tax payable)
do not, because a receivable belongs to a customer rather than to a product
line, and forcing a class there would invent an allocation.

The rule is enforced at the posting gate, in the same function that checks the
period (§5). A document line with no class and no class derivable from its item
does not post. The gate is the only enforcement point, so there is no second
path that forgets.

Default chain, first hit wins: explicit class on the line, then the item's
default class, then the document header's class, then reject.

### Imports with no class

The live QBO book has documents whose lines carry no `ClassRef`, and pretending
otherwise at import would be inventing data.

⚠️ **Dan/CPA to confirm** (W16): at import, a line with no class takes the
default class of its item, and only a line whose item also has no default is
quarantined for Dan to class by hand. The alternative is quarantining every
unclassed line, which is honest and may be thousands of rows. If the item
default is used, a 2016 invoice may carry a class Dan assigned to that SKU in
2026, which is a reasonable reading of history and is still a reading. Whichever
way it goes, imported lines that were classed by default rather than by the
payload carry `class_source = 'item_default'` so a report can exclude them and
the choice is visible forever.

---

## 4. Ledger schema

SQLite, same discipline as `DESIGN.md` §3: `rusqlite` bundled, WAL,
`foreign_keys=ON`, `synchronous=FULL` for ledger writes, `STRICT` tables,
forward only numbered migrations, money as `INTEGER` minor units, decimals as
text.

```sql
CREATE TABLE companies (
    company_id        TEXT PRIMARY KEY,          -- 'aquamentor', 'waterline'
    display_name      TEXT NOT NULL,
    fiscal_year_start TEXT NOT NULL DEFAULT '01-01',   -- D7
    realm_id          TEXT,                      -- the QBO realm it imports from
    created_at        TEXT NOT NULL
) STRICT;

CREATE TABLE accounts (
    company_id      TEXT NOT NULL REFERENCES companies(company_id),
    account_id      TEXT NOT NULL,
    number          TEXT NOT NULL,
    name            TEXT NOT NULL,
    classification  TEXT NOT NULL,   -- Asset|Liability|Equity|Income|COGS|Expense
    subtype         TEXT,
    parent_id       TEXT,
    normal_balance  TEXT NOT NULL CHECK (normal_balance IN ('Dr','Cr')),
    is_contra       INTEGER NOT NULL DEFAULT 0,
    is_active       INTEGER NOT NULL DEFAULT 1,
    needs_mapping   INTEGER NOT NULL DEFAULT 0,
    source_ref      TEXT,            -- QBO account id, kept forever
    PRIMARY KEY (company_id, account_id)
) STRICT;

CREATE TABLE classes (
    company_id  TEXT NOT NULL REFERENCES companies(company_id),
    class_id    TEXT NOT NULL,
    name        TEXT NOT NULL,
    parent_id   TEXT,
    is_active   INTEGER NOT NULL DEFAULT 1,
    source_ref  TEXT,
    PRIMARY KEY (company_id, class_id)
) STRICT;

CREATE TABLE periods (
    company_id     TEXT NOT NULL REFERENCES companies(company_id),
    period_end     TEXT NOT NULL,          -- last day of the month
    state          TEXT NOT NULL CHECK (state IN ('open','closed')),
    closed_at      TEXT,
    closed_by      TEXT,
    tb_snapshot_id TEXT,                   -- §5
    PRIMARY KEY (company_id, period_end)
) STRICT;

CREATE TABLE close_history (
    company_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    at          TEXT NOT NULL,
    actor       TEXT NOT NULL,
    moved_from  TEXT NOT NULL,             -- previous locked_through
    moved_to    TEXT NOT NULL,
    is_reopen   INTEGER NOT NULL DEFAULT 0,
    note        TEXT NOT NULL,
    PRIMARY KEY (company_id, seq)
) STRICT;

CREATE TABLE document_versions (
    company_id    TEXT NOT NULL,
    document_id   TEXT NOT NULL,
    version       INTEGER NOT NULL,
    doc_type      TEXT NOT NULL,
    doc_number    TEXT,
    txn_date      TEXT NOT NULL,
    contact_id    TEXT,
    payload_json  TEXT NOT NULL,           -- the document exactly as saved
    created_at    TEXT NOT NULL,
    command_id    TEXT NOT NULL,           -- the oplog command that produced it
    source_ref    TEXT,                    -- QBO id, when imported or mirrored
    PRIMARY KEY (company_id, document_id, version)
) STRICT;

CREATE TABLE journal_entries (
    company_id      TEXT NOT NULL,
    entry_id        TEXT NOT NULL,         -- UUIDv7
    entry_date      TEXT NOT NULL,
    posted_at       TEXT,
    memo            TEXT,
    source_type     TEXT NOT NULL,         -- 'invoice','bill','build','manual',...
    source_id       TEXT NOT NULL,
    source_version  INTEGER NOT NULL,
    reversal_of_id  TEXT,
    is_posted       INTEGER NOT NULL DEFAULT 0,
    is_flagged      INTEGER NOT NULL DEFAULT 0,   -- manual and imported JE
    PRIMARY KEY (company_id, entry_id),
    FOREIGN KEY (company_id, source_id, source_version)
        REFERENCES document_versions(company_id, document_id, version)
) STRICT;

CREATE TABLE journal_lines (
    company_id    TEXT NOT NULL,
    entry_id      TEXT NOT NULL,
    line_no       INTEGER NOT NULL,
    account_id    TEXT NOT NULL,
    class_id      TEXT,
    debit_minor   INTEGER NOT NULL DEFAULT 0 CHECK (debit_minor  >= 0),
    credit_minor  INTEGER NOT NULL DEFAULT 0 CHECK (credit_minor >= 0),
    memo          TEXT,
    entity_type   TEXT,
    entity_id     TEXT,
    cleared_at    TEXT,                    -- §10, set by statement matching
    CHECK (debit_minor = 0 OR credit_minor = 0),
    CHECK (debit_minor + credit_minor > 0),
    PRIMARY KEY (company_id, entry_id, line_no),
    FOREIGN KEY (company_id, entry_id)
        REFERENCES journal_entries(company_id, entry_id),
    FOREIGN KEY (company_id, account_id)
        REFERENCES accounts(company_id, account_id)
) STRICT;

CREATE INDEX idx_lines_account ON journal_lines(company_id, account_id);
CREATE INDEX idx_entries_date  ON journal_entries(company_id, entry_date);
```

### The invariants, and where each is enforced

| Invariant | Enforced by |
|---|---|
| Exactly one of debit and credit is non-zero, and neither is negative | Three `CHECK` constraints, above. Unbypassable, including from `sqlite3`. |
| Every entry balances | The posting transition. Lines are inserted while `is_posted = 0`; the `UPDATE ... SET is_posted = 1` fires a `BEFORE UPDATE` trigger that recomputes `SUM(debit_minor) - SUM(credit_minor)` for the entry and calls `RAISE(ABORT)` on any non-zero result. An entry that never balances never posts, and an unposted entry appears in no report. |
| A posted entry is never updated or deleted | `BEFORE UPDATE` and `BEFORE DELETE` triggers on both tables that `RAISE(ABORT)` when `is_posted = 1`, with the single exception of `cleared_at` on `journal_lines`, which is reconciliation state rather than accounting content. |
| Every line's account and class belong to the entry's company | Composite foreign keys carrying `company_id`, above. There is no single column key to join across companies by accident. |
| No entry without a source document | `journal_entries.source_id` and `source_version` are `NOT NULL` with a composite foreign key into `document_versions`. There is no new journal entry button, as the prototype says. |
| Corrections are reversing entries | The posting function's only correction path writes a new entry with `reversal_of_id` set and the sides swapped, then posts the replacement. Nothing is edited in place. |

The one exception to "no entry without a source document" is a **QBO
JournalEntry imported in §6**. It arrives with no originating document because
in QBO it *was* the originating document. It is admitted as its own
`document_versions` row of type `JournalEntry`, with `is_flagged = 1` on the
entry, and every report and audit list can filter on that flag.

⚠️ **Dan/CPA to confirm** (W18): imported QBO journal entries are accepted as
written and flagged, rather than being re-derived or refused. If no, every
imported JE needs a hand mapping before the import completes, which is work
proportional to fourteen years of Joel's adjusting entries.

### Commands and the oplog

Every state change is a serialisable command appended to an oplog, exactly as
the kickoff brief specifies, and the tables above are a projection of it.

```sql
CREATE TABLE oplog (
    company_id   TEXT NOT NULL,
    command_id   TEXT NOT NULL,     -- UUIDv7, monotonic by construction
    actor_id     TEXT NOT NULL,
    hlc          TEXT NOT NULL,     -- hybrid logical clock stamp
    kind         TEXT NOT NULL,     -- 'save_invoice','post_build','close_period',...
    payload_json TEXT NOT NULL,
    applied_at   TEXT NOT NULL,
    PRIMARY KEY (company_id, command_id)
) STRICT;
```

A command and everything it produces commit in **one transaction**: the oplog
row, the `document_versions` row, the journal entry and its lines, and the
outbox record that mirrors the document to QBO. Replay of the oplog from
scratch must reproduce the tables byte for byte.

**How that interacts with the outbox.** The outbox (`DESIGN.md` §6) is a
*consumer* of commands, not a second log. Two rules keep them from fighting:

1. A replay is marked as a replay, and outbox enqueueing is suppressed during
   one. Otherwise rebuilding the projection would re-send every document this
   business has ever written to Intuit.
2. The outbox's own state transitions (`in_flight`, `sent`, `rejected`) are
   **not** commands. They are mirror state about a remote system, not facts
   about the book, and an oplog that carries them cannot be replayed on a
   machine that never made those calls.

At cutover the outbox is switched off and the oplog is unchanged, which is the
test that the split was drawn in the right place.

*Rejected: making the outbox the oplog. They have different lifetimes, the
outbox ends at cutover and the oplog is forever, and different truth, one holds
intent and the other holds what Intuit said.*

### Money and rounding

`crates/ledger-core` as it stands (D3): `Money(i64)` minor units, checked
arithmetic returning `Result`, `round_money` the only `Decimal` to `Money`
conversion point, `RoundingPolicy::{LineExtension, TaxCalculation,
MirroredAmount}` (D8). No fourth policy is added here. `MirroredAmount` is used
by the importer in §6 and by nothing else.

⚠️ **Dan/CPA to confirm** (W17, engineering to verify first): `DESIGN.md` §12
item 3 is still open. Tax is rounded half to even, and whether QBO rounds tax
half up was asserted and never verified. It did not matter while `qbo-local`
only mirrored the tax QBO had already computed. It matters here, because this
ledger computes tax itself and then diffs against QBO nightly (§7). If QBO
rounds half up, every taxable invoice with a half cent tax remainder produces a
one cent variance in 2200 and in AR, which will bury the §7 report in noise
that is not error.

---

## 5. Period close

D7, exactly. A closed period **rejects** posting. Not a warning, not a
confirmation dialog. The posting gate asks the period question before it does
anything, and a document dated on or before `locked_through` does not post, is
not partially written, and produces an error that names the date and the lock.

Reopening is permitted, because a bill from Polymer Source genuinely does
arrive in May for April. It is never quiet. Each reopen writes its own row into
`close_history` with `is_reopen = 1`, the actor and a required note, and every
close history view renders those rows flagged. A period that was reopened can
never be made to look like one that was never touched.

⚠️ **Dan/CPA to confirm** (W19): after Joel signs off a fiscal year, that year
gets a second lock that a normal reopen cannot lift, requiring an explicit
year-unlock action with its own history row. If no, a year end is reopenable by
the same one click as a month end, and the audit trail is the only protection.

### The close checklist

Closing a period runs these in order, and a failure stops the close rather than
warning. `locked_through` moves only when all of them pass.

| # | Check | Why it stops the close |
|---|---|---|
| 1 | Every bank and credit card statement covering the period is imported and reconciled, beginning balance plus cleared lines equals ending balance (§10) | A period closed over an unreconciled bank account is a period you will reopen. |
| 2 | No unposted documents dated in the period | An invoice saved but never posted is revenue that does not exist yet. |
| 3 | No quarantined imports dated in the period (`DESIGN.md` §3.5) | A quarantined row is a known unknown, and closing over it makes it an unknown unknown. |
| 4 | No unapplied customer payments older than the period end without a reason note | 2300 is a parking space, not a destination. |
| 5 | The sales tax liability report for the period is generated and frozen (§9) | It is the one report Dan files from, and it must be the version that was filed, not a later recomputation. |
| 6 | A trial balance snapshot is stored, dated, immutable, keyed as `tb_snapshot_id` | The close is the claim. The snapshot is the evidence, and the §7 diff compares against it. |
| 7 | 3950 opening balance equity is zero, on the first close after an import | A non-zero 3950 means the import did not fully resolve (§6). |

⚠️ **Dan/CPA to confirm** (W20): all seven are hard gates rather than
advisories, including 4. If no, and some are advisory, say which, and the close
records which advisories were overridden and by whom.

---

## 6. Import from the replica

The replica is the import source and is **never modified** by the import.
Import runs are idempotent and re-runnable: the same replica state produces the
same ledger, and a re-run replaces the derived entries rather than adding to
them. Every ledger row keeps the QBO id in `source_ref`.

Documents are read entity by entity from `entities.raw_json` and posted through
**the same posting function** the UI uses (ROADMAP §B' step 4). History in the
new book is derived, not copied. That is the point: if the posting rules in §1
are wrong, the import is where it shows, loudly, against a book that has
fourteen years of known answers.

### Full history from where, opening balances before

**Recommended: full history from 1 January of the earliest year where the QBO
trial balance and the replayed one agree, and a single opening balance entry as
of 31 December of the year before that.**

How to find that year, mechanically:

1. Pull QBO's `TrialBalance` report as of 31 December for every year with data,
   2012 through 2025, accrual basis. These are cheap report calls and they are
   cached.
2. Start with the most recent closed year, Y = 2025. Replay only year Y's
   documents on top of an opening balance taken from QBO's trial balance as of
   31 December Y-1. Compare the replayed 31 December Y trial balance against
   QBO's, account by account, on the §7 tolerance rules.
3. If it agrees, step back a year and repeat. If it does not, the previous Y is
   the boundary.
4. The answer is the earliest Y such that Y and every year after it agree. Full
   history is replayed from 1 January of that Y. Everything before it becomes
   one opening balance entry dated 31 December Y-1, debits and credits taken
   from QBO's trial balance, balanced by construction, with 3950 opening balance
   equity as the plug that must come out zero.

The walk is backwards, not forwards, because the recent years are the ones Dan
will actually query and because each failing year tells you what changed, which
is usually a QBO feature switching on. Expect the boundary to land where
automated sales tax or inventory tracking was turned on.

⚠️ **Dan/CPA to confirm** (W21): the boundary year the procedure finds, and
that Joel accepts one opening balance entry for everything before it rather
than requiring full history back to 2012. If no, and full history is required
regardless, every year before the boundary needs its variances individually
explained and accepted, which is unbounded work against a book nobody will
query.

*Rejected: opening balances per year, as the kickoff brief explicitly rejected.
A system that cannot answer what blue XLPE cost per board foot in 2023 is not a
replacement.*

### What is already parsed, and what the import must read from raw JSON

`project.rs` gives the import most of a document for free:

| Already parsed | Where |
|---|---|
| Document header: type, number, `txn_date`, `due_date`, contact and contact type, header class, total, balance, status, customer PO, memos, currency | `ParsedDocument` |
| Lines: line number, item id, description, qty, unit price, amount, line class, `is_taxable` | `ParsedLine` |
| Links: `TxnId` and `TxnType`, header or line (D11) | `ParsedLink` |
| Items: income, expense and asset account refs, type, cost, price | `ParsedItem` |
| Accounts: number, type, subtype, classification | `ParsedAccount` |
| Classes: name, fully qualified name, parent | `ParsedClass` |

What the projection does not carry today and the import must read straight out
of `entities.raw_json`:

| Raw JSON field | Needed for |
|---|---|
| `TxnTaxDetail`, its `TotalTax` and each `TaxLine` with `TaxRateRef` and `NetAmountTaxable` | The 2200 leg, and the §9 check line. The projection has `is_taxable` per line but no tax amount and no rate. |
| `DepositToAccountRef` on Payment and SalesReceipt | Whether a receipt lands in 1150 or straight in a bank account (W8). |
| `PaymentMethodRef` on Payment, `PaymentType` on Purchase, `CheckPayment` and `CreditCardPayment` on BillPayment | Which credit account a payment uses, and the statement match rules in §10. |
| `LinkedTxn.Amount` on a line, and `Line.LinkedTxn` on Payment, BillPayment, CreditMemo and VendorCredit | **How much** of a payment applied to each invoice. `ParsedLink` carries the edge but not the amount, so AR cannot be relieved per invoice from the projection alone. |
| `AccountBasedExpenseLineDetail.AccountRef` | The expense account of a non item bill or purchase line. `ParsedLine` carries `item_id` but no `account_id`, so an expense line has no account without this. |
| `ItemRef` resolved to the item's `ExpenseAccountRef` and `AssetAccountRef` | The COGS and inventory legs on an inventory line. |
| `JournalEntryLineDetail.PostingType` and `AccountRef` | Imported journal entries (W18). |
| `DepositLineDetail` with its `Entity` and `AccountRef` | Splitting a deposit into the payments it groups plus any direct lines. |
| `DiscountLineDetail` | The 4900 leg (W9). |
| `Customer.Taxable`, `ResaleNum`, `TaxExemptionReasonId` | The exemption certificate flag in §9. |
| `TxnStatus` of `Voided`, and `MetaData` on deleted entities | A voided document posts a reversal, it does not vanish. |

Two of those, `LinkedTxn.Amount` and the line level `AccountRef`, are gaps in
the projection rather than fields nobody needs, and they are worth adding to
`project.rs` in the qbo-local track so the ledger reads one shape rather than
two. That is a change to a Rust file and is out of scope for this document.

### Expected variances against QBO, and their causes

| Variance | Cause | Treatment |
|---|---|---|
| Inventory asset and COGS | QBO values inventory FIFO; this ledger uses moving weighted average (W12) | Expected and explained. It is a permanent difference in timing, not an error, and it nets to zero over the life of an item. |
| Undeposited funds and bank | Timing. QBO's deposit date versus the date the batch actually cleared | Expected within a few days at a period end. Explained per period, must resolve by the next close. |
| Sales tax payable | Tax rounding mode (W17), and QBO's automated sales tax computing rates this ledger does not | Must be under a stated tolerance per period, and the cause is named. |
| Unapplied payments and credits | QBO leaves them in AR; this ledger parks them in 2300 (W8) | Expected. AR plus 2300 must equal QBO's AR to the cent, which is the actual test. |
| Uncategorised income and expense | QBO's bank feed puts unclassified transactions there | Mapped to 6900 or 4990 with `needs_mapping`, and each one with a balance blocks sign off (§2). |
| Opening balance equity | QBO's own imports and beginning balances | Must be zero after the import resolves, per close checklist item 7. |
| Voided documents | QBO zeroes a voided document in place; this ledger posts a reversal | Totals agree; entry counts do not. Expected. |

⚠️ **Dan/CPA to confirm** (W22): which of these variances Joel accepts as
explained rather than requiring them to be driven to zero, and the tolerance on
the sales tax line. If no, and everything must be zero, the inventory method
question (W12) is decided by the import rather than by Dan, and it is decided as
FIFO.

---

## 7. Parallel run and the trial balance diff

ROADMAP §E, made concrete. Both books post every document. Every night, for
each entity, the diff runs as of the previous day.

### Which accounts must match to the cent

| Tier | Accounts | Rule |
|---|---|---|
| **Must match to the cent** | Every bank and credit card account (1100s, 2100), AR total (1200 plus 2300), AP total (2000 plus 2050), sales tax payable (2200), total income (4xxx net of contra), total equity | A variance here is an error. The nightly report exits non-zero, names the offending accounts, and lists the entries on each side that make up the difference. |
| **May differ with a recorded explanation** | Inventory (1300, 1310), COGS (5000), manufacturing variance (5100), parts and labour applied (5300), undeposited funds (1150) at a period end, the split of income between 4100, 4300, 4900 and 4950 | Each needs a standing explanation from the §6 variance table, recorded in `DECISIONS.md` with the policy that produced it (ROADMAP §E). An unexplained variance in this tier is treated as tier one until it is explained. |
| **Expected to differ, not diffed** | 3950 opening balance equity, any account flagged `needs_mapping` | Diffing them produces noise about the import rather than about the books. |

Note the AR test: it is `1200 + 2300` against QBO's AR, not `1200` alone,
because W8 parks unapplied money somewhere QBO does not.

### The nightly report

One file per entity per night, written as CSV and as fixed width text, kept
forever, small enough to read in a terminal:

```
TRIAL BALANCE DIFF   aquamentor   as of 2026-09-10   accrual
generated 2026-09-11 02:14 UTC   ledger build a1b2c3d   replica cursor 2026-09-11 01:58

acct  name                        ledger        qbo          delta   tier  note
1100  Checking                   412,884.19   412,884.19      0.00   must
1200  Accounts receivable      1,204,118.50 1,209,118.50 -5,000.00   must  ** see entries below
2200  Sales tax payable            8,412.06     8,412.04      0.02   must  ** W17 rounding
1300  Inventory, raw materials   318,004.77   321,660.12 -3,655.35   may   weighted average vs FIFO

MUST-MATCH FAILURES: 2        MAY-DIFFER UNEXPLAINED: 0
1200: entries in ledger not in QBO: INV-21234 (5,000.00, created 2026-09-10 16:02, outbox pending)
```

A tier one failure is loud: the run exits non-zero, the failure count lands in
the app chrome next to the sync state, and the count does not clear by itself.
AR and AP aging are diffed the same way, by customer and by vendor, in the same
run. The reconciliation sweep over the replica (`DESIGN.md` §7) keeps running
alongside, because the mirror can drift independently of the ledger.

The cutover bar (ROADMAP §F): one full quarter with zero unexplained variance,
checked at the go/no-go against the four week bar.

### The reconciliation sweep's step 5, defined

`DESIGN.md` §7 step 5 said "diff a QBO Trial Balance against one computed from
the replica" and left it there because the ledger did not exist. It is this:

- **QBO side**: `GET /v3/company/<realm>/reports/TrialBalance` with
  `start_date` the fiscal year start, `end_date` the as-of date,
  `accounting_method=Accrual`. Rows come back keyed by QBO account id.
- **Ledger side**: for the same company and the same as-of date,

```sql
SELECT a.source_ref AS qbo_account_id,
       a.number, a.name,
       SUM(l.debit_minor) - SUM(l.credit_minor) AS balance_minor
FROM   journal_lines  l
JOIN   journal_entries e
       ON e.company_id = l.company_id AND e.entry_id = l.entry_id
JOIN   accounts a
       ON a.company_id = l.company_id AND a.account_id = l.account_id
WHERE  l.company_id = ?1
  AND  e.is_posted  = 1
  AND  e.entry_date <= ?2
GROUP BY a.account_id
HAVING balance_minor <> 0;
```

- **Join** on `accounts.source_ref` to the QBO account id. A QBO account with
  no `source_ref` match is a mapping failure (§2), not a variance. Compare
  `balance_minor` against QBO's amount converted with
  `RoundingPolicy::MirroredAmount`, which cannot round by construction, so any
  QBO amount that is not exactly two decimal places is an error rather than a
  difference.

⚠️ **Dan/CPA to confirm** (W23): the tier assignment above, in particular that
inventory and COGS may differ with an explanation for the whole parallel run
rather than being forced to zero before cutover. If no, W12 is settled as FIFO
and §11 becomes lot aware before 1 November.

---

## 8. Accountant mode

D17: a read-only role in the app, modelled on what QBO gives Joel today, rather
than an export pack. Joel sees it before 1 November and accepts it in writing,
or the cutover moves (ROADMAP §F).

### Scope

| Capability | Detail |
|---|---|
| Period-locked view | The accountant sees closed periods as closed. Everything is readable; nothing is writable, in any period, by any route. The role has no posting permission at all, so the §5 gate is never even reached. |
| Reports | General ledger detail, trial balance, P&L with comparison periods and percent of income, balance sheet, AR aging summary and detail, AP aging summary and detail. Every one takes an as-of or date range, a basis (§0) and an optional class filter. |
| Exports | CSV and PDF of each report, plus a full GL detail CSV for the year. The CSV is the one Joel's software imports; the PDF is the one that goes in the file. |
| Adjusting entry request queue | The accountant proposes an entry: date, lines, accounts, classes, memo, reason. It sits as a request. Dan approves or rejects. On approval it posts as a normal journal entry flagged `is_flagged = 1` and `source_type = 'manual'`, with the request id as its source. If it corrects a posted entry, it posts as a reversal plus a replacement, never as a silent edit. |
| Reclassify tool | Select transactions by date, account, class or contact, propose a new account or class, and the tool produces an adjusting entry request per the above. It does not touch the original entries. |
| Close history log | The full `close_history` table, reopens flagged, readable but not writable. |
| Audit trail | Every posting since the last close: entry, source document and version, the command that produced it, actor, timestamp. Filterable to manual and imported entries only, which is the list a CPA actually reads. |

The queue is the part that matters. In QBO, an accountant user posts adjusting
entries directly into the client's book. Here the accountant proposes and Dan
approves, which makes the CPA relationship explicit in the ledger rather than
implicit in the permissions.

⚠️ **Dan/CPA to confirm** (W25): every accountant adjustment goes through the
request queue with no direct posting, and a correction always takes the form
reversal plus replacement. If no, and Joel needs to post directly, the role
gains a posting permission and the §5 gate becomes the only control, which
means an accountant can post into a period Dan considers closed until Dan
reopens it for them.

### How Joel reaches it

⚠️ **Dan/CPA to confirm** (W24): **recommended, a local install with a
read-only copy of the SQLite file for 2027.**

| Option | What it costs | What it risks |
|---|---|---|
| **Local install, read-only copy** (recommended) | One signed macOS or Windows build, plus a scripted export that produces a dated, read-only snapshot of the ledger and hands it over on request. Days of work, no running cost. | Joel is looking at an as-of copy, not live. Fine for year end work, wrong for a question about last week. Joel needs a machine that will run the build. |
| Hosted read-only view | A server, an auth story, TLS, a security posture for a machine holding both companies' complete books, and a monthly bill forever. Weeks of work. | It is the first thing in this project that is not local first, and it is a new thing to keep running in the months before a cutover. |

Recommendation is the local install, because the year end handoff is a
scheduled event rather than a conversation, and because the cheaper option is
reversible: a hosted view can be built in 2028 over the same read-only role if
the snapshot turns out to be the wrong shape. If no, and Joel needs live
access, the hosted view has to start before 1 November for him to accept it in
writing, which makes it a go/no-go item in its own right.

### What QBO's accountant toolbox has that this deliberately does not

Stated so nobody discovers it in January:

- **No 1099 wizard.** Out of scope entirely (D17). The vendor 1099 flag stays
  as a column and nothing is generated.
- **No books review or "prep for taxes" workflow.** Those are QBO products
  wrapped around QBO's data model, and Joel already does that work in his own
  software.
- **No undo reconciliation.** A reconciliation is undone by reopening the
  period, which is loud by design (§5).
- **No bulk reclassify that posts directly.** The tool exists; it produces
  requests (W25).
- **No accountant-only direct journal entry.** Same reason.
- **No multi-client dashboard, no ProAdvisor list.** Joel has two companies
  here, not a practice.
- **No close the books password.** A password that Dan also knows is not a
  control. The period lock rejects instead.
- **No write-off tool.** Bad debt is one document Dan creates (W11).

---

## 9. Sales Tax Liability Report

The one report Dan files from (ROADMAP §G, D17). It is a four-line quarterly
sheet, as Dan actually files it, plus a fifth line this system can produce and
QBO cannot.

### The report

For one entity and one calendar quarter:

| Line | Name | Derivation |
|---|---|---|
| **A** | Total income | Sum of the P&L income section for the quarter: credits less debits on every 4xxx account, contra accounts 4900 and 4950 included as reductions. Accrual basis (W1, W5). |
| **B** | Sales tax collected | Credits less debits on 2200 sales tax payable for the quarter. Payments of tax to the state debit 2200 and are excluded by document type, so B is what was charged, not what is left owing. |
| **C** | Taxable sales | B divided by the rate. `C = B / rate`. |
| **D** | Non-taxable sales | `D = A - C`. |
| **E** | Check: taxable sales from the lines | Sum of line amounts where `is_taxable` is set, over the same quarter, from the document lines rather than from the tax account. Shown next to C with the variance `E - C` in dollars and as a percentage of C. |

### The filing mapping

Dan's worksheet carries the four lines straight onto the New Jersey ST-50
lines, and the report prints that mapping as a second column so the filing is
a copy, not a translation:

| ST-50 line | Value | From |
|---|---|---|
| Line 1, gross receipts | A | total income |
| Line 2, receipts not subject to tax | D | `A - C` |
| Line 3, receipts subject to tax | C | `B / rate` |
| Lines 4 to 9 | 0.00 | printed as zero; Dan has never had an entry on them (11 Sep 2026) |

The report is frozen per quarter at close (§5 item 5) with the A, B, C, D, E
values and the rate used, so a later reprint shows what was filed rather than
what the ledger says now.

The rate is a configuration value, `sales_tax.nj_rate = 0.06625`, per entity,
effective dated so a historical quarter reprints at the rate in force then. It
is not a tax engine and there is no per-agency breakdown: one agency, New
Jersey, one rate.

Line E is the whole reason to build this from the ledger rather than typing it
off a P&L. C is arithmetic on the tax account and cannot disagree with itself.
E comes from what was actually marked taxable on each line. When they diverge,
something real is wrong: a taxable line invoiced with no tax, a tax adjustment
posted straight to 2200, an exempt customer charged tax, or a rate change that
straddles the quarter. The report shows the variance rather than picking a
winner, and a non-zero variance is what Dan looks at before filing.

⚠️ **Dan/CPA to confirm** (W26): New Jersey is the only agency, 6.625 percent
is the only rate, and the four lines above are the entire filing. If no, and a
second state has nexus, the rate becomes a per-jurisdiction effective-dated
table and B has to be split by agency at the line level, which requires the tax
code id on every line.

⚠️ **Dan/CPA to confirm** (W27): the tolerance on the `E - C` variance below
which Dan files without investigating. Recommended: any variance at all is
shown, and anything over one dollar blocks the period close (§5 item 5). If no,
the close gate moves to a stated figure.

### What the line level data needs

`is_taxable` is already in the projection (`ParsedLine.is_taxable`,
`document_lines.is_taxable`), derived from `TaxCodeRef` being anything other
than the sentinel `NON`. That is enough for line E.

What does not exist yet and is added to the ledger's own document lines:

| Field | Why |
|---|---|
| `tax_code_id` | Which code was applied, so a reprint of a 2024 invoice shows what it showed then, and so a second agency (W26) is a data change rather than a schema change. |
| `tax_rate_applied` | The rate in force on `txn_date`, stored on the line, `Decimal` as text. Effective-dated rates mean a historical document must reprint at its own rate, and looking the rate up at print time gets that wrong the first time a rate changes. |
| `tax_amount_minor` | The tax computed for the line, so B can be proven line by line rather than only in total. |

### The exemption certificate

`customers.is_tax_exempt`, plus `exemption_reason` (resale, government,
nonprofit), `exemption_certificate_ref` and `exemption_expires_on`. Out-of-state
wholesale is the common case here: Northgate Aquatic in Wisconsin and Blue
Harbor Swim in Colorado both buy for resale and both are exempt. The flag drives
the default taxability of a new document's lines, it appears on the customer
page, and an expired certificate is a warning on the invoice screen rather than
a block, because the sale is still legal and it is the paperwork that is stale.
Exempt sales land in D by arithmetic, since they carry no tax and so contribute
nothing to B.

---

## 10. Bank statement import and reconciliation

ROADMAP §G. The files the bank and Chase already generate, parsed and matched.
No aggregator.

```sql
CREATE TABLE bank_statements (
    company_id     TEXT NOT NULL,
    statement_id   TEXT NOT NULL,
    account_id     TEXT NOT NULL,          -- the 1100 or 2100 account
    period_start   TEXT NOT NULL,
    period_end     TEXT NOT NULL,
    begin_minor    INTEGER NOT NULL,
    end_minor      INTEGER NOT NULL,
    state          TEXT NOT NULL CHECK (state IN ('open','closed')),
    closed_at      TEXT,
    PRIMARY KEY (company_id, statement_id)
) STRICT;

CREATE TABLE bank_lines (
    company_id     TEXT NOT NULL,
    statement_id   TEXT NOT NULL,
    line_id        TEXT NOT NULL,
    account_id     TEXT NOT NULL,
    posted_date    TEXT NOT NULL,
    amount_minor   INTEGER NOT NULL,       -- signed, negative is money out
    description    TEXT NOT NULL,
    external_id    TEXT,                   -- the bank's own id, when the file has one
    matched_entry  TEXT,                   -- journal_entries.entry_id
    matched_at     TEXT,
    match_rule     TEXT,                   -- which rule matched, for auditing
    PRIMARY KEY (company_id, statement_id, line_id),
    UNIQUE (company_id, account_id, external_id)
) STRICT;
```

The `UNIQUE` on `external_id` is what makes re-importing the same file
harmless, which it will be, because statements overlap at the edges and
somebody always imports September twice.

### Match rules, in order

1. **External id.** If the file carries an id this ledger has already matched,
   it is the same line. Nothing else is tried.
2. **Exact amount plus a date window.** Amount equal to the cent, posted date
   within a configurable window (default four days either side, because a check
   clears when it clears) against an unmatched payment, deposit, bill payment or
   purchase on the same account. One candidate is a match.
3. **Authorize.net batch settlement.** A deposit line matches a settlement when
   its amount equals the sum of that settlement's payments minus its fees. The
   settlement import (§1) has already created the entry, so this rule matches to
   that entry rather than proposing a new one. This is the rule that pays for
   1160 existing.
4. **Amount plus description token.** A check number in the description against
   a bill payment's check number, or a card's last four against a purchase.
   Proposed to Dan, not applied.
5. **Unmatched.** The line proposes a new Purchase (money out) or Deposit
   (money in), pre-filled with the amount, the date and a guessed account from
   the description. It posts only when Dan confirms it. Nothing posts from a
   statement by itself.

Where rule 2 finds more than one candidate the line is shown with all of them
and Dan picks, because two identical amounts in the same week is exactly the
case where an automatic pick quietly pays the wrong bill.

⚠️ **Dan/CPA to confirm** (W28): the default account for an unmatched money out
line that the description does not resolve, recommended 6900 other operating
expense with a required class and a review flag, rather than a suspense account.
If no, a 1900 suspense account is added and its balance joins the close
checklist.

### The per statement close

Beginning balance plus the signed sum of cleared lines equals ending balance, or
the statement does not close. Closing a statement sets `cleared_at` on every
matched journal line, which is the one field a posted entry may still have
written (§4), and the statement is then immutable. A statement that does not
close cannot be forced, and an unreconciled statement covering a period blocks
that period's close (§5 item 1).

---

## 11. Manufacturing costing

**Built after cutover** (ROADMAP §H). It is designed here because it is the
section of the ledger QBO cannot do and because the posting rules in §1 already
reference it, but it is worthless in a book that is not yet the book of record.
Schema and posting effects only. No UI.

The idea, from the prototype: cost flows from the sheet to the finished product
automatically, so moving a foam price re-prices every product that uses it.

### Landed cost

A sheet costs what it cost plus the freight that brought it. Freight on a mixed
pallet is allocated **by board foot**, not evenly across lines and not by value.
Splitting a mixed pallet evenly makes the cheaper material look dearer and every
margin downstream inherits the error.

```sql
CREATE TABLE materials (
    company_id      TEXT NOT NULL,
    material_id     TEXT NOT NULL,
    sku             TEXT NOT NULL,
    description     TEXT NOT NULL,
    bf_per_unit     TEXT NOT NULL,     -- board feet per sheet, Decimal as text
    avg_cost_per_bf TEXT NOT NULL,     -- moving weighted average, W12
    class_id        TEXT NOT NULL,
    PRIMARY KEY (company_id, material_id)
) STRICT;

CREATE TABLE material_receipts (
    company_id       TEXT NOT NULL,
    receipt_id       TEXT NOT NULL,
    document_id      TEXT NOT NULL,     -- the raw material receipt document
    material_id      TEXT NOT NULL,
    qty              TEXT NOT NULL,
    base_minor       INTEGER NOT NULL,
    freight_minor    INTEGER NOT NULL,  -- allocated share, by board foot
    landed_minor     INTEGER NOT NULL,  -- base + freight, what hits 1300
    PRIMARY KEY (company_id, receipt_id)
) STRICT;
```

Each receipt recomputes `avg_cost_per_bf` for its material as
`(existing_bf * existing_avg + received_bf * received_landed_per_bf) /
(existing_bf + received_bf)`, carried at four decimal places so a 6.5 board foot
part does not inherit the sheet's rounding error. The conversion to `Money`
happens once, at the posting line, through `round_money`.

### Build sheets, and yield as a divisor

```sql
CREATE TABLE build_sheets (
    company_id     TEXT NOT NULL,
    item_id        TEXT NOT NULL,
    version        INTEGER NOT NULL,
    yield_pct      TEXT NOT NULL,      -- nesting yield, Decimal as text
    labor_minutes  TEXT NOT NULL,
    shop_rate_minor INTEGER NOT NULL,
    effective_from TEXT NOT NULL,
    PRIMARY KEY (company_id, item_id, version)
) STRICT;

CREATE TABLE build_sheet_components (
    company_id  TEXT NOT NULL,
    item_id     TEXT NOT NULL,
    version     INTEGER NOT NULL,
    line_no     INTEGER NOT NULL,
    material_id TEXT,                  -- one of material_id or part_name
    part_name   TEXT,
    bf          TEXT,                  -- finished board feet, before yield
    cost_minor  INTEGER,               -- for bought parts
    PRIMARY KEY (company_id, item_id, version, line_no)
) STRICT;
```

**Yield divides, it does not subtract.** To ship 2.1 board feet of finished part
at 78 percent nesting yield you must buy 2.69 board feet, and you paid for all
of it. Material cost per unit is `sum(bf / yield * avg_cost_per_bf)`, and the
waste, `sum((bf / yield - bf) * avg_cost_per_bf)`, is a column on every costing
view rather than a footnote. Build sheets are versioned and effective dated, so
a build in March costs at March's sheet.

### Builds and variance

```sql
CREATE TABLE builds (
    company_id      TEXT NOT NULL,
    build_id        TEXT NOT NULL,
    document_id     TEXT NOT NULL,
    item_id         TEXT NOT NULL,
    sheet_version   INTEGER NOT NULL,
    qty             TEXT NOT NULL,
    bf_expected     TEXT NOT NULL,
    bf_actual       TEXT NOT NULL,
    finished_minor  INTEGER NOT NULL,  -- qty * build sheet unit cost
    raw_used_minor  INTEGER NOT NULL,  -- bf_actual * avg_cost_per_bf
    applied_minor   INTEGER NOT NULL,  -- qty * (parts + labour) at standard
    variance_minor  INTEGER NOT NULL,  -- the plug, W4
    operator        TEXT,
    PRIMARY KEY (company_id, build_id)
) STRICT;
```

Posting effect, per the §1 row: Dr 1310 `finished_minor`, Cr 1300
`raw_used_minor`, Cr 5300 `applied_minor`, and 5100 takes the difference on
whichever side makes the entry balance. A build that consumed more than the
sheet allowed debits 5100, which is a cost. A good nest credits it.

⚠️ **Dan/CPA to confirm** (W29): finished goods are valued at build sheet
**standard** cost, with the difference from actual going to 5100, rather than at
actual cost. If no, `finished_minor` becomes `raw_used_minor + applied_minor`,
5100 disappears, and the build ceases to report whether the recipe is right.

⚠️ **Dan/CPA to confirm** (W30): direct labour and shop overhead are **absorbed
into inventory** through 5300 rather than expensed as incurred. This is the
difference between a kickboard on the shelf carrying its cutting time and
carrying only its foam. If no, 5300 and 5400 disappear, builds post only
material, and inventory on the balance sheet is materially lower and moves
differently from QBO's, which changes the §7 tolerance on 1310.

### Sensitivity

A pure function over the same data, no schema: what a foam price move does to
unit cost and margin, and what a yield improvement is worth. On a rescue tube,
five points of yield beats a five percent foam discount, and the report exists
to make that arithmetic something Dan reads rather than something he derives.

---

## 12. What is deliberately not here

| Not built | Why, and what happens instead |
|---|---|
| 1099-NEC | Out of scope entirely (D17). The vendor flag stays as a column, `vendors.is_1099`, and nothing is generated. FY2026 1099s come from QBO, which holds the 2026 books (ROADMAP §F). |
| Multi-currency | Both entities are USD. `Money` carries no currency tag (`DESIGN.md` §1) and adding one touches every signature to buy nothing. |
| Payroll | SurePayroll stays. One journal per pay run from its summary: Dr 5400 and 6xxx for gross by class, Cr 2400 for the liabilities, Cr 1100 for the net paid. Nothing computes a paycheck here. |
| Inventory quantities | Quantities are not tracked until manufacturing costing lands (§11, after cutover). Until then 1300 and 1310 are value accounts fed by documents, and physical counts come in as inventory adjustments. QBO's inventory tracking is not used for foam today either. |
| Budgets | No budget table, no budget versus actual report. |
| Time tracking, e-invoicing, ACH origination, a web or mobile client, any hosted service | Kickoff §12. No abstraction layers are added for any of them. |

---

## 13. Open items

Every ⚠️ in this document, who decides, and what it blocks. "Joel" means the
CPA, "Dan" means the business decision is Dan's alone, "Engineering" means it is
a fact to verify rather than a policy to choose.

| # | § | Item | Decides | Blocks |
|---|---|---|---|---|
| W1 | 0 | Books are accrual, cash basis is a report time transformation, materially equal rather than to the cent | Joel | The report layer's cash path, and the parallel run bar |
| W2 | 1 | Raw material bills capitalise to inventory rather than expensing | **Decided, Dan, 12 Sep 2026: yes** | Ledger engine, ROADMAP §B' start |
| W3 | 1 | Vendor credit reduces material cost rather than other income | **Decided, Dan, 12 Sep 2026: yes** | Ledger engine, §11 landed cost |
| W4 | 1 | Build variance plugs to 5100 rather than being spread over unit cost | **Decided, Dan, 12 Sep 2026: yes** | Ledger engine, §11 |
| W5 | 1 | Sales tax is a liability at the invoice date, not the payment date | Joel | §9, and the 2200 posting rule |
| W6 | 1 | Shipping charged to customers is income, not contra freight | **Decided, Dan, 12 Sep 2026: yes** | §9 line A, chart of accounts |
| W7 | 1 | Freight is taxable in NJ when the goods are taxable | Joel | §9 line E |
| W8 | 1 | Payments land in undeposited funds by default | **Decided, Dan, 12 Sep 2026: yes** | §10 matching, §7 AR tier |
| W9 | 1 | Discounts are contra income (4900) | Joel | §9 line A |
| W10 | 1 | Credit memos and refunds debit returns and allowances (4950) | Joel | Chart of accounts, P&L shape |
| W11 | 1 | Bad debt written off direct to expense, no allowance | Joel | §5 close checklist |
| W12 | 1 | Inventory at moving weighted average rather than FIFO | **Decided, Dan, 12 Sep 2026: yes** | §11, §6 import variances, §7 tiers |
| W13 | 1 | Merchant fees gross as expense rather than netted | Dan | Authorize.net settlement import |
| W14 | 2 | Account numbering scheme, and whether QBO names are preserved | Joel | Import mapping table |
| W15 | 3 | The six class list, and cnc/uv appearing in both books | **Decided, Dan, 12 Sep 2026: yes** | Import, and it cannot be backfilled |
| W16 | 3 | Unclassed import lines take the item default rather than being quarantined | **Decided, Dan, 12 Sep 2026: yes** | Import volume and the quarantine queue |
| W17 | 4 | QBO's tax rounding mode, banker's versus half up (`DESIGN.md` §12 item 3) | Engineering to verify, then Joel if it diverges | §7 noise floor on 2200 and AR |
| W18 | 4 | Imported QBO journal entries accepted as written and flagged | Joel | Import completion |
| W19 | 5 | A signed off fiscal year gets a second lock a normal reopen cannot lift | Dan | Close screen |
| W20 | 5 | All seven close checklist items are hard gates | Dan | Close screen |
| W21 | 6 | The boundary year, and one opening balance entry for everything before it | Joel | Import scope, §7 start date |
| W22 | 6 | Which import variances are accepted as explained rather than driven to zero | Joel | Import sign off, cutover bar |
| W23 | 7 | The must-match-to-the-cent tier list | Joel | Nightly diff, go/no-go |
| W24 | 8 | Accountant mode delivery: local read-only install (recommended) versus hosted | Joel | Joel's written acceptance, go/no-go |
| W25 | 8 | Adjustments always through the request queue, corrections as reversal plus replacement | Joel | Accountant mode build |
| W26 | 9 | NJ only, one rate at 6.625 percent, four lines are the whole filing | Dan | §9, line level tax fields |
| W27 | 9 | Tolerance on the E minus C variance before filing | Dan | §5 close checklist item 5 |
| W28 | 10 | Default account for an unresolved unmatched money out line | Dan | Statement import screen |
| W29 | 11 | Finished goods at build sheet standard cost, difference to 5100 | Joel | §11, after cutover |
| W30 | 11 | Labour and overhead absorbed into inventory rather than expensed | Joel | §11, §7 tolerance on 1310 |

Thirty items. Twenty-six of them can be answered in one sitting with the
posting-rules table in front of Dan and Joel; W21, W22 and W23 need the import
to have been run once; W17 needs thirty minutes against Intuit's documentation.
Nothing in ROADMAP §B' starts before W2, W3, W4, W12, W15 and W16 are answered,
because those six are the ones that cannot be changed after history is imported.
