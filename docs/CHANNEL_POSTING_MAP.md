# Channel Posting Map (D26 draft), 2026-09-30

Approved by Dan 2026-09-30: model #3 (QuickBooksKILLA ingest + gated QBO posting), payout-level books + line-level reports, backfill from 2026-01-01, restate Jan-Jun to gross, keep QBO native Amazon sync for Aug+ and add payout matching, family NonInventory items, pilot month = July 2026.

## Principles
1. One QBO posting unit per payout/settlement (Shopify payout id, Amazon settlement id, Walmart recon period, Etsy deposit). External id = `<channel>:<payout_id>` stored in QuickBooksKILLA; unique; re-runs never double-post.
2. Gross revenue, fees as separate expense (W13). Marketplace-facilitator tax (Amazon, Walmart, Etsy) is excluded entirely (collected and withheld nets to zero). Shopify-collected tax is a liability.
3. Every clearing account nets to zero per payout: receipt lines - refunds - fees - reserve movements = payout amount = bank deposit.
4. Channel identity = QBO Location (already used: "Amazon AquaMentor US"); add Shopify, Walmart, Etsy locations. P&L by Location = channel P&L.
5. Unit/SKU detail lives in QuickBooksKILLA; QBO gets product-FAMILY NonInventory items (no QtyOnHand, no COGS side effects).
6. Shopify orders with gateway `amazon` (351 orders, #4911-#5503, Apr 23-May 28, $73,854.65) and `ebay` (#4762) are Marketplace Connect mirrors: EXCLUDED from Shopify revenue and unit reports; the Amazon side is the source of truth.

## QBO accounts (existing Id where it exists; NEW = create at pilot gate)
| Role | Shopify | Amazon | Walmart | Etsy |
|---|---|---|---|---|
| Clearing (Bank type, reconcilable) | NEW "07-Shopify Payments balance" | 463 "08-Amazon.com balance" | 459 "89-Walmart" | 450 "94-Etsy" |
| Product income | via family items -> 497 Sales of Product Income | same | same | same |
| Shipping income | 481 Shipping Income | 481 | 481 | 481 |
| Discounts/promos | 548 Discounts given | 548 | 548 | 548 |
| Selling/processing fees | NEW sub "SHOPIFY FEES" under 285 Merchant Account Fees | 465 AMAZON FEES | 470 WALMART FEES | 471 ETSY FEES |
| Fulfillment fees | n/a | 554 Amazon FBA Fees | - | - |
| Advertising billed against payout | - | 537 AMAZON ADVERTISING/MARKETING | - | Etsy marketing -> advertising |
| Subscription | card-paid, unchanged | 616 Amazon subscription fees | - | - |
| Channel shipping labels bought through channel | - | postage/freight-out account (confirm Id) | - | postage |
| Chargebacks / adjustments | 605 Channel adjustments | 606 Amazon adjustments | 605 | 605 |
| Reserve held | NEW OCA "Shopify Payments reserve" | 620 Amazon reserve balance | - | - |
| Tax collected (Shopify only) | item "Shopify sales tax collected" -> NJ/other tax liability | excluded | excluded | excluded |

Payout -> bank: Transfer from channel clearing to 308 "02-4161-Checking", matched to the bank-feed deposit (never "Add" as income).

## QBO transactions per payout
- Sales Receipt: Location = channel, Deposit To = channel clearing, DocNumber = next sequential in the SR series (per Dan's numbering rule; payout id goes in memo + PrivateNote). Lines = family items with qty and amount, plus shipping line, discount line, Shopify tax line.
- Refund Receipt: family item lines, Refund From = clearing.
- Expense (Purchase): vendor Shopify/Amazon/Walmart/Etsy, paid from clearing, fee lines by account above.
- Transfer: clearing -> 02-4161-Checking for the payout amount, dated bank deposit date.

## Family items (NEW NonInventory, income 497)
Rescue Tube 50", Rescue Tube 40", Rescue Tube Factory Second, Kickboard, Floating Mat, Pool Noodle, Swim Belt / Flotation, Wristbands & Tags, Lifeguard Chair, Lifeguard Equipment (other), Robotic Pool Cleaner & Parts (dropship), Signs, Custom / Other. SKU -> family table lives in QuickBooksKILLA (sku_map); unmapped SKUs fall to "Custom / Other" and are listed for review.

## Restatement, Jan-Jun (gated per month)
- Shopify: 81 existing deposits credited to 585 AQUAMENTOR.COM SALES -> re-point credit line to Shopify clearing; post payout Sales Receipts + fee Expenses into clearing. 11 Shopify debit Purchases coded to 585 ($5,572.85) -> re-point to clearing/chargeback or correct account (1,000.00 on 2/7 "Payment Thank You-Mobile" looks like a card payment, review).
- Amazon: deposits credited to 463 (Jan-May, $38,513.83) already sit in clearing; deposits credited to "Amazon Sales" (Apr-Jun, $54,308.27) -> re-point to 463; post settlement Sales Receipts + fee Expenses into 463. Result: 463 nets to zero per settlement.
- Etsy: 94-Etsy (-$302.49) gets the matching receipts + fees.

## Amazon Aug+ (native QBO sales-channel sync, kept)
Per-order Sales Receipts to Undeposited Funds + per-order fee Purchases from UF already exist (UF = $29,781.80). Matching method for payouts OPEN until one native payout is inspected (whether the app creates its own payout deposit/transfer). Do not categorize Amazon bank deposits to income under any circumstances.

## Data sources (exports/ folder)
- Shopify: payouts_export_1.csv (payout-level), payment_transactions_export_1.csv (per-charge gross/fee/net with Payout ID), shopify_orders/lines/refunds/transactions_2026.csv (GraphQL).
- Amazon: *CustomUnifiedTransaction*.csv (all settlements, Standard + Invoiced orders, 2021-09-10..2026-09-09; 61 settlements in 2026), 26 settlement V2 .txt files (Invoiced Orders account type only), Date Range report for 9/9-9/30 due 10/1.
- Walmart: pending login. Etsy: pending CSV links.
