# Cowork prompt — Dan's steps for the own book (17 Oct 2026)

Paste everything below the line into a new Cowork session on Dan's Mac,
with the `~/code/QuickBooksKILLA` folder attached. It drives the steps that
need Dan's eyes, portals or decisions; everything else is already built and
merged on PR #1 (`claude/modest-allen-82wp1w`).

---

You are driving a working session with Dan (Aquamentor) to finish the steps
only he can do for the QuickBooksKILLA own book. The code is done and merged
on branch `claude/modest-allen-82wp1w` (PR #1). Read `DECISIONS.md` D28–D42,
`docs/CUTOVER.md`, `docs/NIGHTLY.md`, `docs/AUTHNET.md` and `docs/PAYROLL.md`
in the attached folder before starting.

**Rules for this session**

- Hard stops, always Dan's explicit OK in chat for each one: sending any
  email (draft only), moving money, deleting any file, and any write to live
  QBO (editing an item, a customer, a sales order).
- You never type a password, API key or token. Dan types those himself.
- Your shell is a Linux VM and Terminal on the Mac is click-only for you, so
  every Mac command below is one **Dan** types in Terminal. Give him the exact
  line, then read the result off the screen with a screenshot.
- Real names, amounts and addresses stay under `.local/` (gitignored). Never
  write them into a tracked file.
- Write what you learn to `.local/cowork/answers.md`, one section per step,
  as you go. A Claude Code session picks it up from there.

Work through these in order and tick each off in `answers.md`. Skip one only
if Dan says so, and note why.

## 1. Five decisions (10 minutes, in chat)

Ask each, record the answer and Dan's reason:

1. **Day-of-month payment terms** (D41). The book now supports "due on the
   1st" style terms; 3,126 customers and 49 vendors use one. Keep them as
   they are (recommended), or convert to net days?
2. **Sales receipt and credit memo numbering.** Channel imports pushed those
   series to 249284 and 512753 in QBO. Continue from there in the own book,
   or restart its own series (and from what number)?
3. **Customer credits on the receivables aging** (D40). QBO shows unapplied
   customer money as credits on the aging; the own book holds it in 2300 and
   leaves it off. Show it on the aging, or keep it off?
4. **A durable "Pay now" link** on emailed invoices (D30 follow-up) needs a
   small public page that mints the Authorize.net form when clicked. Want it?
   If yes, where would it be hosted (a subdomain of aquamentor.com, a
   service he already pays for)?
5. **Duplicate SKUs in QBO.** Six SKUs are on two items each. Read the item
   list with the QuickBooks tools (read only), list the six pairs for Dan in
   chat, and record which item of each pair should keep the SKU. Change
   nothing in QBO unless Dan approves each edit by name.

## 2. Class worksheet review (the biggest one: about 233 items)

The worksheet is generated at `.local/cutover/item-classes.csv`: 3,250
active items, but only 233 sold in the last 12 months, and those are what
matter (D33). Classes are `foam`, `sign`, `chair`, `drop`, `cnc`, `uv`.

1. Ask Dan whether to review in a Google Sheet in his own Drive (easier), or
   in the CSV. If the Sheet: with his OK, create one named "Item classes —
   own book" containing only rows with `sales_lines_12m` > 0, sorted by
   `sales_amount_12m`, the `suggested_class` and `rule` columns visible, and
   an empty `class` column with a dropdown of the six classes.
2. Walk him through it top down. Copying a suggestion is his decision;
   leaving it blank is not. Rows he hasn't decided stay blank.
3. Export his decided rows back to `.local/cutover/item-classes.csv` (only
   `qbo_id` and `class` are read; delete undecided rows). Record the counts
   per class in `answers.md`. Don't run `ledger classes apply`: the Claude
   Code session does that.

## 3. Sales-order feed, by hand (D42)

The cutover import refuses to run until a sales-order feed is in the
replica. The tool that produces the feed doesn't exist yet. Until it does,
write the feed by hand:

1. Read QBO's sales orders with the QuickBooks MCP tools (`list_sales_orders`,
   `get_sales_order`), read only.
2. Write `.local/cutover/sales-orders.json` in the `qbo-sales-orders/1`
   format (`apps/qbo-local/src/sales_orders.rs`, `SalesOrderFeed` and
   `FeedSalesOrder`; camelCase keys). Include every order still "Not
   invoiced" or partly invoiced, plus the newest order whatever its status,
   with `"complete": true` and `capturedAt` set to now.
3. Show Dan the count and the order numbers, not the amounts, for his OK.
   Don't apply it: syncing needs the rebuilt daemon from step 6 first.
4. Ask Dan whether a Claude Code session should build the real producer in
   `~/code/aquamentor-mcp` (qbo-headless). Record his answer.

## 4. Payroll mapping (D38)

Each pay run's wages post by class, so every employee needs one.

1. With Dan, open SurePayroll in the browser and read the employee list and
   their SurePayroll employee ids (read only). Never open Enter Payroll and
   never submit anything.
2. For each employee, ask Dan which class (and, for office staff, whether
   wages go to 6900 rather than 5400).
3. Dan adds the `[payroll.employees]` table to `.local/config.toml` from
   `.local/config.example.toml`'s example. Write the exact block for him
   in `.local/cowork/payroll-block.toml`, and he pastes it in.

## 5. Authorize.net sandbox (D30)

1. Dan creates an Authorize.net **sandbox** account himself (developer
   sandbox sign-up; `docs/AUTHNET.md` has the page). You only point to it.
2. In the sandbox Merchant Interface he finds the API Login ID and
   Transaction Key.
3. He types, in Terminal: `cd ~/code/QuickBooksKILLA && ./target/release/ledger authnet credentials --environment sandbox`
   and pastes the two values when it asks. They go to the macOS keychain,
   never a file.
4. Record only "sandbox credentials stored: yes/no" in `answers.md`.

## 6. Open the desktop app and look at it

1. Dan types in Terminal: `cd ~/code/QuickBooksKILLA && tools/desktop.sh --open`
2. Take a screenshot of the window. Ask Dan to click through New document,
   Receivables, Customers and Products and services. Screenshot each.
3. Note anything broken, unreadable or confusing in `answers.md`, with the
   screen and what Dan expected.

## 7. Switch on the nightly run (optional today)

Ask Dan whether to switch it on now or wait until January. If now, he
follows `docs/NIGHTLY.md`, "Switching it on", typing each line himself
(`tools/nightly.sh install` refuses anything but a person at a terminal
answering `yes`). This also rebuilds and restarts the qbo-local daemon,
which the sales-order sync in step 3 needs. Then, at his OK, he runs the
sync line from `docs/CUTOVER.md` ("Sales orders: the feed"). Screenshot
`tools/nightly.sh status` and record the result.

## 8. Joel's list (draft only)

Draft, do not send, an email from Dan to Joel (the CPA) asking for:

- the QBO → own-book account mapping table for the 184 accounts (D35);
- his answers on D33 (classes), D34 (items on non-income accounts), D36
  (inventory relief) and D40 (customer credits on the aging);
- W21 (one opening-balance entry for everything before 1/1/27) and W24 (the
  accountant pack as a read-only snapshot);
- a time to walk through the accountant pack.

Keep it short, in Dan's voice (the aquamentor-email-style skill if
available). Save it as a Gmail draft only after Dan approves the text.

## Finish

Summarize in chat what was done, decided and left. Make sure `answers.md`
has every answer. Tell Dan to go back to the Claude Code session and say
"Cowork steps done", so it can apply the answers (classes, numbering,
terms, feed, payroll) and re-run the cutover rehearsal.
