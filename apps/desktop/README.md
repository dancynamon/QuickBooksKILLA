# `apps/desktop` — the M1 read UI

`ROADMAP.md` §B1, stack decision A (`DECISIONS.md` D14): a Tauri v2 shell
around the prototype's vanilla view code, with the prototype's fake data
layer replaced by a real provider over `qbo-local`'s read-only query API.
No framework, no bundler. This directory is built so that the Mac
session's Tauri step — the one part that needs the toolchain and macOS —
is wiring, not writing.

```
apps/desktop/
  ui/            the whole frontend: static HTML/CSS/JS, no build step
  commands/      qbo-desktop-commands — the Tauri command's entire body,
                 in the workspace, builds and tests on this machine
  src-tauri/     the Tauri v2 scaffold — NOT in the workspace (see below)
  README.md      this file
```

## Running the UI against fixtures

No Rust, no Tauri, no network — just a static file server:

```sh
cd apps/desktop/ui
python3 -m http.server 8000
# open http://localhost:8000
```

`js/data/index.js` checks for `window.__TAURI__`; finding none, it selects
`js/data/fixture.js`, an in-memory dataset shaped exactly like `docs/MCP.md`'s
eleven tool results (`prototype/README.md`: "invent nothing real" — the
fixture's two companies and every name and number in them are invented the
same way the prototype's are). Every screen renders against it precisely
the way it would against a live replica, because every screen reads only
through `provider` — `js/data/fixture.js` and `js/data/tauri.js` are the
only two files that know which backend answered.

Try the command palette (`⌘K` / `Ctrl-K`): search a document number
(`21234`), a customer PO (`KA-2026-4417`), a SKU (`XRT-40-SFSP`), or free
text. Switch companies with the button at the top left — realm switching is
real here too, not decorative: it calls `provider.setRealmId` and the
whole UI re-renders off the other realm's data.

## The ledger provider

`js/data/ledger/provider.js` is the second data-layer contract, over
`ledger-mcp`'s seventeen tools (`docs/MCP.md`'s ledger section) rather than
`qbo-local`'s eleven: ten reads (`trialBalance`, `profitAndLoss`,
`balanceSheet`, `salesTaxLines`, `generalLedger`, `auditTrail`,
`entriesForDocument`, `listAdjustments`, `bankStatus`, `lockedThrough`) and
seven gated writes (`saveAndPostDocument`, `reverseEntry`,
`proposeAdjustment`, `decideAdjustment`, `bankConfirmProposal`,
`closePeriod`, `reopenPeriod`). It is its own module (`js/data/ledger/`,
mirroring `js/data/`'s own shape one level down) rather than folded into the
qbo-local provider, because `ledger-mcp` is a different server with a
different scoping rule: no `realm_id` on any call — the company is fixed
for the life of the process, so `createProvider(callTool)` here takes no
initial realm the way `js/data/provider.js`'s does.

- **`js/data/ledger/fixture.js`** — an in-memory book, invented the same
  way the qbo-local fixture's documents are, that re-derives
  `apps/ledger/src/report.rs`'s and `accountant.rs`'s own report math
  (trial balance, P&L by account and by class, the balance sheet with
  current-year net income folded into equity, the §9 sales-tax lines and
  their ST-50 mapping, general ledger running balances, the audit trail)
  rather than returning canned per-screen payloads, and actually posts a
  write's entry, enforces the §3 class rule, and refuses a write dated on
  or before its own `locked_through`. `createFixtureProvider()` builds a
  **fresh** book on every call — unlike the read-only qbo-local fixture,
  this one has real writes, and two calls must never see each other's
  posted entries.
- **`js/data/ledger/tauri.js`** — `invoke("ledger_query", { tool, args })`,
  the second Tauri command (below), unwrapping a rejection into a plain
  `Error` the same way `js/data/tauri.js` does.

`js/data/ledger/index.js` picks one at import time the same way
`js/data/index.js` does, exporting the result as `ledgerProvider`. Every
ledger screen under `js/screens/ledger/` imports only `ledgerProvider` —
never `fixture.js` or `tauri.js` directly.

`js/data/ledger/schema.js` is the ledger's schema contract, the same idea
as `js/data/schema.js` but derived from `apps/ledger/src/mcp.rs`'s own
`_json` rendering functions field for field, covering all seventeen tools
including every write's shape (`document_id`/`entry_id`/`entry` and the
rest, always with `locked_through` alongside).

## The ledger screens

Nine screens, one rail section ("Ledger"), reached from the rail, the
command palette, and nowhere else — `LEDGER-DESIGN.md` §4: there is **no
"new journal entry" button anywhere** in this UI. Every entry in the book
is generated from a document or an accountant's decided adjustment, never
hand-posted from a form.

