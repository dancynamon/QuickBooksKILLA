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

## Write tools

### `save_and_post_document`

Saves a document and, unless it is an Estimate or PurchaseOrder, posts the
entry `crate::post`'s rules table derives from it. See "Repointing a skill"
below for a full example. Because an MCP call has no replica to look items
up in, an inline `items` map (item id to `ItemAccounts` JSON) supplies
whatever an `Item` line needs. Returns `{ document_id, version, entry_id,
entry, locked_through }` — `entry_id` and `entry` are `null` for a
non-posting document.

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
