# The `qbo-local` MCP server

`ROADMAP.md` §G, "Channel intake": Dan's Shopify, Amazon and wholesale skills
in `claude-config` write to QBO today through the QuickBooks MCP server.
Every skill that only *reads* QBO — and most of them do — can be repointed at
this server instead, talking to the local replica rather than Intuit's API.
Nothing here writes. Document-write tools (create an invoice, apply a
payment, and so on) arrive in Phase D; until then this is the query API from
`apps/qbo-local/src/store/query.rs`, `store/search.rs` and `store/lineage.rs`
over MCP, nothing more.

The server is `apps/qbo-local/src/mcp.rs`, run by the `qbo-local-mcp` binary.
It speaks MCP protocol revision `2025-06-18` over stdio — one JSON-RPC 2.0
request per line on stdin, one response per line on stdout, nothing else
ever written to stdout.

## Registering it

The server opens one SQLite file, named by `QBO_LOCAL_DB`, strictly
read-only (`Store::open_read_only` — `SQLITE_OPEN_READ_ONLY`, enforced by
SQLite itself). That file has to already exist and be current: it's the
replica the sync daemon (`qbo-local`, `DESIGN.md` §4) keeps up to date, not
something this server creates or migrates.

**Claude Desktop** (`claude_desktop_config.json`):

```json
{
  "mcpServers": {
    "qbo-local": {
      "command": "/path/to/qbo-local-mcp",
      "env": {
        "QBO_LOCAL_DB": "/path/to/replica.db"
      }
    }
  }
}
```

**Claude Code** (`.claude/settings.json`, or via `claude mcp add`):

```json
{
  "mcpServers": {
    "qbo-local": {
      "command": "/path/to/qbo-local-mcp",
      "env": {
        "QBO_LOCAL_DB": "/path/to/replica.db"
      }
    }
  }
}
```

Build the binary with `cargo build --release -p qbo-local --bin qbo-local-mcp`
and point `command` at
`target/release/qbo-local-mcp`.

Every tool call takes `realm_id` — the QBO company id (a numeric string).
Aquamentor and WaterLine CNC are separate realms sharing nothing, so a skill
that talks to both companies passes a different `realm_id` per call; nothing
in this server infers which realm you mean.

## Tools

### `search`

Search by document number, the customer's PO number, an item SKU, a dollar
amount, or free text across contacts, items, memos and line descriptions.
Exact identifiers are tried before full text (`DESIGN.md` §3.3), so typing a
known document number lands on it rather than a ranked guess.

```json
{
  "name": "search",
  "arguments": { "realm_id": "1234567890123456", "query": "1088" }
}
```

### `document_detail`

One document — invoice, bill, estimate, sales receipt, purchase order, and
so on — with its lines and its lineage (the estimate it came from, the bills
a bill payment settles).

```json
{
  "name": "document_detail",
  "arguments": { "realm_id": "1234567890123456", "qbo_id": "418" }
}
```

### `list_documents`

Documents of one type in an optional date range, newest first, paged.

```json
{
  "name": "list_documents",
  "arguments": {
    "realm_id": "1234567890123456",
    "doc_type": "Invoice",
    "from": "2026-01-01",
    "to": "2026-12-31"
  }
}
```

### `open_documents`

Documents of one type still open — carrying a balance, or (for purchase
orders and estimates) holding an open QBO status even at a zero balance.
The AR/AP work queue.

```json
{
  "name": "open_documents",
  "arguments": { "realm_id": "1234567890123456", "doc_type": "Bill" }
}
```

### `contact_detail`

One customer or vendor: contact info and balance, every document still
carrying a balance against them, and their 50 most recent documents.

```json
{
  "name": "contact_detail",
  "arguments": {
    "realm_id": "1234567890123456",
    "contact_type": "customer",
    "qbo_id": "31"
  }
}
```

### `item_detail`

One item: price, cost, quantity on hand, every document that has used it,
and total units sold across invoices and sales receipts.

```json
{
  "name": "item_detail",
  "arguments": { "realm_id": "1234567890123456", "qbo_id": "12" }
}
```

### `ar_aging` / `ap_aging`

