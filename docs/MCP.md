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

Phase D adds document-write tools — create an estimate, record a payment,
and the rest of the outbox-backed write path (`DESIGN.md` §6) — over this
same server. Until then, any skill that needs to *write* QBO keeps talking
to the QuickBooks MCP; only reads move here.
