# Accountant mode

`LEDGER-DESIGN.md` §8, `DECISIONS.md` D7, D17. What Joel gets, how a request
flows from him to the book, what QBO's accountant toolbox has that this
deliberately does not, and how Joel reaches it (W24 — proposed, awaiting
Joel).

Everything below is implemented in `apps/ledger/src/accountant.rs`: GL
detail and the audit trail, CSV exports and the year-end pack, the
adjusting-entry request queue, and `AccountantView`, the type Joel's process
actually gets. The handoff pack Joel receives — a read-only snapshot of the
book plus every export — is `apps/ledger/src/handoff.rs`. Nothing here is a second way to change the book — every write
still goes through `crate::store::Ledger::post_entry`, the same period gate
and class rule as every other posting.

## What Joel gets

A period-locked, read-only view of one or both companies' books:

- **General ledger detail** — every posted line for an account or the whole
  chart, in date order, with a running balance, entry id, source type and
  id, memo, class, entity, whether it is flagged, and what it reverses if
  anything (`ledger gl`).
- **Trial balance, P&L, balance sheet, sales tax lines** — the same reports
  `ledger tb` / `pnl` / `bs` / `tax` already produce, reachable through
  `AccountantView` without a posting permission attached.
- **Audit trail** — every posting since the last close (or since a date he
  names), flagged entries first, each carrying the actor and command that
  produced its source document straight off the oplog (`ledger audit`).
  Filtering to manual and imported entries is just filtering the result to
  `is_flagged`, since every manual and imported entry carries that flag.
- **Close history** — the full `close_history` log, reopens flagged,
  readable and not writable through this role.
- **CSV exports** — a stable, two-decimal CSV per report. `ledger export`
  writes the five-file year-end CSV set (`tb-`, `pnl-`, `bs-`, `gl-`,
  `close-history.csv`) into one directory; `ledger accountant pack` writes
  the full handoff pack (below), which adds P&L by class, the audit trail,
  the adjustment queue, **AR and AP aging by contact** (`books::aging`, D28),
  and the §9 sales tax lines for each quarter. **There is no PDF yet.** §8
  calls for a PDF alongside the CSV — "the CSV is the one Joel's software
  imports; the PDF is the one that goes in the file" — but that arrives
  with the UI, which does not exist yet. Aging is summary by contact only;
  aging *detail* (one line per open document) is `open_documents`, which
  the MCP server exposes but the pack does not yet export.
- **The adjusting-entry request queue** — see below.
- **The reclassify tool** — a specialised request that reclassifies one
  posted line to a different account or class, always produced as a
  two-line balanced proposal, never as an edit to the original entry.

None of this is gated by a runtime role check. `AccountantView` simply does
not have a `post_entry`, `close_period`, `reopen_period` or `save_document`
method — the read-only, period-locked guarantee is a fact about the type
that the compiler enforces everywhere the type is used, not a permission
flag a bug could route around.

## How a request flows

```
Joel                              Dan                         The book
 |                                 |                              |
 | propose_adjustment / reclassify|                              |
 |-------------------------------->                              |
 |   (validated immediately:      |                              |
 |    balanced? class rule OK?)   |                              |
 |                                 |                              |
 |          adjustment_requests row, state 'proposed'             |
 |                                 |                              |
 |                          decide_adjustment                     |
 |                                 |---- reject: state 'rejected',|
 |                                 |     decision recorded, nothing
 |                                 |     posts                    |
 |                                 |                              |
 |                                 |---- approve: saves a          |
 |                                 |     JournalEntry-kind         |
 |                                 |     LedgerDocument, posts it  |
 |                                 |     flagged through           |
 |                                 |     post_entry ----------------> journal_entries,
 |                                 |     state 'posted',           |  journal_lines
 |                                 |     posted_entry_id recorded  |
```

Joel proposes: a date (implicitly `now`, since a request has no separate
"as of" — it *is* the transaction being asked for), lines, accounts,
classes, memo, reason. Two things are checked before the request is even
stored, so a mistake is a rejection Joel sees immediately rather than
something Dan discovers at approval time:

