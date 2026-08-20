# Prototype — Ledger Local

A visual prototype of the app `qbo-local` is being built toward. Single
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