Accounts-receivable (open invoices) or accounts-payable (open bills) aging
as of a date, bucketed into current / 1-30 / 31-60 / 61-90 / over-90 days
past due, by customer or vendor, with realm-wide totals.

```json
{
  "name": "ar_aging",
  "arguments": { "realm_id": "1234567890123456", "as_of": "2026-09-11" }
}
```

### `sync_status`

Whether writes are enabled for this realm, and per entity type how much is
mirrored, the last CDC cursor and full-sweep timestamps, and how many
records are quarantined for failing to parse.

```json
{
  "name": "sync_status",
  "arguments": { "realm_id": "1234567890123456" }
}
```

### `class_tree` / `chart_of_accounts`

Every QBO class (with parent, for the hierarchy) or every account (with
number, type, subtype, classification and balance).

```json
{
  "name": "chart_of_accounts",
  "arguments": { "realm_id": "1234567890123456" }
}
```

## Output shape

Every tool result comes back as MCP `content` (one `text` block holding
pretty-printed JSON — for a skill reading the response as text) plus the
same value under `structuredContent`. A `Money` field (a total, a balance, an
aging bucket) is rendered as a decimal string with two places — `"1234.56"`,
a credit as `"-12.00"` — not the bare integer minor units the database and
the outbox use internally. A failure the caller should see — a bad
`realm_id`, a `qbo_id` that doesn't exist, an unparseable date — comes back
as a normal tool result with `isError: true` and the reason in `content`,
never as a JSON-RPC error; a JSON-RPC error means the *transport* failed
(malformed JSON, an unknown method), not the query.

## What's next

Phase D's document-write tools did not land on this server. `ROADMAP.md`
§D's own shape — a save commits to the own document store and posts to the
own ledger, and only then enqueues the outbox record that mirrors to QBO —
means the write path is a *ledger* write, not a QBO write, so it lives on its
own server, over its own book. See "The `ledger` MCP server" below. Until a
skill is repointed at it, any skill that needs to write keeps talking to the
QuickBooks MCP; only reads have moved off it.

Both servers share their JSON-RPC 2.0 / MCP stdio transport — line framing,
error codes, `initialize`/`ping`/`tools/list`/`tools/call` — from one crate,
`crates/mcp-stdio`, rather than each hand-rolling its own copy. Nothing in
this document changes as a result; it is mentioned here because a skill
reading raw protocol behaviour off either server is reading the same code
either way.

---

# The `ledger` MCP server