- **Trial balance** (`#ledger-tb`) — every account's debit/credit total and
  signed balance as of a date; a wrong-side account is highlighted, a
  contra account is flagged but never counted as wrong-side.
- **Profit and loss** (`#ledger-pnl`) — income/COGS/expense for a date
  range, toggled by account or by class, with gross margin and net income.
- **Balance sheet** (`#ledger-bs`) — assets, liabilities and equity as of a
  date, current-year net income folded into equity as its own row.
- **Sales tax** (`#ledger-tax`) — a year/quarter picker; lines A through D,
  the line-level check E and its variance against C, and the ST-50 filing
  mapping (line 1 = A, line 2 = D, line 3 = C) printed as a second column.
- **General ledger** (`#ledger-gl`) — a date range and an optional account
  filter, one panel per account with a running balance; clicking a line
  calls `entries_for_document` for the document behind it and expands
  every line of that entry inline.
- **Audit trail** (`#ledger-audit`) — every posting since a date (default
  the last close), flagged (manual and imported) entries first.
- **Adjustments** (`#ledger-adjustments`) — the queue by state, a propose
  form (`ACCOUNT:dr|cr:AMOUNT[:CLASS]`, one line per journal line,
  validated immediately for balance and the §3 class rule), and
  approve/reject with a note; every write's notice shows the
  `locked_through` it came back with.
- **Bank reconciliation** (`#ledger-bank`) — one statement's status
  (matched/proposed/unmatched), a confirm form per proposed line (account
  and class), and the close rule — `opening + matched = closing` — with
  the live difference.
- **Period close** (`#ledger-close`) — the locked-through date, a close
  form with a mandatory note, and a reopen that is loud by design: a red
  banner and a preview of the close-history line it will write, since
  reopening never happens quietly (D7).

`js/screens/ledger/shared.js` carries the two calculations named directly
by their own tests: `st50Rows` (the ST-50 filing mapping) and
`reconciliationDifference` (the bank screen's live difference) — both pure
functions with no DOM, exercised head-on in
`test/ledger-screens.test.js` rather than only through a rendered screen.

## Running the tests

```sh
cd apps/desktop/ui
npm test
```

No dependencies to install — `package.json` declares none. This runs
`node --test`, which auto-discovers `test/*.test.js`. (`node --test test`,
a directory positional argument, does not filter test discovery the way
some Node versions' docs describe on the Node build available in this
environment — v22.22.2 treats a bare positional after `--test` as the
script to run, and fails with `MODULE_NOT_FOUND` on a directory rather than
scanning it. `node --test` with no path argument, or an explicit
shell-expanded glob like `node --test test/*.test.js`, both work; the
`package.json` script here uses the former. Re-check this if you're on a
different Node version — the directory form may simply work there.)

- `test/fixture.test.js` — the schema contract test: asserts the fixture
  provider's output satisfies `js/data/schema.js` (derived from
  `apps/qbo-local/src/store/query.rs` and `mcp.rs`) for all eleven tools,
  across both fixture realms, plus a handful of behavioural checks (lineage
  actually walks a chain, aging buckets a known invoice correctly, an open
  estimate is included and a closed one excluded).
- `test/provider.test.js` — asserts `js/data/index.js` picks the fixture
  provider when no Tauri runtime is present and the Tauri provider when
  `window.__TAURI__` exists, and that `js/data/tauri.js` maps every one of
  the eleven provider methods to the documented tool name and argument
  keys (stubbing `window.__TAURI__.core.invoke` — there is no real Tauri
  runtime on this machine to test against).
- `test/ledger-fixture.test.js` — the ledger's own schema contract test:
  every one of the seventeen tools against `js/data/ledger/schema.js`,
  plus behavioural checks — the book balances, a write dated on or before
  `locked_through` is refused, proposing and approving an adjustment moves
  the trial balance, and confirming a bank proposal resolves that line.
- `test/ledger-provider.test.js` — the ledger's own provider-selection and
  Tauri-mapping test, the same shape as `test/provider.test.js` but for
  `js/data/ledger/tauri.js`'s `invoke("ledger_query", ...)` calls, all
  seventeen tools.
- `test/ledger-screens.test.js` — pure-function tests for
  `js/screens/ledger/shared.js`'s `st50Rows` (line 1 = A, line 2 = D, line
  3 = C) and `reconciliationDifference` (the bank screen's live
  difference), plus `parseAdjustmentLineSpecs` and `quarterOf`.

## The provider contract

`js/data/provider.js` is the interface: one method per MCP tool
(`search`, `documentDetail`, `listDocuments`, `openDocuments`,
`contactDetail`, `itemDetail`, `arAging`, `apAging`, `syncStatus`,
`classTree`, `chartOfAccounts`), each returning a `Promise` of exactly the
JSON shape `docs/MCP.md` documents for that tool — money as two-place
decimal strings, enums as their `as_str` names, both backends alike.
`createProvider(callTool, initialRealmId)` builds all eleven from one
`callTool(tool, args)` function and a realm the provider itself owns
(`setRealmId`/`getRealmId`) — the realm switcher's whole job is calling
`setRealmId` and asking the current screen to re-render; no call site
threads a realm through by hand, and there is no way to accidentally build
a call missing it.