1. **The lines balance.** Same rule as every posted entry.
2. **The §3 class rule.** Every income, COGS or expense line (4xxx, 5xxx,
   6xxx/7xxx) carries exactly one class; every balance-sheet line carries
   none.

A request whose effective date falls on or before the company's current
`locked_through` is still accepted as a proposal, but comes back flagged
`period_closed: true` — because approving it will hit the same period gate
every other posting does, and needs a reopen first (§5). That flag is not
stored; it is recomputed against whatever `locked_through` is *now*, since a
period that was open when the request was made can close before Dan decides
it, and one that was closed can be reopened in between.

Dan decides once. **Rejecting** records the decision and nothing else — no
entry, no `posted_entry_id`. **Approving** builds a `JournalEntry`-kind
document from the request's lines, saves it, and posts it flagged
(`is_flagged = 1`, same flag every manual and imported entry carries)
through the ordinary posting path — so a request approved into a closed
period fails exactly the way any other late posting would, and the fix is
the same one: reopen the period, then approve again. A request that has
already been decided cannot be decided again; the second call is refused
rather than silently overwriting the first decision.

**Reclassify** is a request builder, not a new kind of posting. It selects
one posted line by `(entry_id, line_no)` and proposes exactly two lines: a
reversal of that line on its original account and class, and a repost of
the same amount to the new account and class. The original entry is never
touched — a correction is always reversal plus replacement (§4), and a
reclassify is just that pattern with both halves inside one proposal.

## What QBO's accountant toolbox has that this does not

Restated from `LEDGER-DESIGN.md` §8, so it is written down once rather than
discovered in January:

- **No 1099 wizard.** Out of scope entirely (D17).
- **No "prep for taxes" or books-review workflow.** That is QBO's product
  wrapped around QBO's data model; Joel already does this work in his own
  software against the exports above.
- **No undo reconciliation.** A reconciliation is undone by reopening the
  period, which is loud by design (§5) — there is no quieter path.
- **No bulk reclassify that posts directly.** The reclassify tool exists;
  it only ever produces a request.
- **No accountant-only direct journal entry.** Same reason — everything
  Joel drafts is a proposal until Dan decides it.
- **No multi-client dashboard, no ProAdvisor list.** Joel has two companies
  here, not a practice.
- **No "close the books" password.** A password Dan also knows is not a
  control; the period lock rejects instead of asking nicely.
- **No write-off tool.** Bad debt is one document Dan creates, not an
  accountant-side action.
- **No PDF export yet** — see "What Joel gets" above. Named in §8's scope;
  it waits on the UI. (AR/AP aging summary is in the pack; aging detail is
  not yet exported.)

## How Joel reaches this (W24 — proposed, awaiting Joel)

**Proposed answer: a local install reading a dated, read-only snapshot**,
handed to Joel on request. Built and tested; it becomes the answer when Joel
accepts it in writing (`LEDGER-DESIGN.md` §13, W24 — a go/no-go item).

| Option | What it costs | What it risks |
|---|---|---|
| **Local install, read-only snapshot** (proposed) | One signed build plus `ledger accountant pack`. No running cost. | Joel sees an as-of copy, not a live one — fine for year-end, wrong for a question about last week. |
| Hosted read-only view | A server, an auth story, TLS, a security posture for a machine holding both companies' complete books, a monthly bill. Weeks of work, and the first thing in this project that is not local-first. | A new thing to keep running before the cutover, for a use case (year-end handoff) that does not need it to be live. |

The handoff is a scheduled event rather than a live conversation, and the
local copy is the cheaper, reversible choice — a hosted view can be built
later over the same `AccountantView` if the snapshot turns out to be the
wrong shape.

### Dan: make a pack

```bash
ledger accountant pack --db .local/ledger.sqlite --company aquamentor --as-of 2026-12-31 --out ~/Desktop/for-joel
```

