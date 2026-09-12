# Accountant mode

`LEDGER-DESIGN.md` §8, `DECISIONS.md` D7, D17. What Joel gets, how a request
flows from him to the book, what QBO's accountant toolbox has that this
deliberately does not, and the one open deployment question (W24).

Everything below is implemented in `apps/ledger/src/accountant.rs`: GL
detail and the audit trail, CSV exports and the year-end pack, the
adjusting-entry request queue, and `AccountantView`, the type Joel's process
actually gets. Nothing here is a second way to change the book — every write
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
- **CSV exports** — a stable, two-decimal CSV per report, and `ledger export`
  writes a five-file year-end pack (`tb-`, `pnl-`, `bs-`, `gl-`,
  `close-history.csv`) into one directory. **There is no PDF yet.** §8 calls
  for a PDF alongside the CSV — "the CSV is the one Joel's software imports;
  the PDF is the one that goes in the file" — but that arrives with the UI,
  which does not exist yet. AR/AP aging (summary and detail) is also in
  scope per §8's table but has no report in `crate::report` today, so there
  is nothing yet to export for it either — both are gaps to close before
  the PDF and aging land, not decisions to leave open.
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
- **No PDF export yet**, and **no AR/AP aging report yet** — see "What Joel
  gets" above. Both are named in §8's scope and neither exists; the PDF
  waits on the UI, and aging waits on a report that has not been built.

## Open: how Joel reaches this (W24)

Unresolved, and explicitly a deployment question rather than an API one —
`AccountantView` is the same type either way:

| Option | What it costs | What it risks |
|---|---|---|
| **Local install, read-only copy** (recommended in `LEDGER-DESIGN.md` §8) | One signed build plus a scripted export producing a dated, read-only SQLite snapshot handed to Joel on request. Days of work, no running cost. | Joel sees an as-of copy, not a live one — fine for year-end, wrong for a question about last week. |
| **Hosted read-only view** | A server, an auth story, TLS, a security posture for a machine holding both companies' complete books, a monthly bill. Weeks of work, and the first thing in this project that is not local-first. | A new thing to keep running before the cutover, for a use case (year-end handoff) that does not need it to be live. |

The recommendation stands: the local install, because the handoff is a
scheduled event rather than a live conversation, and because it is the
cheaper, reversible choice — a hosted view can be built later over the same
read-only role if the snapshot turns out to be the wrong shape. Whichever
way Joel reaches it, what he gets when he does is `AccountantView`.
