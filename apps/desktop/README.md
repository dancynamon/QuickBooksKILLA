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
2. Set `QBO_LOCAL_DB` to a real replica path before running — `main.rs`
   panics with a clear message if it's unset, the same contract
   `qbo-local-mcp` already has (`docs/MCP.md`).
3. Everything the command itself does — argument parsing, dispatch, error
   shaping — is already written and tested in `apps/desktop/commands`
   (`cargo test -p qbo-desktop-commands`). `src-tauri/src/main.rs`'s
   `query` command is a five-line wrapper: take the lock, call
   `qbo_desktop_commands::query`, map the error to a string. There should
   be nothing left to debug in the command itself — only whatever Tauri's
   own build (icons, bundle identifiers, code signing for a `.dmg`)
   turns up.
4. `apps/desktop/commands` is in the repo's workspace `members` and builds
   here; `apps/desktop/src-tauri` is deliberately not (its `Cargo.toml`
   says why) — add it to `members` once building on macOS with the Tauri
   toolchain installed is the normal case, not this one.

## Screens carried over from the prototype, and screens that stay there

Every M1 read screen is here, reached generically over the document-type
argument the query API already takes rather than one screen per QBO form:
the command palette and reference search, a document viewer with the
lineage rail for any document type, customer and vendor pages, the item
(SKU) page, A/R and A/P aging, open-document queues (open estimates, open
purchase orders, and any other type), the sync status screen, classes, the
chart of accounts, sync state in the chrome, and the realm switcher.

The prototype's ledger, period-close, and manufacturing sections — the
orders queue, build sheets, builds, the ledger and trial balance, period
close, vendor credit application, and progress invoicing — are **not**
carried into this UI. `ROADMAP.md` §7 ("Prototype governance"): those
sections are the first draft of `LEDGER-DESIGN.md`, not shelved design
assets waiting to be ported, and they stay in `prototype/bunzbooks.html`
as design reference until they're lifted into that document.
