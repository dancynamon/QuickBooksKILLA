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
