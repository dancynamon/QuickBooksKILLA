#!/usr/bin/env python3
"""Generate the synthetic channel-ingest fixtures under
apps/ledger/tests/fixtures/channel/.

Nothing here is copied from a real export. Names, ids, order numbers, SKUs
and amounts are invented; only the *shape* (column names, preamble, tab and
comma delimiting, how Amazon files a transfer under the next settlement) is
taken from the real Shopify and Amazon exports. That is the same rule
HANDOFF.md section 2.6 sets for QBO fixtures: nothing that reaches `git add`
may contain a real customer, order or balance. The script is deterministic;
re-running it rewrites identical files.

    python3 tools/gen-channel-fixtures.py
"""
import csv
import pathlib
from decimal import Decimal as D

OUT = pathlib.Path(__file__).resolve().parent.parent / "apps/ledger/tests/fixtures/channel"
OUT.mkdir(parents=True, exist_ok=True)


def money(x):
    return f"{D(x):.2f}"


# --------------------------------------------------------------------------
# Shopify
# --------------------------------------------------------------------------
ORDERS = [
    # name, created_at, financial_status, gateways, shipping, tax, total
    ("#9001", "2026-03-09T15:00:00Z", "PARTIALLY_REFUNDED", "shopify_payments", "14.00", "11.62", "186.57"),
    ("#9002", "2026-03-09T16:00:00Z", "REFUNDED", "shopify_payments", "0.00", "0.00", "125.10"),
    ("#9003", "2026-03-12T16:00:00Z", "PAID", "shopify_payments", "0.00", "0.00", "200.00"),
    ("#9004", "2026-03-10T16:00:00Z", "PAID", "amazon", "0.00", "0.00", "65.98"),
    ("#9005", "2026-03-11T16:00:00Z", "PAID", "shopify_payments", "0.00", "0.00", "300.00"),
    ("#9007", "2026-03-11T17:00:00Z", "VOIDED", "shopify_payments", "0.00", "0.00", "50.00"),
]
LINES = [
    # order, sku, title, qty, discounted_total
    ("#9001", "XRT-50", "Aquamentor 50\" ExoTube", 2, "131.96"),
    ("#9001", "KX 120200 BL", "Aquamentor Adult Competition Kickboard", 1, "28.99"),
    ("#9002", "PM-SC-BL-4824", "Swiss Cheese Foam Floating Mat, Made in USA", 1, "139.00"),
    ("#9003", "ZZ-WIDGET-1", "Splash Pool Widget", 1, "200.00"),
    ("#9004", "XRT-40", "Aquamentor 40\" ExoTube", 1, "65.98"),
    ("#9005", "AM-LGC-50", "Aquamentor 50\" Lifeguard Chair", 1, "300.00"),
    ("#9007", "XRT-50", "Aquamentor 50\" ExoTube", 1, "50.00"),
]
REFUNDS = [("#9001", "2026-03-15T14:00:00Z", "50.00")]

with open(OUT / "shopify_orders.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["order_id", "name", "created_at", "processed_at", "financial_status", "fulfillment_status",
                "cancelled_at", "gateways", "source_name", "email", "company", "ship_province", "ship_country",
                "subtotal_current", "discounts", "shipping", "tax", "total", "refunded", "tags"])
    for i, (name, created, status, gw, ship, tax, total) in enumerate(ORDERS):
        w.writerow([f"gid://shopify/Order/{1000 + i}", name, created, created, status, "FULFILLED", "", gw, "web",
                    "", "", "NJ", "US", "0.0", "0.0", ship, tax, total, "0.0", ""])

with open(OUT / "shopify_order_lines.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["order_name", "sku", "variant_id", "product_title", "variant_title", "quantity", "unit_price",
                "discounted_total", "vendor", "requires_shipping", "is_gift_card"])
    for order, sku, title, qty, total in LINES:
        w.writerow([order, sku, "gid://shopify/ProductVariant/1", title, "", qty,
                    money(D(total) / qty), total, "Aquamentor", "True", "False"])

