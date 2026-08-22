# Prototype — BunzBooks

A visual prototype of the app `qbo-local` is being built toward. Named BunzBooks,
with a sesame-bun wordmark. Single
self-contained HTML file, no build step, no dependencies: open it in a browser.

**All data is fictional**, though shaped after the real Aquamentor book —
aquatic safety equipment, foam fabrication, CNC and UV job work, and the class
dimension (`DESIGN.md` §2.4) running through every document and line.

## What it demonstrates

| | |
|---|---|
| Density | QuickBooks Desktop's information density at 2026 rendering. Dense grids, no cards, no whitespace dashboards. |
| Command palette | `⌘K` / `Ctrl-K`, fuzzy match across every record, reporting how long the local search took. |
| Sync state in the chrome | Last sync, queued writes, and unresolved failures visible at all times — never something you have to go look for. |
| Offline toggle | The `⇅` button in the chrome. Everything still opens instantly; writes queue and go out on reconnect. This is the whole product in one interaction. |
| Optimistic writes | Saving an invoice commits locally in milliseconds and marks it "not yet in QBO" until confirmed. |
| The failure inbox | Conflicts and rejections surfaced for a human decision. Nothing is auto-merged; nothing is silently dropped. |
| Posting preview | Every invoice shows the journal entry it produces, balanced. Forward-looking to the ledger project, which owns the real posting rules. |
| Two companies | Switching realms swaps the whole book. They share nothing. |
| Adjustable text | `A−` / `A+` in the chrome scale the entire interface from 90% to 170%, rows and controls included. |
| Light and dark | The `☀` / `☾` control. Both themes are designed, not inverted. |

Text size and theme persist between sessions where the browser allows storage,
and both are also reachable from the command palette.

## Try this first

Open an invoice, press `⌘S`, and watch it commit locally. Then hit `⇅` to drop
the network and do it again — same speed, and the write waits its turn.

## Status

Design exploration, not production code. Nothing here is wired to the Rust
crates or to QuickBooks. Its job is to make the target concrete enough to argue
with before the UI gets built for real in M1.

## Record logic and reference search

Added after the first review round, and grounded in how QuickBooks actually
models these things rather than invented.

**Reference search.** Type any number into `⌘K`. It resolves against every
number a record can be known by — invoice number, estimate number, PO number,
bill number, payment number, the customer's own PO, a tracking number, or an
amount — and labels which one matched. A typed number is ambiguous (3611 is an
estimate, 21234 an invoice, 4471882 a customer's PO), so the app says why it
matched instead of guessing. Alphanumeric references like `KA-2026-4417` work
the same way.

**Document lineage.** QuickBooks models this with `LinkedTxn`. Every document
carries a chain rail across the top:

- Estimate → Invoice → Payment
- Purchase order → Bill → Bill payment

Steps not yet reached show as `not yet`; reached steps are clickable and carry
their state and amount. Converting an estimate creates an invoice that keeps
the link back, carries the customer PO across, and lands in the outbox as a
pending write.

**Customer PO numbers.** On estimates and invoices, and indexed for search.
Dan's live QuickBooks already has a "P.O. Number" custom field (definition id
3) on both — populated on invoices, empty on estimates.

**Dropship linking.** A vendor PO can name the customer invoice it fulfils.
The PO shows what was charged, what it cost, and the gross margin. The invoice
shows the PO in its linked records.

**Shopify.** Invoices originating from the web store carry the Shopify order
number and link out to the admin.

## Progress invoicing and partial receipt

**Progress invoicing.** One estimate can be billed across several invoices — a
deposit, a stage payment, the balance — each linked back to it. The estimate
stays the contract rather than being consumed by the first invoice. Estimate
3596 is the worked example: 25% at order, 50% at frame completion, 25%
outstanding. The Estimates list shows a billed percentage and a progress bar,
so an accepted estimate under 100% is visibly money still owed to you.

**Partial PO receipt.** Purchase orders carry ordered-against-received per
line plus a receipt history, because goods arrive in more than one delivery.
PO-4469 is the worked example: twelve blue EVA sheets received, eight yellow
backordered. Without per-line quantities a backordered colour looks identical
to a complete order. PO status is derived from the line quantities rather than
stored, so the two can never disagree.

## Vendor credits

A vendor credit — a short shipment, a return, a price adjustment — sits
against the vendor until applied to a bill, and can be split across several.
The Bills list shows credits alongside payments, and the new bill detail view
shows exactly how a balance is reached: total, less each credit, less each
payment, equals what is due.