Run it once per company (`--company waterline` for WaterLine CNC). It writes
`~/Desktop/for-joel/aquamentor-2026-12-31/` and refuses if that folder
already exists — a pack is never written over; move the old one aside to
make a new one. Zip the folder and send it however Joel takes files.

What is in it:

| File | What |
|---|---|
| `book.sqlite` | A `VACUUM INTO` copy of the **whole** ledger file as it stood when the pack was made (both companies, everything posted so far, not cut off at `--as-of`). Rollback-journal mode, so it opens without writing `-wal`/`-shm` files. |
| `tb-<as-of>.csv` | Trial balance as of the date. |
| `pnl-<year>.csv` | P&L year to date by account. |
| `pnl-by-class-<year>.csv` | Income, COGS and expense year to date by account and class. |
| `bs-<as-of>.csv` | Balance sheet as of the date. |
| `gl-<year>.csv` | GL detail for the year, with running balances. |
| `audit-trail.csv` | Every posting since the last close, with actor, command and timestamp. |
| `close-history.csv` | Every close and reopen. |
| `adjustments.csv` | The adjustment request queue, one line per proposed journal line. |
| `ar-aging-<as-of>.csv`, `ap-aging-<as-of>.csv` | Aging by contact in QBO's buckets, with a `TOTAL` line that ties to 1200 and to 2000 on the trial balance. |
| `sales-tax-<year>.csv` | §9 lines A–D, plus line E and the E − C variance, for each quarter that has ended by `--as-of`. |
| `manifest.json` | Company, as-of, generated-at, schema version, tax rate, the trial balance totals, every table's row count in the snapshot, and each file's SHA-256, size and CSV row count. |
| `SHA256SUMS` | The same hashes (plus the manifest's) in `shasum -a 256 -c` form. |

Every CSV is rendered **from the snapshot**, not from the live book, so the
folder and the file cannot disagree. Two packs of the same book for the same
`--as-of` are byte-identical apart from `generated_at` (and so the
manifest's own line in `SHA256SUMS`).

### Joel: open it

```bash
ledger accountant open --pack aquamentor-2026-12-31
```

This re-hashes every file against `manifest.json`, opens `book.sqlite`
read-only, checks its trial balance still totals what the manifest says, and
prints the trial balance and how far the books are closed. Without the
`ledger` binary, the hashes alone check with:

```bash
shasum -a 256 -c SHA256SUMS
```

Every report reads the snapshot directly — `tb`, `pnl`, `bs`, `tax`, `gl`,
`audit` and `adjust list` open a write-protected `--db` read-only:

```bash
ledger gl --db aquamentor-2026-12-31/book.sqlite --company aquamentor --from 2026-01-01 --to 2026-12-31
```

and so does the MCP server, for asking Claude about the books:
`LEDGER_DB=aquamentor-2026-12-31/book.sqlite LEDGER_COMPANY=aquamentor ledger-mcp`.

### Why Joel's copy cannot post

Three layers, any one of which is enough, all tested
(`handoff::tests::every_write_is_refused_against_a_snapshot`):

1. Every file in the pack is mode `0444`.
2. `Ledger::open_read_only` opens the snapshot `SQLITE_OPEN_READ_ONLY` and
   sets `PRAGMA query_only`, and runs no migration — SQLite refuses every
   write statement, so `post_entry`, `close_period` and even
   `AccountantView::propose_adjustment` fail. A snapshot made by an older
   build is refused rather than read against the wrong schema.
3. The MCP server's write gate refuses all fourteen write tools up front
   with "this ledger is a read-only accountant snapshot".

So on the snapshot Joel reads everything and proposes nothing. His adjusting
entries reach Dan as he sends them today (email, his own workpapers), and
Dan enters each one on the live book with `ledger adjust propose` and then
decides it — the request queue above is unchanged. If that round trip turns
out to be the friction point, the fix is a proposals file the pack can
import, not write access to the copy.