Two backends implement `callTool`:

- **`js/data/fixture.js`** — an in-memory dataset, and enough of
  `store/query.rs`'s and `store/search.rs`'s own logic (aging buckets,
  lineage's breadth-first walk, `open_documents`' status rules, search's
  ranked stages) re-derived in JS to actually exercise what a screen
  depends on, not just return canned per-screen payloads.
- **`js/data/tauri.js`** — `window.__TAURI__.core.invoke("query", { tool,
  args })`, unwrapping a rejection into a plain `Error` carrying the
  failure's own message.

`js/data/index.js` picks one at import time based on `window.__TAURI__`'s
presence and exports the result as `provider`. Every screen under
`js/screens/` imports only `provider` from `js/data/index.js` — never
`fixture.js` or `tauri.js` directly, and never inlines data of its own.

There is no MCP tool to list realms or to list all customers/vendors/items.
Two consequences, both deliberate: `js/data/realms.js` is a small static
config the realm switcher reads (a real installation would be pointed at
its own two companies by whoever set up `QBO_LOCAL_DB`, not asking the
replica what it contains), and there is no "Customers" or "Items" list
screen — a customer, vendor, or item page is reached by search
(`⌘K`) or by following a document's party link, matching
`contact_detail`/`item_detail`'s own one-record-at-a-time shape.

## What the Mac session still has to do

Everything in `src-tauri/` is scaffold, not working code — the Tauri
toolchain and the system libraries it links against are not installed in
this environment, so none of it has been compiled or run:

1. `cargo tauri dev` from `apps/desktop/src-tauri/` (needs the Tauri CLI
   installed; `frontendDist` already points at `../ui`, and there is
   deliberately no `devUrl` — the UI has no dev server, no bundler, and
   under the shipped `tauri.conf.json`'s CSP (`connect-src 'none'`), no
   network at all).
2. Set `QBO_LOCAL_DB` to a real replica path, and `LEDGER_DB`/
   `LEDGER_COMPANY` to a real ledger `.sqlite` path and a company id
   (`"aquamentor"` or `"waterline"`), before running — `main.rs` panics
   with a clear message if any of the three is unset, the same contract
   `qbo-local-mcp` and `ledger-mcp` already have (`docs/MCP.md`). The
   ledger is opened read-write (`Ledger::open`), not read-only like the
   replica, because the write screens (Adjustments, Bank reconciliation,
   Period close) have nowhere else to post to.
3. Everything either command does — argument parsing, dispatch, error
   shaping — is already written and tested in `apps/desktop/commands`
   (`cargo test -p qbo-desktop-commands`). `src-tauri/src/main.rs`'s
   `query` and `ledger_query` commands are each a few lines: take the
   relevant lock, call `qbo_desktop_commands::query` or
   `qbo_desktop_commands::ledger_query`, map the error to a string. There
   should be nothing left to debug in either command itself — only
   whatever Tauri's own build (icons, bundle identifiers, code signing for
   a `.dmg`) turns up.
4. `apps/desktop/commands` (which now depends on `ledger` as well as
   `qbo-local`) is in the repo's workspace `members` and builds here;
   `apps/desktop/src-tauri` is deliberately not (its `Cargo.toml` says
   why) — add it to `members` once building on macOS with the Tauri
   toolchain installed is the normal case, not this one.

## Screens carried over from the prototype, and screens that stay there

Every M1 read screen is here, reached generically over the document-type
argument the query API already takes rather than one screen per QBO form:
the command palette and reference search, a document viewer with the
lineage rail for any document type, customer and vendor pages, the item
(SKU) page, A/R and A/P aging, open-document queues (open estimates, open
purchase orders, and any other type), the sync status screen, classes, the
chart of accounts, sync state in the chrome, and the realm switcher.

The ledger's own nine screens ("The ledger screens" above) are also here
now, over `ledger-mcp` rather than `qbo-local-mcp` — the first draft of
those was `prototype/bunzbooks.html`'s ledger, trial balance, period-close
and bills/payments screens, lifted into `LEDGER-DESIGN.md` and now into
this UI.

The prototype's manufacturing sections — the orders queue, build sheets,
builds, vendor credit application, and progress invoicing — are **not**
carried into this UI. `ROADMAP.md` §7 ("Prototype governance"): those
sections have no counterpart in `LEDGER-DESIGN.md` yet, so they stay in
`prototype/bunzbooks.html` as design reference until they're lifted into
that document the way the ledger sections already were.