with open(OUT / "shopify_refunds.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["order_name", "refund_created_at", "refund_total"])
    w.writerows(REFUNDS)

# payment transactions: (type, order, amount, fee)
def txn_rows(payout_id, payout_date, status, rows, tx_date):
    out = []
    for kind, order, amount, fee in rows:
        net = D(amount) - D(fee)
        out.append([f"{tx_date} 10:00:00 -0400", kind, order, "visa", "online", status, payout_date, payout_id,
                    payout_date, amount, fee, money(net), "#1", "card", amount, "USD", "USD"])
    return out

A = [
    ("charge", "#9001", "186.57", "5.71"),
    ("charge", "#9002", "125.10", "3.93"),
    ("charge", "#9005", "100.00", "3.20"),
    ("reserved_funds", "#9001", "-20.00", "0.00"),
]
B = [
    ("charge", "#9003", "200.00", "6.10"),
    ("charge", "#9005", "200.00", "6.10"),
    ("refund", "#9001", "-50.00", "0.00"),
    ("chargeback", "#9002", "-125.10", "15.00"),
    ("marketplace sales tax", "#9001", "-2.86", "0.00"),
    ("reserved_funds", "#9001", "20.00", "0.00"),
]
C = [("charge", "#9006", "80.00", "2.62")]   # order not in the export
P = [
    ("9100001", "2026-03-10", "paid", A, "2026-03-09"),
    ("9100002", "2026-03-17", "paid", B, "2026-03-16"),
    ("9100003", "2026-03-24", "in_transit", C, "2026-03-23"),
]
hdr = ["Transaction Date", "Type", "Order", "Card Brand", "Card Source", "Payout Status", "Payout Date", "Payout ID",
       "Available On", "Amount", "Fee", "Net", "Checkout", "Payment Method Name", "Presentment Amount",
       "Presentment Currency", "Currency"]
with open(OUT / "shopify_payment_transactions.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(hdr)
    for pid, pdate, status, rows, txd in P:
        w.writerows(txn_rows(pid, pdate, status, rows, txd))

with open(OUT / "shopify_payouts.csv", "w", newline="") as f:
    w = csv.writer(f)
    w.writerow(["Payout Date", "Status", "Charges", "Refunds", "Adjustments", "Marketplace Sales Tax", "Advances",
                "Reserved Funds", "Fees", "Retried Amount", "Total", "Currency", "Bank Reference",
                "Duties and import taxes", "Labels"])
    for pid, pdate, status, rows, txd in P:
        charges = sum(D(a) for k, o, a, fe in rows if k == "charge")
        refunds = sum(D(a) for k, o, a, fe in rows if k == "refund")
        adj = sum(D(a) - D(fe) for k, o, a, fe in rows if k.startswith("chargeback"))
        mtax = sum(D(a) for k, o, a, fe in rows if k == "marketplace sales tax")
        res = sum(D(a) for k, o, a, fe in rows if k == "reserved_funds")
        fees = sum(D(fe) for k, o, a, fe in rows if k == "charge")
        total = sum(D(a) - D(fe) for k, o, a, fe in rows)
        w.writerow([pdate, status, money(charges), money(refunds), money(adj), money(mtax), "0.00", money(res),
                    money(fees), "0.00", money(total), "USD", "", "0.00", "0.00"])

# --------------------------------------------------------------------------
# Amazon
# --------------------------------------------------------------------------
PREAMBLE = [
    "Includes Amazon Marketplace, Fulfillment by Amazon (FBA), and Amazon Webstore transactions",
    "All amounts in USD, unless specified",
    "Definitions:",
    "Sales tax collected: Includes sales tax collected from buyers for product sales, shipping, and gift wrap.",
    "Selling fees: Includes variable closing fees and referral fees.",
    "Other transaction fees: Includes shipping chargebacks, shipping holdbacks, per-item fees  and sales tax collection fees.",
    'Other: Includes non-order transaction amounts. For more details, see the "Type" and "Description" columns for each order ID.',
]
COLS = ["date/time", "settlement id", "type", "order id", "sku", "description", "quantity", "marketplace",
        "account type", "fulfillment", "order city", "order state", "order postal", "tax collection model",
        "product sales", "product sales tax", "shipping credits", "shipping credits tax", "gift wrap credits",
        "giftwrap credits tax", "Regulatory Fee", "Tax On Regulatory Fee", "promotional rebates",
        "promotional rebates tax", "marketplace withheld tax", "selling fees", "fba fees",
        "other transaction fees", "other", "total"]
NUM = COLS[14:29]


def arow(dt, sid, ty, order="", sku="", desc="", qty="", acct="Standard Orders", **amt):
    vals = {c: "0" for c in NUM}
    for k, v in amt.items():
        vals[k.replace("_", " ")] = v
    total = sum(D(vals[c]) for c in NUM)
    return [dt, sid, ty, order, sku, desc, qty, "amazon.com" if order else "", acct, "Seller" if order else "",
            "", "NJ" if order else "", "", "MarketplaceFacilitator" if order else ""] + [vals[c] for c in NUM] + [money(total)]


S1, S2, S3, S4 = "30000000001", "30000000002", "30000000003", "30000000004"
rows = []
# Settlement 1 (Standard): two orders, a refund, fees, an adjustment.
rows += [
    arow("Mar 2, 2026 9:00:00 AM PST", S1, "Order", "111-0000001-0000001", "XRT-50RD",
         'Aquamentor 50" ExoTube - Lifeguard Rescue Tube', "1",
         product_sales="65.98", product_sales_tax="4.37", shipping_credits="7.50",
         marketplace_withheld_tax="-4.37", selling_fees="-9.90", other_transaction_fees="-0.41"),
    arow("Mar 3, 2026 10:00:00 AM PST", S1, "Order", "111-0000002-0000002", "KX-120190-BL",
         "Aquamentor Swim Kickboard (Blue)", "2",
         product_sales="45.98", product_sales_tax="3.05", promotional_rebates="-5.00",
         marketplace_withheld_tax="-3.05", selling_fees="-6.90", fba_fees="-6.20"),
    arow("Mar 5, 2026 11:00:00 AM PST", S1, "Refund", "111-0000000-0000009", "PM-SC-BL-4824",
         "Aquamentor Floating Mat - Swiss Cheese", "1",
         product_sales="-139.00", product_sales_tax="-9.21", shipping_credits="-7.50",
         marketplace_withheld_tax="9.21", selling_fees="20.85"),
    arow("Mar 6, 2026 8:00:00 AM PST", S1, "Service Fee", desc="Cost of Advertising", other_transaction_fees="-25.00"),
    arow("Mar 6, 2026 8:01:00 AM PST", S1, "Service Fee", desc="Subscription", other="-39.99"),
    arow("Mar 6, 2026 8:02:00 AM PST", S1, "Shipping Services", desc="Shipping Label Purchased through Amazon", other="-8.50"),
    arow("Mar 6, 2026 8:03:00 AM PST", S1, "FBA Inventory Fee", desc="FBA storage fee", other="-3.00"),
    arow("Mar 7, 2026 8:04:00 AM PST", S1, "Adjustment", desc="FBA Inventory Reimbursement - Lost:Warehouse", other="12.00"),
]
# Settlement 2 (Standard): one order (unmapped SKU) and the transfer that pays settlement 1.
rows += [
    arow("Mar 10, 2026 9:00:00 AM PDT", S2, "Order", "111-0000003-0000003", "ZZ-WIDGET-1",
         "Splash Pool Widget", "1", product_sales="200.00", product_sales_tax="13.25",
         marketplace_withheld_tax="-13.25", selling_fees="-30.00"),
]
# Settlement 3 (Invoiced): the one mirrored in V2 below.
S3_ROWS = [
    arow("Mar 3, 2026 12:00:00 PM PST", S3, "Order", "114-0000001-0000001", "BL50",
         "Aquamentor Reusable Vinyl Wristbands - Implement Water Safety", "2", acct="Invoiced Orders",
         product_sales="39.98", product_sales_tax="2.60", shipping_credits="7.50",
         marketplace_withheld_tax="-2.60", selling_fees="-6.00", other_transaction_fees="-0.82"),
    arow("Mar 4, 2026 1:00:00 PM PST", S3, "Order", "114-0000002-0000002", "LGC-50-25",
         "Aquamentor Lifeguard Chair for Pool, Lake and Beach (50\")", "1", acct="Invoiced Orders",
         product_sales="799.00", product_sales_tax="52.94", shipping_credits="0.00",
         marketplace_withheld_tax="-52.94", selling_fees="-119.85", fba_fees="-0.00"),
    arow("Mar 9, 2026 8:30:00 PM PST", S3, "Order", "114-0000003-0000003", "RT-40-A",
         'Aquamentor 40" Rescue Tube', "1", acct="Invoiced Orders",
         product_sales="59.99", product_sales_tax="3.97", shipping_credits="5.00",
         marketplace_withheld_tax="-3.97", selling_fees="-9.00"),
]
rows += S3_ROWS
# Settlement 4 (Invoiced): one order and the transfer that pays settlement 3.
rows += [
    arow("Mar 12, 2026 9:00:00 AM PDT", S4, "Order", "114-0000004-0000004", "RT-50-A",
         'Aquamentor 50" Rescue Tube', "1", acct="Invoiced Orders",
         product_sales="64.99", product_sales_tax="4.31", marketplace_withheld_tax="-4.31", selling_fees="-9.75"),
]
net = lambda sid: sum(D(r[-1]) for r in rows if r[1] == sid)
rows += [
    arow("Mar 11, 2026 3:00:00 AM PDT", S2, "Transfer",
         desc="To your account ending in: 0000, Bank Transfer ID: TESTXFER0001", other=f"{-net(S1):.2f}"),
    arow("Mar 12, 2026 3:00:00 AM PDT", S4, "Transfer", acct="Invoiced Orders",
         desc="To your account ending in: 0000, Bank Transfer ID: TESTXFER0002", other=f"{-net(S3):.2f}"),
]
with open(OUT / "amazon_unified.csv", "w", newline="", encoding="utf-8-sig") as f:
    for line in PREAMBLE:
        f.write('"' + line.replace('"', '""') + '"\n')
    w = csv.writer(f, quoting=csv.QUOTE_ALL, lineterminator="\n")
    w.writerow(COLS)
    w.writerows(rows)

# Settlement V2 flat file for settlement 3: same money, other layout.
V2_COLS = ["settlement-id", "settlement-start-date", "settlement-end-date", "deposit-date", "total-amount",
           "currency", "transaction-type", "order-id", "merchant-order-id", "adjustment-id", "shipment-id",
           "marketplace-name", "amount-type", "amount-description", "amount", "fulfillment-id", "posted-date",
           "posted-date-time", "order-item-code", "merchant-order-item-id", "merchant-adjustment-item-id", "sku",
           "quantity-purchased", "promotion-id"]


def v2(order, item, sku, qty, posted_utc, parts):
    out = []
    for at, ad, amount in parts:
        r = [""] * len(V2_COLS)
        r[0] = S3
        r[6] = "Order"
        r[7] = order
        r[11] = "Amazon.com"
        r[12] = at
        r[13] = ad
        r[14] = amount
        r[15] = "MFN"
        r[16] = posted_utc[:10]
        r[17] = posted_utc + " UTC"
        r[18] = item
        r[21] = sku
        r[22] = qty
        out.append(r)
    return out

v2rows = []
v2rows += v2("114-0000001-0000001", "900000000001", "BL50", "2", "2026-03-03 20:00:00", [
    ("ItemPrice", "Principal", "39.98"), ("ItemPrice", "Tax", "2.60"), ("ItemPrice", "Shipping", "7.50"),
    ("ItemWithheldTax", "MarketplaceFacilitatorTax-Principal", "-2.60"),
    ("ItemFees", "Commission", "-6.00"), ("ItemFees", "GetPaidFasterFee", "-0.82")])
v2rows += v2("114-0000002-0000002", "900000000002", "LGC-50-25", "1", "2026-03-04 21:00:00", [
    ("ItemPrice", "Principal", "799.00"), ("ItemPrice", "Tax", "52.94"),
    ("ItemWithheldTax", "MarketplaceFacilitatorTax-Principal", "-52.94"),
    ("ItemFees", "Commission", "-119.85")])
# 04:30 UTC on the 10th is 20:30 PST on the 9th: the settlement's last day
# must come out as 9 March in both formats.
v2rows += v2("114-0000003-0000003", "900000000003", "RT-40-A", "1", "2026-03-10 04:30:00", [
    ("ItemPrice", "Principal", "59.99"), ("ItemPrice", "Tax", "3.97"), ("ItemPrice", "Shipping", "5.00"),
    ("ItemWithheldTax", "MarketplaceFacilitatorTax-Principal", "-3.97"),
    ("ItemFees", "Commission", "-9.00")])
total = sum(D(r[14]) for r in v2rows)
with open(OUT / "amazon_settlement_v2.txt", "w", newline="") as f:
    w = csv.writer(f, delimiter="\t", lineterminator="\n")
    w.writerow(V2_COLS)
    head = [""] * len(V2_COLS)
    head[0], head[1], head[2] = S3, "2026-03-03 07:00:00 UTC", "2026-03-10 07:00:00 UTC"
    head[3], head[4], head[5] = "2026-03-12 10:00:00 UTC", money(total), "USD"
    w.writerow(head)
    w.writerows(v2rows)

# --------------------------------------------------------------------------
# Walmart: Payments Reconciliation report (synthetic; real column names)
# --------------------------------------------------------------------------
WM_COLS = ["Period Start Date", "Period End Date", "Transaction Key", "Transaction Posted Timestamp",
           "Transaction Type", "Transaction Description", "Customer Order #", "Customer Order line #",
           "Purchase Order #", "Purchase Order line #", "Amount", "Amount Type", "Ship Qty",
           "Commission Rate", "Base Commission Rate", "Transaction Reason Description",
           "Partner Item Id", "Partner GTIN", "Partner Item Name", "Product Tax Code", "Ship to State",
           "Ship to City", "Ship to Zipcode", "Contract Category", "Product Type", "Commission Rule",
           "Shipping Method", "Fulfillment Type", "Fulfillment Details", "Original Commission",
           "Commission Incentive Program", "Commission Saving", "Customer Promo Type",
           "Total Walmart Funded Savings Program", "Campaign Id", "Item Condition", "Original charge",
           "Charge Savings", "Incentive Program Name", "Ship to country", "Store Id"]


def wm_row(**kw):
    r = [""] * len(WM_COLS)
    for k, v in kw.items():
        r[WM_COLS.index(k.replace("_", " "))] = v
    return r


def wm_order(key, when, txn, order, sku, title, price, comm, tax):
    rows = []
    for amt_type, amt in (("Product Price", price), ("Commission on Product", comm),
                          ("Product tax", tax), ("Product tax withheld", "-" + tax.lstrip("-") if not tax.startswith("-") else tax.lstrip("-"))):
        rows.append(wm_row(Transaction_Key=key, Transaction_Posted_Timestamp=when, Transaction_Type=txn,
                           Transaction_Description="Purchase" if txn == "Sale" else "Refund",
                           **{"Customer_Order_#": order, "Customer_Order_line_#": "1",
                              "Purchase_Order_#": "1291" + order[-6:], "Purchase_Order_line_#": "1"},
                           Amount=amt, Amount_Type=amt_type, Ship_Qty="1", Partner_Item_Id=sku,
                           Partner_Item_Name=title, Ship_to_State="NJ", Ship_to_country="USA",
                           Fulfillment_Type="Seller Fulfilled"))
    return rows


wm = []
# Period ending Sat 2026-03-21, paid Tue 2026-03-24.
wm += wm_order("2026_03_10_000001", "03/10/2026", "Sale", "200000000000001", "BL100",
               "Synthetic Vinyl Wristbands 100 ct", "21.99", "-3.30", "1.94")
wm += wm_order("2026_03_12_000002", "03/12/2026", "Sale", "200000000000002", "XRT-50",
               "Synthetic 50\" Rescue Tube", "59.99", "-9.00", "4.20")
wm += wm_order("2026_03_14_000003", "03/14/2026", "Sale", "200000000000003", "ZZ-WIDGET-1",
               "Splash Pool Widget", "10.00", "-1.50", "0.70")
wm.append(wm_row(Transaction_Posted_Timestamp="03/15/2026", Transaction_Type="Release Reserve",
                 Transaction_Description="Reserve credited back", Amount="-5.00", Amount_Type="Fee/Reimbursement"))
# Period ending Sat 2026-04-04, paid Tue 2026-04-07: a refund of the first order and a new sale.
wm += wm_order("2026_03_30_000004", "03/30/2026", "Refund", "200000000000001", "BL100",
               "Synthetic Vinyl Wristbands 100 ct", "-21.99", "3.30", "-1.94")
wm += wm_order("2026_04_02_000005", "04/02/2026", "Sale", "200000000000005", "PM-SC-YL-4824",
               "Synthetic Floating Mat", "89.99", "-13.50", "6.30")
# Period ending Sat 2026-04-18: facilitator tax that does not net (collected 2.00, withheld 1.00).
bad = wm_order("2026_04_10_000006", "04/10/2026", "Sale", "200000000000006", "KX 120200 BL",
               "Synthetic Kickboard", "12.00", "-1.80", "2.00")
bad[3][WM_COLS.index("Amount")] = "-1.00"
wm += bad
# Period ending Sat 2026-09-19, paid Tue 2026-09-22: pending when as_of is earlier.
wm += wm_order("2026_09_10_000007", "09/10/2026", "Sale", "200000000000007", "XRT-40",
               "Synthetic 40\" Rescue Tube", "49.99", "-7.50", "3.50")
with open(OUT / "walmart_recon.csv", "w", newline="") as f:
    w = csv.writer(f, lineterminator="\n")
    w.writerow(WM_COLS)
    w.writerows(wm)

# --------------------------------------------------------------------------
# Etsy: Payment account monthly statement (synthetic; real column names, newest first)
# --------------------------------------------------------------------------
ET_COLS = ["Date", "Type", "Title", "Info", "Currency", "Amount", "Fees & Taxes", "Net", "Tax Details"]


def et(date, typ, title, info="", amount="--", fees="--", net="--"):
    return [date, typ, title, info, "USD", amount, fees, net, "--"]


mar = [  # oldest first here; written newest first below
    et("March 2, 2026", "Fee", "Listing fee", "Listing #900000001", "--", "-$0.20", "-$0.20"),
    et("March 2, 2026", "Sale", "Payment for Order #1001", "", "$67.35", "--", "$67.35"),
    et("March 2, 2026", "Fee", "Processing fee", "Order #1001", "--", "-$2.27", "-$2.27"),
    et("March 2, 2026", "Tax", "Sales tax paid by buyer", "Order #1001", "--", "-$5.35", "-$5.35"),
    et("March 2, 2026", "Fee", "Transaction fee: Synthetic Cork Yoga Mat", "Order #1001", "--", "-$4.03", "-$4.03"),
    et("March 3, 2026", "Marketing", "Etsy Ads", "Bill for click-throughs to your shop on Mar 2, 2026", "--", "-$0.55", "-$0.55"),
    et("March 3, 2026", "Shipping", "USPS shipping label", "Label #900000002", "--", "-$6.37", "-$6.37"),
    et("March 5, 2026", "Deposit", "$48.58 sent to your bank account"),
    et("March 10, 2026", "Sale", "Payment for Order #1002", "", "$30.00", "--", "$30.00"),
    et("March 10, 2026", "Fee", "Processing fee", "Order #1002", "--", "-$1.05", "-$1.05"),
    et("March 10, 2026", "Tax", "Sales tax paid by buyer", "Order #1002", "--", "-$2.00", "-$2.00"),
    et("March 10, 2026", "Fee", "Transaction fee: Synthetic Cork Yoga Mat", "Order #1002", "--", "-$1.95", "-$1.95"),
    et("March 15, 2026", "Marketing", "Etsy Ads", "Bill for click-throughs to your shop on Mar 14, 2026", "--", "-$0.40", "-$0.40"),
    et("March 16, 2026", "Deposit", "$24.60 sent to your bank account"),
    et("March 20, 2026", "Sale", "Payment for Order #1003", "", "$20.00", "--", "$20.00"),
    et("March 20, 2026", "Fee", "Transaction fee: Synthetic Cork Yoga Mat", "Order #1003", "--", "-$1.30", "-$1.30"),
    et("March 22, 2026", "Deposit", "$99.99 sent to your bank account"),
    et("March 30, 2026", "Marketing", "Etsy Ads", "Bill for click-throughs to your shop on Mar 29, 2026", "--", "-$0.10", "-$0.10"),
]
apr = [
    et("April 2, 2026", "Sale", "Payment for Order #1004", "", "$15.00", "--", "$15.00"),
    et("April 2, 2026", "Fee", "Transaction fee: Synthetic Cork Yoga Mat", "Order #1004", "--", "-$1.00", "-$1.00"),
    et("April 2, 2026", "Marketing", "Etsy Ads", "Bill for click-throughs to your shop on Apr 1, 2026", "--", "-$0.15", "-$0.15"),
    et("April 3, 2026", "Deposit", "$13.75 sent to your bank account"),
    et("April 20, 2026", "Marketing", "Etsy Ads", "Bill for click-throughs to your shop on Apr 19, 2026", "--", "-$0.30", "-$0.30"),
]
for name, rows in (("etsy_statement_2026_3.csv", mar), ("etsy_statement_2026_4.csv", apr)):
    with open(OUT / name, "w", newline="", encoding="utf-8-sig") as f:
        w = csv.writer(f, lineterminator="\n")
        w.writerow(ET_COLS)
        w.writerows(reversed(rows))

print("fixtures written to", OUT)
print("settlement 3 net", money(net(S3)), "v2 total", money(total))
