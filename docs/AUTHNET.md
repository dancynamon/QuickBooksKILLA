# Authorize.net settlement import

`LEDGER-DESIGN.md` §1 (the Settlement row and the Deposit row), §10
(settlement matching); `DECISIONS.md` D15 ("customer payments move to
Authorize.net at cutover... a hosted payment link on the own invoice,
settlement import posting the customer payment"). This is the how-to; the
policy is in the design document, this file is the operator's guide.

The code is `apps/ledger/src/authnet.rs` and its `csv` submodule, plus the
fee-aware extension to `apps/ledger/src/bank.rs`'s settlement match. The
import never calls Authorize.net's API — every batch arrives as a file
exported by hand from the Merchant Interface, the same "no aggregator"
discipline as `docs/BANK.md`. The one API call is the payment link (the
`link` submodule, last section below): it asks for a hosted payment page and
moves no money itself.

## Exporting the CSV

Merchant Interface -> **Reports** -> **Transaction Detail** (or **Settled
Batch List** -> a batch's own transaction list) -> **Download to File** ->
comma-separated. Any date range that overlaps a batch already imported is
harmless — see re-importing, below.

## The columns this parser reads

Header matching is case-insensitive and tolerant of the handful of names
Authorize.net has shipped for the same column; an extra column the file
carries that this list does not name is ignored outright.

| Column | Aliases accepted | Required | Used for |
|---|---|---|---|
| Transaction ID | Trans ID, Transaction Id | yes | the per-transaction dedupe key |
| Transaction Status | Status | yes | which rows carry money (below) |
| Settlement Date/Time | Settlement Date, Batch Settlement Date/Time | yes | the batch's (and every document's) `txn_date` |
| Batch ID | Settlement Batch ID, Batch Id | yes | groups rows into one `SettlementBatch` |
| Settlement Amount | — | one of these two | the transaction amount |
| Total Amount | Amount | required | |
| Submit Date/Time | Submit Date, Submit Time | no | kept on the parsed row, not used by grouping |
| Invoice Number | Invoice No, Invoice #, Invoice | no | resolves a sale's `Payment` to an invoice |
| Customer ID | Customer Id, Cust ID | no | the `Payment`/`RefundReceipt`'s customer entity |
| Card Type | Payment Method, Method | no | kept on the parsed row, not posted anywhere |
| Transaction Type | Trans Type | no | kept on the parsed row (`auth_capture`, `credit`, `void`, ...), not posted anywhere |

A column this list marks required that the file does not have under any
alias is `ParseError::MissingColumn("...")` — the whole file is rejected
rather than guessing which column was meant. An amount carrying more than two
decimal places is `ParseError::Precision` (D9): quarantined, never rounded
silently. An unrecognised `Transaction Status` value is `ParseError::Malformed`
rather than being ignored — an export whose vocabulary has changed should
fail loudly, not silently drop rows.

Only two statuses carry money into a batch: **Settled Successfully** (a sale)
and **Refund Settled Successfully** (a refund). **Voided** and **Declined**
rows still parse — a malformed voided row still fails the import — and are
then dropped when batches are grouped, since nothing settled.

## The batch and fee model

One `Batch ID` becomes one [`SettlementBatch`]: `gross` is the sum of settled
sales, `refunds` the sum of settled refunds, `net_before_fees = gross -
refunds` — the one figure this export actually states about what moved
through Authorize.net's clearing.

**This export carries no per-batch fee, ever.** Two different fees exist and
neither is in this file:

1. **Authorize.net's own gateway fee** is billed monthly, not per batch or
   per transaction. It is booked from that monthly statement as an ordinary
   expense bill (Dr 6700, Cr 2000, `LEDGER-DESIGN.md`'s Bill row) — **not**
   through this import. If a monthly Authorize.net statement lands in the
   inbox, it goes through `ledger`'s ordinary bill entry, same as any other
   vendor bill.
2. **The processor's per-transaction discount fee** — the difference between
   `net_before_fees` and what the bank actually receives — is real money but
   is not stated anywhere in the Merchant Interface's transaction export
   either. It only becomes knowable when the real bank statement shows a
   smaller deposit than the batch's net.

So `settlement_documents` always posts the batch's `Settlement` document
assuming that fee is zero: `Dr` the bank account, `Dr` 6700 only if something
external already supplied a fee (see below), `Cr` 1160 clearing, all at
`net_before_fees`. Paired with the `Payment` (Dr 1160) each settled sale posts
and the `RefundReceipt` (Cr 1160) each settled refund posts, **1160 always
nets to zero the moment a batch's documents are posted**, regardless of
whether the true discount fee is known yet — that is what `chart.rs` means by
1160 "nets to zero after each batch," and it is why the fee not being known
at import time does not leave a hole in the books.

What *is* provisionally wrong, until the fee is known, is the `Settlement`
entry's bank leg: it assumes the whole `net_before_fees` reached the bank.
`ledger bank match` (§10, `docs/BANK.md`) is what corrects it — see the next
section.

## Importing

```
ledger authnet import --db LEDGER.sqlite --company aquamentor \
    --file settlement-september.csv
```

For each batch this posts one `Settlement` document
(`document_id = "authnet:<batch_id>"`), then for each settled sale a
`Payment` (`document_id = "authnet:txn:<txn_id>"`) — applied to the invoice
whose `number` matches the row's `Invoice Number`, when one is found, else
parked `unapplied` in 2300 with a `warning:` line printed for Dan to chase —
and for each settled refund a `RefundReceipt` under `--refund-class` (default
`drop`, since the export carries no class or item information to derive one
from).

**A sale never applies more than the invoice still has open.** The
application is capped at the invoice's open balance (total less every
payment and credit applied to it, whatever its date), less what earlier rows
in the same file already applied to it. The rest of the sale is parked in
2300 with a `warning:` line naming the split, e.g. `106.62 applied, 13.38
over its open balance parked unapplied`. An invoice already paid in full (two
links both paid, or a check plus a link) or voided takes the whole sale
unapplied (`nothing left open on it`). Dan then refunds the customer or
applies the credit to their next invoice.

**Re-importing the same file posts nothing new.** Every document's id is
deterministic from the batch/transaction id, and a settled Authorize.net
transaction never changes once it appears in this export — so a document
whose id has already been saved is skipped outright rather than replaced
(unlike a QBO import, which re-posts a document whose payload changed).
Running the same file twice reports `skipped (already imported): N` and
`batches settled: 0`. Already-imported sales are left out before the
open-balance cap is worked out, since the balance already reflects them.

```
--clearing NUM        Authorize.net clearing account, default 1160.
--fees NUM             Merchant fees account, default 6700.
--undeposited NUM      Undeposited funds account, default 1150 — kept for
                       symmetry with the rest of the ledger's posting
                       config; a settled sale's Payment always debits
                       --clearing, never this, so that 1160 nets to zero
                       (see above).
--refund-class CLASS   Class an Authorize.net refund's 4950 leg carries,
                       default "drop".
--fee-class CLASS      Class the Settlement's own 6700 leg would carry if
                       a fee were already known at import time, default
                       "drop". Inert on an ordinary import, since the fee
                       never is known that early — see "The batch and fee
                       model" above.
```

## Confirming the real fee: `bank match`

Once the real bank statement for the payout arrives, import and match it
exactly as any other statement (`docs/BANK.md`):

```
ledger bank import --db LEDGER.sqlite --company aquamentor \
    --account 1100 --file chase-september.csv --format csv --profile chase \
    --opening ... --closing ... --from ... --to ...
ledger bank match --db LEDGER.sqlite --company aquamentor --statement STMT_ID
```

§10's settlement rule now has two cases:

- **Exact** (unchanged): the deposit line's amount equals a settlement
  entry's 1160 credit sum exactly — the discount fee genuinely was zero.
  Marked `settlement`.
- **Fee-aware** (new): the deposit line comes in *under* a settlement entry's
  1160 credit sum by no more than `MatchRules::max_fee_ratio` of that sum
  (default 4%) — a plausible processing fee, not a coincidence. The
  settlement entry's own bank leg is cleared exactly as an exact match would
  be, and the shortfall posts as its own correcting entry: **Dr 6700, Cr the
  bank account** (not Cr 1160 — 1160 was already right; what was wrong was
  the settlement entry's guess about the bank leg). Also marked `settlement`
  in the match report, since it resolves the same way an exact match does;
  `MatchReport::settlement_fee` counts these separately for anyone who wants
  to see how often the fee correction actually fires.

After this, 1160 is exactly zero, the bank account matches the real
statement, and 6700 carries the discovered fee under `MatchRules::fee_class`
(default `drop`).

A line further out than `max_fee_ratio` — a genuine mismatch, not a
processing fee — falls through to the ordinary keyword proposal, same as any
other unmatched line, rather than being silently absorbed.

## Payment link

D30. The customer side of the loop: a hosted Authorize.net payment page for
one of the ledger's own invoices, so the customer pays by card on
Authorize.net's page and the settled payment comes back through the import
above against that same invoice. Code: `apps/ledger/src/authnet/link.rs`.

**How the payment finds its invoice.** The request sends the invoice's own
`number` as `order.invoiceNumber` and its open balance as `amount`. The
Merchant Interface export carries that number back in its `Invoice Number`
column, and `authnet import` applies the sale to the invoice with that number
(above). Nothing else links the two, so an invoice needs a number of at most
20 characters (Authorize.net's limit) — the ledger's own sequence (`1001`,
`1002`, ...) always is.

**What is refused**, before Authorize.net is asked anything: a document that
is not an invoice, a voided invoice, a fully paid one, and one with no
number.

**The token is short-lived.** Authorize.net's form token is valid for 15
minutes and has to be POSTed to the hosted form; it is not a URL that can sit
on a PDF for a week. So a link is the form URL, the token, its expiry, and a
small page that POSTs the token when opened. Mint it when the customer is
about to pay (on the phone, or in the email you are sending right now). A
durable "Pay now" link printed on the invoice needs an endpoint that mints a
fresh token at click time — not built yet (D30).

```
ledger authnet link --db LEDGER.sqlite --company aquamentor \
    --document DOCUMENT_ID --out pay-1042.html
```

prints the invoice number, amount, environment, form URL and expiry, and
writes the page to `--out` (without `--out` it prints the token instead).
From an MCP client it is the `payment_link` tool (`docs/MCP.md`).

### Sandbox first, production off by default

`[authnet]` in `.local/config.toml` (gitignored; `--config PATH` or
`LEDGER_AUTHNET_CONFIG` to point elsewhere). Every key is optional; no file
or no table means sandbox.

```toml
[authnet]
environment = "sandbox"          # or "production"
allow_production = false         # production needs this true as well
merchant_name = "Aquamentor"     # shown on the hosted receipt
# return_url = "https://aquamentor.com/thanks"   # receipt's Continue button
# cancel_url = "https://aquamentor.com"
```

| Environment | API endpoint | Hosted form |
|---|---|---|
| sandbox | `https://apitest.authorize.net/xml/v1/request.api` | `https://test.authorize.net/payment/payment` |
| production | `https://api.authorize.net/xml/v1/request.api` | `https://accept.authorize.net/payment/payment` |

### Credentials

The API Login ID and Transaction Key are never read from a file — a
`transaction_key` or `api_login_id` under `[authnet]` is an error. In order:

1. `AUTHNET_API_LOGIN_ID` and `AUTHNET_TRANSACTION_KEY` in the environment.
2. The macOS login keychain: service `com.aquamentor.ledger.authnet`, account
   `sandbox` or `production`. Store them with

   ```
   ledger authnet credentials --environment sandbox
   ```

   which reads the Login ID then the Key from stdin, one per line, so neither
   lands in shell history.

Neither value is ever printed or put in an error message.

### Trying it in the sandbox

1. Sign up for a free sandbox account at
   <https://developer.authorize.net/hello_world/sandbox.html>; in its
   Merchant Interface (`https://sandbox.authorize.net`), Account -> Settings
   -> API Credentials & Keys gives the API Login ID and a new Transaction Key.
2. `ledger authnet credentials --environment sandbox`.
3. Create an invoice (`create_document`), then `ledger authnet link ... --out
   pay.html`, open `pay.html` within 15 minutes and pay with Authorize.net's
   test card `4111111111111111`, any future expiry.
4. After the sandbox batch settles, export the Transaction Detail CSV and run
   `ledger authnet import` against a scratch book: the invoice should leave
   `aging`.

Production is the same with the production account's keys,
`environment = "production"` and `allow_production = true` — at cutover, not
before.