`ROADMAP.md` §D ("write UI, own store first, QBO second") and §G ("Channel
intake"): once a document's save commits to the own ledger before anything
mirrors to QBO, the skills that create documents need somewhere to send
that save. This server is that somewhere — every report `apps/ledger/src`
already computes, as a read tool, plus the gated writes that save and post a
document, correct one, run the accountant's adjustment queue, confirm a bank
line, and move the period gate.

The server is `apps/ledger/src/mcp.rs`, run by the `ledger-mcp` binary. Like
`qbo-local-mcp` it speaks MCP protocol revision `2025-06-18` over stdio — one
JSON-RPC 2.0 request per line on stdin, one response per line on stdout,
nothing else ever written to stdout — but on one important point it differs:
**this server writes.** The ledger is opened read-write (`Ledger::open`), not
through any read-only flag, because `save_and_post_document` and the rest of
the write tools below have nowhere else to post to.

Unlike `qbo-local-mcp`, which serves every realm from one process and takes
`realm_id` on each call, `ledger-mcp` is bound to one company for its whole
process — there is no per-call company argument.

## Registering it

Two environment variables, both required:

- `LEDGER_DB` — the ledger's own SQLite file (not the `qbo-local` replica).
  It must already exist with the company below initialised in it (`ledger
  init`, `LEDGER-DESIGN.md`) — this server does not create or migrate it.
- `LEDGER_COMPANY` — `"aquamentor"` or `"waterline"`. Every tool call for the
  life of the process is scoped to this one company; Aquamentor and
  WaterLine CNC share nothing, so a skill that talks to both runs two
  `ledger-mcp` processes, one per company, exactly as it would run against
  two different `realm_id`s on the `qbo-local` server.

```json
{
  "mcpServers": {
    "ledger-aquamentor": {
      "command": "/path/to/ledger-mcp",
      "env": {
        "LEDGER_DB": "/path/to/ledger.db",
        "LEDGER_COMPANY": "aquamentor"
      }
    }
  }
}
```

Build the binary with `cargo build --release -p ledger --bin ledger-mcp` and
point `command` at `target/release/ledger-mcp`.

One optional variable, for `payment_link` only: `LEDGER_AUTHNET_CONFIG`
names the config file whose `[authnet]` table it reads (default
`.local/config.toml`, relative to the server's working directory). Missing,
the tool runs against the Authorize.net **sandbox**. The API Login ID and
Transaction Key come from `AUTHNET_API_LOGIN_ID` / `AUTHNET_TRANSACTION_KEY`
in the server's `env`, else the macOS keychain (`docs/AUTHNET.md`, "Payment
link") — never from the config file.

Two more, for `render_document` / `render_statement` only (D39):
`LEDGER_DOCUMENTS_CONFIG` names the file whose `[documents]` table they read
(default `.local/config.toml` — the same file), and `LEDGER_DOCUMENTS_DIR`
overrides where the PDFs go. See "Document tools" below.

## The write gate

Two rules apply to every write tool below, uniformly, so a skill never has
to special-case one of them:

1. **Refused during a replay.** `crate::pipeline`'s importer walks the
   `qbo-local` replica with `Ledger::set_replaying(true)` for the duration,
   so a live write can never interleave with a replay mid-walk. Every write
   tool checks `Ledger::is_replaying()` first and refuses immediately —
   before touching the document store or the period gate — with `isError`
   and no other side effect, rather than racing the importer.
2. **`locked_through` is always in the result.** The period gate
   (`LEDGER-DESIGN.md` §5) can reject a write for a reason a caller only
   otherwise discovers from the error text, so every write tool's result —
   success or refusal — carries the company's current `locked_through`
   alongside it. A caller always sees the gate it is or is not up against
   without a second call.

A domain failure — a closed period, a missing class, an unknown item, an
unbalanced journal, and the rest — comes back as an ordinary `tools/call`
result with `isError: true` and the reason in `content`, never as a JSON-RPC
error, exactly as `qbo_local::mcp` documents: a JSON-RPC error means the
*transport* failed, not the command.

## Money on the wire

Tool **output** always renders `Money` as a two-place decimal string —
`"1234.56"`, a credit as `"-12.00"` — the same convention `qbo_local::mcp`
uses, for the same reason: an LLM reading the text content wants a number it
can read, not minor-unit cents.

Tool **input** is more forgiving. A `Money`-typed field — a line `amount`, a
`unit_cost`, `tax.total_tax`, a `debit`/`credit` — may be given either as
that same decimal string (`"1234.56"`) or as a bare minor-unit integer
(`123456`), matching `Money`'s own wire format. The server rewrites every
recognised money field from the decimal-string form to the integer form
before any typed deserialiser sees it, so a caller can write whichever is
more natural for a given field.

## Read tools

### `trial_balance`

Every account's debit and credit total and signed balance as of a date, plus
which accounts sit on the wrong side of their normal balance.

```json
{ "name": "trial_balance", "arguments": { "as_of": "2026-09-30" } }
```

### `profit_and_loss`

Income, COGS and expense between two dates inclusive, by account and by
class, with gross margin and net income.

```json
{
  "name": "profit_and_loss",
  "arguments": { "from": "2026-01-01", "to": "2026-09-30" }
}
```

### `balance_sheet`

Assets, liabilities and equity as of a date, current year net income folded
into equity.

```json
{ "name": "balance_sheet", "arguments": { "as_of": "2026-09-30" } }
```

### `sales_tax_lines`

The quarterly filing lines (`LEDGER-DESIGN.md` §9) — A/B/C/D, the
line-level-taxable cross-check E, the variance, and the ST-50 lines.

```json
{
  "name": "sales_tax_lines",
  "arguments": { "year": 2026, "quarter": 3 }
}
```

### `general_ledger`

Every posted line between two dates, grouped by account (or one account),
with a running balance.

```json
{
  "name": "general_ledger",
  "arguments": { "from": "2026-09-01", "to": "2026-09-30", "account": "1200" }
}
```

### `audit_trail`

Every posting since a date (or since the last close), flagged entries first.

```json
{ "name": "audit_trail", "arguments": { "since": "2026-09-01" } }
```

### `entries_for_document`

Every journal entry ever posted from one document.

```json
{
  "name": "entries_for_document",
  "arguments": { "document_id": "inv-1088" }
}
```

### `list_adjustments`

The adjusting-entry request queue, optionally filtered to one state
(`"proposed"`, `"approved"`, `"rejected"`, `"posted"`).

```json
{ "name": "list_adjustments", "arguments": { "state": "proposed" } }
```

### `bank_status`

One statement's header plus every line on it and a match summary.

```json
{ "name": "bank_status", "arguments": { "statement_id": "stmt-2026-08" } }
```

### `locked_through`

The date the period gate is locked through, or `null`.

```json
{ "name": "locked_through", "arguments": {} }
```

### `list_contacts` / `list_items`

Customers and vendors (`kind` `"customer"` or `"vendor"`), and products and
services, from the book's own master data (D28). `query` matches anywhere in
the name, company name, email, SKU or description, case-insensitive;
`include_inactive` adds the inactive ones.

```json
{ "name": "list_contacts", "arguments": { "kind": "customer", "query": "pool" } }
```

### `open_documents` / `aging`

What is still open on the receivable book (invoices, credit memos as
negatives) or the payable book (bills, vendor credits) as of a date, derived
from the saved documents and the applications against them — nothing is
stored. `aging` buckets it per contact into current, 1-30, 31-60, 61-90 and
over 90 days past due; its total ties to 1200 (receivable) or 2000 (payable)
on the trial balance. The slim cutover import (D28) checks itself against
QBO's aging with these.

```json
{ "name": "aging", "arguments": { "book": "receivable", "as_of": "2026-10-15" } }
```

### `get_document`

The newest saved version of one document, as an editor needs it back: every
field a person typed or the book filled (`kind`, `number`, `txn_date`,
`due_date`, `contact`, `header_class`, `lines`, `tax`, `deposit_to`,
`pay_from`, `applications`, `unapplied`, `is_voided`, `memo`, `source_ref`),
money as two-place strings, plus `version` (how many versions are saved) and
`total`. The desktop app opens a document from Receivables/Payables into the
entry form with this, then sends it back through `update_document`.

```json
{ "name": "get_document", "arguments": { "document_id": "019a…" } }
```

## Write tools

### `save_and_post_document`

Saves a document and, unless it is an Estimate or PurchaseOrder, posts the
entry `crate::post`'s rules table derives from it. See "Repointing a skill"
below for a full example. Because an MCP call has no replica to look items
up in, an inline `items` map (item id to `ItemAccounts` JSON) supplies
whatever an `Item` line needs. Returns `{ document_id, version, entry_id,
entry, locked_through }` — `entry_id` and `entry` are `null` for a
non-posting document.

### `upsert_contact` / `upsert_item`

Create or replace a customer, vendor or item. A contact's `display_name` is
unique per kind; an item's accounts and class must exist, and an inventory
item (and only one) carries an `asset_account`. `source_ref` keeps the QBO id
when the slim import loads them.

### `create_document`

The standalone entry path (D28), and the one a repointed skill should use.
It takes what a person would type and fills the rest from master data: a new
`document_id`, the next `number` for invoices, estimates, sales receipts,
credit memos, refund receipts and POs (a number already in use is refused),
`due_date` from the contact's terms, `header_class` from the contact's
default, and sales tax at the NJ rate on taxable lines (zero for an exempt
customer). The contact must exist, be active and be the right kind; every
item must exist. A document the posting rules refuse leaves nothing saved.

```json
{
  "name": "create_document",
  "arguments": {
    "actor": "process-sales-order",
    "document": {
      "kind": "Invoice",
      "txn_date": "2026-09-13",
      "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
      "lines": [
        { "kind": "Item", "item_id": "rt-50-red", "qty": "10", "amount": "199.50",
          "is_taxable": true }
      ]
    }
  }
}
```

Returns `{ document_id, version, number, due_date, total, tax, entry_id,
entry, locked_through }`.

### `update_document`

Edits a saved document. A correction is a reversal, never an overwrite
(`LEDGER-DESIGN.md` §4/§6): `document` is what `create_document` takes, with
the saved `document_id`; it is checked and filled the same way and saved as
the next version in `document_versions` (every earlier version stays). When
what it posts changed, the old entry is reversed — dated as the entry it
reverses, so nothing is left at the old figure — and the new entry posted;
when it did not (a memo or due-date edit), the entry stands untouched. The
reversal, the version and the new entry commit as one unit
(`Ledger::atomically`, a SQLite savepoint): a refusal part way leaves the
book exactly as it was.

The number is kept: leave it out, or send the one it has. Leave
`applications` out to keep the saved ones — editing a credit memo's lines
does not unapply it — or send `[]` to clear them. Refused, with nothing
changed:

- a voided document (it stays voided), or an edit that sets `is_voided`
  (use `void_document`);
- a change of `kind`, or of contact type (customer to vendor or back) —
  another customer is fine;
- renumbering a document the book numbers (a bill's vendor number may
  change);
- a total below what payments and credits already apply against it, or an
  application beyond what is left on its target;
- the old date or the new date on or before `locked_through`.

```json
{
  "name": "update_document",
  "arguments": {
    "actor": "dan",
    "document": {
      "document_id": "019a…",
      "kind": "Invoice",
      "txn_date": "2026-09-13",
      "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
      "lines": [
        { "kind": "Item", "item_id": "rt-50-red", "qty": "12", "amount": "239.40",
          "is_taxable": true }
      ]
    }
  }
}
```

Returns `{ document_id, version, number, due_date, total, tax, is_voided,
reversal_entry_ids, entry_id, entry, locked_through }` — `entry_id` is `null`
when nothing new posted.

### `void_document`

Voids a saved document: it posts a reversal rather than vanishing (§6). A
new version is saved with `is_voided` true and `Voided: <reason>` leading its
memo, and every live entry is reversed as of its own date with
`VOID: <memo> (<reason>)` on the reversal. A voided invoice or bill drops off
`open_documents` and `aging`; a voided payment or credit memo releases what
it applied. `reason` is required. Refused: a document already voided; an
invoice or bill something is still applied against (void or edit that payment
or credit first); a closed period. Returns the same fields as
`update_document`.

```json
{
  "name": "void_document",
  "arguments": { "document_id": "019a…", "reason": "entered twice", "actor": "dan" }
}
```

### `apply_credit`

Applies an open credit memo to an open invoice of the same customer (or an
open vendor credit to an open bill of the same vendor). The application goes
on the credit, as a new version of it, and `open_documents` and `aging` net
it on both sides. A credit posts the same whatever it is applied to, so no
entry moves and AR still ties to 1200. `amount` (decimal string or minor
units) defaults to the smaller of the two open balances and may not exceed
either. Refused on a voided document, a different contact, the wrong kind of
target, and a credit dated on or before `locked_through` (applying it would
change a closed period's aging). Returns the `update_document` fields plus
`target_document_id` and `applied` (how much of the credit now sits on that
target).

```json
{
  "name": "apply_credit",
  "arguments": { "credit_id": "019b…", "target_document_id": "019a…",
                 "amount": "25.00", "actor": "dan" }
}
```

### `reverse_entry`

The only correction path (`LEDGER-DESIGN.md` §4): posts a new entry with the
same lines, sides swapped, dated `on`.

```json
{
  "name": "reverse_entry",
  "arguments": { "entry_id": "01…", "on": "2026-09-15", "actor": "dan" }
}
```

### `propose_adjustment` / `decide_adjustment`

The accountant queue (`LEDGER-DESIGN.md` §8): an accountant proposes,
validated for balance and the class rule immediately; Dan decides.

```json
{
  "name": "propose_adjustment",
  "arguments": {
    "requested_by": "joel",
    "description": "reclassify August shop supplies",
    "lines": [
      { "account": "6100", "class": "foam", "debit": "125.00" },
      { "account": "1100", "credit": "125.00" }
    ]
  }
}
```

```json
{
  "name": "decide_adjustment",
  "arguments": {
    "request_id": "01…",
    "decided_by": "dan",
    "approve": true,
    "note": "agreed"
  }
}
```

### `bank_confirm_proposal`

Turns a bank line's suggested proposal into a posted `BankLine` document
against the account a human confirms.

```json
{
  "name": "bank_confirm_proposal",
  "arguments": { "line_id": "01…", "account": "6600", "actor": "dan" }
}
```

### `close_period` / `reopen_period`

The period gate itself. `close_period` locks the book through a date, after
snapshotting the trial balance. `reopen_period` is loud by design (D7): it
always writes a `close_history` row with `is_reopen: true`, whatever `note`
says — there is no quiet way to relock a period back to where it was.

```json
{
  "name": "close_period",
  "arguments": { "period_end": "2026-08-31", "actor": "dan", "note": "August close" }
}
```

## Payment tool

### `payment_link`

An Authorize.net hosted payment page for one open invoice (D15, D30,
`docs/AUTHNET.md`). The request carries the invoice's own `number` as
`invoiceNumber` and its open balance (total less every payment and credit
applied so far) as the amount, so when the batch settles `ledger authnet
import` applies the payment back to this invoice and it leaves the AR aging.

Refused, with `isError` and no call to Authorize.net, for a document that is
not an invoice, a voided invoice, a fully paid one, one with no number or a
number over 20 characters, and while an import replay is in progress. Writes
nothing to the ledger and moves no money: the customer pays on Authorize.net's
own page. Not a write tool, so its result carries no `locked_through`.

```json
{ "name": "payment_link", "arguments": { "document_id": "0192d4c8-..." } }
```

Returns `{ document_id, invoice_number, amount, environment, url, token,
expires_at, html }`. **The token lasts 15 minutes and must be POSTed** to
`url` as form field `token` — a GET of `url` does nothing. `html` is a small
page that does the POST as soon as it opens; save it and open it, or hand it
to the customer at once. `environment` is `"sandbox"` unless production is
enabled in the config.

## Document tools

D39: the PDFs QBO renders today — invoice, estimate, sales receipt, credit
memo, purchase order and a customer statement — built from the own book
(`apps/ledger/src/render.rs`). Both tools read the ledger and write one file;
neither writes the ledger, **neither sends anything** (emailing a document is
a separate, gated step), and both are refused while an import replay is in
progress. The same output comes from the CLI: `ledger render document` and
`ledger render statement` (`ledger --help`).

**What prints.** Everything comes from saved documents and master data: the
newest version of the document; the contact's display or company name and
bill/ship addresses (newline-separated in `contacts`); the number, date,
terms (`Net N` from the due date, else the contact's terms) and due date;
each line's item SKU and description (the line's own description first),
quantity, rate (amount ÷ quantity) and amount; subtotal, discount, sales tax
at its rate, total; amount paid and balance due from the same open-item rule
`open_documents` uses; the memo as NOTES. A voided document keeps its lines
and totals, shows `Status: VOID` and a balance of 0.00, and has VOID across
every page. A purchase order prints VENDOR and SHIP TO the company itself.
Long documents paginate, repeating the table header, with "Page x of y" on
every page.

**Company identity** — name, legal name, address lines, phone, email,
website, logo (an 8-bit PNG) and footer — comes from
`[documents.company.<company_id>]` in the config file, never from code:

```toml
[documents]
output_dir = "~/Documents/Ledger Documents"    # one subfolder per company
# pay_url = "https://pay.example.com/i/{number}"   # optional, see below

[documents.company.aquamentor]
name = "Aquamentor"
legal_name = "Suspended Aquatic Mentor Inc"
address = ["Street", "Town, ST 00000"]
phone = "(000) 000-0000"
email = "billing@example.com"
logo = "/absolute/path/to/logo.png"
footer = "Thank you for your business."
```

With no table for the company, the ledger's own company display name prints
alone. A logo that is missing or unreadable prints the name as a wordmark
and adds a warning.

**Where files go.** `LEDGER_DOCUMENTS_DIR`, else `output_dir`, else
`~/Documents/Ledger Documents`, then `/<company_id>`. A directory inside a git
work tree is refused unless it is under that tree's ignored `target/` or
`.local/`, so a customer's PDF cannot be committed by accident. Files are
named `Invoice 1001 - Blue Harbor Swim Club.pdf` (` (VOID)` added for a void)
and `Statement - Blue Harbor Swim Club - 2027-04-20.pdf`; rendering again
overwrites.

### `render_document`

```json
{ "name": "render_document", "arguments": { "document_id": "0192d4c8-...", "pay_link": true } }
```

`document_id` is required; `pay_link` defaults to `true`. Refused for any
kind but the five above. Returns `{ path, page_count, document_id, kind,
number, is_voided, subtotal, tax, total, amount_paid, balance_due, pay_link,
warnings }`.

**Pay link.** For an open, non-voided invoice, when the server has an
Authorize.net client (`ledger-mcp` always builds one from `[authnet]`), the
tool calls the same code as `payment_link`, writes its self-submitting page
beside the PDF as `<name>.pay.html`, and adds a PAY ONLINE box to the
invoice. Because the token lasts 15 minutes (D30), the PDF never carries it:
the box points at `pay_url` (the durable mint-on-click page D30 leaves as a
follow-up) when that is configured, else tells the customer to use the link
sent with the invoice — so the sending step should render (or call
`payment_link`) right before it sends. `pay_link.status` is one of:

| status | meaning |
|---|---|
| `attached` | minted; also `amount`, `environment`, `url`, `expires_at`, `page` (the `.pay.html` path). The token itself is only in that file. |
| `not_applicable` | not an open, non-voided invoice |
| `skipped` | `pay_link: false` |
| `not_configured` | no client, no credentials, or `[authnet]` unusable; `reason` says which |
| `failed` | the gateway or network refused; `reason`, and a warning. The PDF rendered without the box. |

### `render_statement`

```json
{ "name": "render_statement", "arguments": { "contact_id": "cust-blue-harbor", "as_of": "2027-04-20" } }
```

One customer's open invoices and credit memos as of `as_of` (date, number,
due date, amount, open balance, days past due) and the aging buckets
Current / 1-30 / 31-60 / 61-90 / Over 90 / TOTAL DUE. Those are exactly that
customer's row of `aging` (`book: "receivable"`) on the same date — the
tests hold the two equal. Refused for a vendor or an unknown contact. A
customer with nothing open gets a statement saying so. Returns `{ path,
page_count, contact_id, as_of, open_items, aging, warnings }`, `aging` in
`aging`'s row shape.

Not yet on the statement: unapplied customer payments (they park in 2300, not
AR, so `aging` does not carry them either) and an activity section (invoices
and payments in a date range).

## Repointing a skill

`ROADMAP.md` §G names the shape directly: the Shopify, Amazon and wholesale
skills that write QBO today go through the QuickBooks MCP's own tools —
`qbo_sales_create_invoice` among them. Once a skill's writes move to the own
store, that call becomes `save_and_post_document` with `document.kind`
`"Invoice"`, on this server:

```json
{
  "name": "save_and_post_document",
  "arguments": {
    "document": {
      "document_id": "shopify-order-45213",
      "kind": "Invoice",
      "number": null,
      "txn_date": "2026-09-13",
      "due_date": null,
      "contact": { "kind": "Customer", "id": "cust-blue-harbor" },
      "header_class": null,
      "lines": [
        {
          "line_no": 1,
          "kind": "Item",
          "amount": "199.50",
          "class": "foam",
          "item_id": "rt-50-red",
          "account": null,
          "is_taxable": true,
          "qty": "10",
          "unit_cost": null,
          "description": "Rescue tube, 50 inch, red",
          "posting": null,
          "entity": null
        }
      ],
      "tax": { "total_tax": "13.22", "taxable_base": "199.50", "rate": "0.06625" },
      "deposit_to": null,
      "pay_from": null,
      "applications": [],
      "unapplied": 0,
      "is_voided": false,
      "source_ref": "shopify:45213",
      "memo": "Shopify order 45213"
    },
    "actor": "shopify-sync",
    "items": {
      "rt-50-red": {
        "income": "4100",
        "expense": "5000",
        "asset": "1310",
        "default_class": "foam",
        "unit_cost": "8.40"
      }
    }
  }
}
```

The response's `entry_id` is what the skill records as "posted to the own
book"; the outbox record that eventually mirrors this same document to QBO
(`ROADMAP.md` §D) is a separate concern this call does not touch.