Applying a credit is never automatic. Which bill a credit lands on is an
accounting decision that changes what you pay this month, so the app proposes
and the user commits.

Bill balance and status are derived from the credits and payments recorded
against the bill, not stored — the same principle as PO status coming from
line quantities. Two fields that can drift apart eventually will.

## Vendors, payments, items and reports

**Vendors.** Vendors were referenced by every PO, bill and credit with nowhere
to go. Each now has a page: open bills, unapplied credit, outstanding POs with
receipt progress, spend to date, and a 1099-NEC flag (the brief calls for 1099
tracking, and one subcontractor is marked to show it).

**Receive payment.** Workflow 5 from the brief. One payment applied across
several open invoices, oldest first, with any remainder shown as unapplied
rather than silently spread — guessing which invoice a customer meant to pay is
how disputes start. Age is colour-coded so the overdue ones are obvious.

**Item detail.** Price, cost, margin, on-hand, units sold, and where-used —
every invoice and estimate the SKU appears on. A SKU page that cannot answer
"where has this been used" is not worth opening.

**Reports.** Profit & loss by product line, and a full A/R aging matrix by
customer and bucket. Both computed from documents rather than stored totals.
The P&L bars are gross margin, not revenue: a line can be large and thin or
small and fat, and those are different questions.

## Manufacturing

The block no tier of QuickBooks Online answers. The design idea is that
**cost flows from the sheet to the finished product automatically**, so moving
a foam price re-prices every product that uses it.

- **Landed cost** — a sheet's true cost is base plus freight, and freight is
  allocated by board-foot rather than by line count. Splitting a mixed pallet
  evenly would quietly make the cheaper material look dearer, and every margin
  downstream would inherit that error.
- **Build sheets** — components, bought parts and labour rolled into a unit
  cost. Yield *divides* rather than subtracts: to ship 2.1 board-feet of part
  at 78% nesting yield you must buy 2.69, and you paid for all of it. Waste is
  a column, not a footnote.
- **Sensitivity** — what a foam price move does to margin, and what better
  nesting is worth. On a rescue tube, five points of yield beats a five percent
  foam discount.
- **Builds** — what was actually cut against what the sheet said, with the
  variance priced. One overrun is noise; the same product over every time is a
  recipe that is lying to you.

## The ledger

Documents describe what happened; the ledger is what happened. Every entry is
generated from a document by one function, so an entry no document justifies is
not expressible — there is deliberately no "new journal entry" button.

- **Posting rules** are written out as a reviewable table. This is accounting
  policy rather than engineering judgement, and three rules encode decisions
  that need challenging: raw material bills capitalise to inventory rather than
  expensing, a vendor credit reduces material cost rather than posting to other
  income, and build variance is a plug against manufacturing variance rather
  than being spread back over unit cost.
- **Trial balance** balances by construction. Accounts that drift to the wrong
  side are flagged contra, which is a real signal — except for genuine contra
  accounts, which carry a flag so the warning is not always on.

## The data in this prototype is fictional

Every name, number and identifier is invented. Customers, vendors, the company
name, the realm id, tracking numbers and customer PO numbers are all made up.
It is safe to hand to anyone.

An earlier revision was **not** — it had been seeded with real customer and
vendor names taken from records used while building it. The amounts were always
invented, but the names were real. They have been replaced.

If you add data to this file, invent it. The moment a real name lands here the
page stops being shareable and nothing in the file will tell you that happened.

## Local snapshots of the real book

Real figures live in `.local/`, which is gitignored in full — one ignored
directory rather than a list of filenames someone will forget to extend.

```sh
python3 tools/build-snapshot.py     # .local/qbo/*.json -> .local/snapshot.html
```

The extracts are refreshed by asking Claude in a session connected to
QuickBooks; the pull goes through the QuickBooks connector rather than the
script, because it needs OAuth credentials the script deliberately does not
hold.

Three ways to stay current, in order of effort:

1. **Ask for a refresh** — about a minute, good for a weekly look.
2. **Scheduled refresh** — the same pull on a routine, current when opened.
   Still a snapshot.
3. **Real sync** — `qbo-local`. The only option where the data is genuinely
   live rather than as-of.

A published page cannot poll QuickBooks itself: the artifact runtime can call
claude.ai connectors, but a session-level MCP server does not qualify, and a
page declaring connector access cannot be shared at all.
