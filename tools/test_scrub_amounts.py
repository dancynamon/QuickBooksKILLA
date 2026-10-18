"""Property-style tests for tools/scrub-fixtures.py's amount scrubbing, its
leak detector and its invariant self-check, over randomly generated books.

Run with:

    python3 -m unittest tools/test_scrub_amounts.py

(`cargo test -p qbo-local --test scrub_tools` runs this file and
tools/test_scrub_fixtures.py too.)

Every name, address, amount and note below is generated here and invented.
"""

import importlib.util
import json
import pathlib
import random
import sys
import tempfile
import unittest
from decimal import ROUND_HALF_UP, Decimal

_SCRIPT = pathlib.Path(__file__).resolve().parent / "scrub-fixtures.py"
_spec = importlib.util.spec_from_file_location("scrub_fixtures", _SCRIPT)
if "scrub_fixtures" in sys.modules:
    sf = sys.modules["scrub_fixtures"]
else:
    sf = importlib.util.module_from_spec(_spec)
    sys.modules[_spec.name] = sf
    _spec.loader.exec_module(sf)  # type: ignore[union-attr]

CENT = Decimal("0.01")
REAL_REALM = "9311200000000042"
SEEDS = ["alpha-seed", "bravo-seed", "charlie-seed"]
BOOKS = range(8)


def q2(d):
    return d.quantize(CENT, rounding=ROUND_HALF_UP)


def num(d):
    return float(d)


class BookBuilder:
    """A random, internally consistent book: every rule `check_invariants`
    knows holds exactly, so scrubbing must keep it at zero violations."""

    def __init__(self, seed: int) -> None:
        self.rng = random.Random(seed)
        self.entities = {}  # (type, id) -> raw_json
        self.next_id = 1000
        self.names = []
        self.emails = []
        self.phones = []
        self.streets = []

    def nid(self) -> str:
        self.next_id += 1
        return str(self.next_id)

    def money(self, low=5, high=900) -> Decimal:
        return Decimal(self.rng.randint(low * 100, high * 100)) / 100

    def add(self, entity_type, raw):
        self.entities[(entity_type, raw["Id"])] = raw
        return raw

    def build(self):
        r = self.rng
        # Accounts: a parent with two children, balances rolled up.
        kids = []
        for n in range(2):
            cb = self.money()
            kids.append(
                self.add(
                    "Account",
                    {"Id": f"A{n}", "Name": f"Operating {n}", "AcctNum": f"77{n}31",
                     "ParentRef": {"value": "AP"}, "CurrentBalance": num(cb),
                     "CurrentBalanceWithSubAccounts": num(cb)},
                )
            )
        own = self.money()
        self.add(
            "Account",
            {"Id": "AP", "Name": "Operating", "CurrentBalance": num(own),
             "CurrentBalanceWithSubAccounts": num(own + sum(Decimal(repr(k["CurrentBalance"])) for k in kids))},
        )
        self.add("TaxCode", {"Id": "T1", "Name": "County", "SalesTaxRateList": {"TaxRateDetail": [{"TaxRateRef": {"value": "R1"}}]}})
        self.add("TaxRate", {"Id": "R1", "Name": "County rate", "RateValue": 8.875})
        items = []
        for n in range(4):
            price = self.money(2, 300)
            items.append(self.add("Item", {"Id": f"I{n}", "Name": f"Widget {n}", "UnitPrice": num(price), "PurchaseCost": num(q2(price / 2))}))

        customers = [self.customer(f"Zephyrine Aquatics {r.randint(100, 999)}")]
        customers.append(self.customer(f"Quillon Natatorium {r.randint(100, 999)}"))
        customers.append(self.customer(f"Vandermeer Swim Club {r.randint(100, 999)}", parent=customers[0]["Id"]))
        customers.append(self.customer("Al"))  # a short real name: whole-word only
        for customer in customers:
            self.customer_docs(customer, items)
        self.rollup_customers(customers)

        vendors = [self.vendor(f"Ostrander Foamworks {r.randint(100, 999)}"), self.vendor(f"Pemberly Freight {r.randint(100, 999)}")]
        for vendor in vendors:
            self.vendor_docs(vendor, items)

        self.deposits()
        self.journal(customers)
        return self.entities

    # -- contacts -----------------------------------------------------------

    def contact_fields(self, name):
        r = self.rng
        email = f"ap{r.randint(10, 99)}@{name.split()[0].lower()}-pools.test"
        phone = f"(212) 55{r.randint(0, 9)}-{r.randint(1000, 9999)}"
        street = f"{r.randint(10, 9999)} Kittiwake Esplanade"
        self.names.append(name)
        self.emails.append(email)
        self.phones.append(phone)
        self.streets.append(street)
        return {
            "DisplayName": name,
            "CompanyName": name,
            "PrimaryEmailAddr": {"Address": email},
            "PrimaryPhone": {"FreeFormNumber": phone},
            "BillAddr": {"Line1": name, "Line2": street, "City": "Albany", "CountrySubDivisionCode": "NY", "PostalCode": "12207"},
            "Notes": f"Ask for the owner at {phone}",
        }

    def customer(self, name, parent=None):
        raw = {"Id": self.nid(), **self.contact_fields(name), "Balance": 0, "BalanceWithJobs": 0}
        if parent:
            raw["ParentRef"] = {"value": parent}
        return self.add("Customer", raw)

    def vendor(self, name):
        raw = {"Id": self.nid(), **self.contact_fields(name), "Balance": 0, "TaxIdentifier": "XX-XXX1234"}
        return self.add("Vendor", raw)

    # -- sales ----------------------------------------------------------------

    def sales_lines(self, items, taxable_allowed=True):
        r = self.rng
        lines, leaf, taxable = [], Decimal(0), Decimal(0)
        for n in range(r.randint(1, 4)):
            item = r.choice(items)
            qty = Decimal(r.choice([1, 2, 3, 5, 12, "1.5"]))
            price = self.money(1, 400)
            amount = q2(qty * price)
            code = "TAX" if taxable_allowed and r.random() < 0.6 else "NON"
            lines.append({
                "Id": str(n + 1), "LineNum": n + 1, "Amount": num(amount), "DetailType": "SalesItemLineDetail",
                "Description": f"{item['Name']} at {price} each",
                "SalesItemLineDetail": {"ItemRef": {"value": item["Id"], "name": item["Name"]},
                                        "Qty": float(qty) if qty != int(qty) else int(qty),
                                        "UnitPrice": num(price), "TaxCodeRef": {"value": code}},
            })
            leaf += amount
            if code == "TAX":
                taxable += amount
        return lines, leaf, taxable

    def sales_doc(self, doc_type, customer, items):
        r = self.rng
        lines, leaf, taxable = self.sales_lines(items)
        if r.random() < 0.5:
            lines.append({"Amount": num(leaf), "DetailType": "SubTotalLineDetail", "SubTotalLineDetail": {}})
        discount = Decimal(0)
        if r.random() < 0.4:
            pct = Decimal(r.choice([5, 10, 15]))
            discount = q2(leaf * pct / 100)
            lines.append({"Amount": num(discount), "DetailType": "DiscountLineDetail",
                          "DiscountLineDetail": {"PercentBased": True, "DiscountPercent": int(pct)}})
        tax = Decimal(0)
        raw = {"Id": self.nid(), "DocNumber": str(r.randint(1000, 9999)), "TxnDate": "2026-03-04",
               "CustomerRef": {"value": customer["Id"], "name": customer["DisplayName"]}, "Line": lines}
        if taxable:
            tax_lines = []
            for pct in ([Decimal("4"), Decimal("4.875")] if r.random() < 0.5 else [Decimal("8.875")]):
                amount = q2(taxable * pct / 100)
                tax += amount
                tax_lines.append({"Amount": num(amount), "DetailType": "TaxLineDetail",
                                  "TaxLineDetail": {"TaxRateRef": {"value": "R1"}, "PercentBased": True,
                                                    "TaxPercent": float(pct), "NetAmountTaxable": num(taxable)}})
            raw["TxnTaxDetail"] = {"TxnTaxCodeRef": {"value": "T1"}, "TotalTax": num(tax), "TaxLine": tax_lines}
        total = leaf - discount + tax
        raw["TotalAmt"] = num(total)
        if doc_type in ("Invoice", "CreditMemo"):
            raw["Balance"] = num(total)
        if doc_type == "CreditMemo":
            raw["RemainingCredit"] = num(total)
        if doc_type == "Invoice":
            raw["PrivateNote"] = f"Call {customer['DisplayName']} before delivery"
            raw["BillEmail"] = {"Address": self.emails[-1]}
            raw["CustomerMemo"] = {"value": f"Total due {total}"}
        return self.add(doc_type, raw)

    def customer_docs(self, customer, items):
        r = self.rng
        invoices = [self.sales_doc("Invoice", customer, items) for _ in range(r.randint(2, 5))]
        credit = self.sales_doc("CreditMemo", customer, items) if r.random() < 0.6 else None
        self.sales_doc("SalesReceipt", customer, items)
        self.sales_doc("Estimate", customer, items)
        open_invoices = list(invoices)
        r.shuffle(open_invoices)
        while open_invoices and r.random() < 0.85:
            batch = [open_invoices.pop() for _ in range(min(len(open_invoices), r.randint(1, 3)))]
            self.payment(customer, batch, credit if credit and r.random() < 0.5 else None)
            credit = None if credit and Decimal(repr(credit["Balance"])) == 0 else credit

    def payment(self, customer, invoices, credit):
        r = self.rng
        lines, total = [], Decimal(0)
        for invoice in invoices:
            balance = Decimal(repr(invoice["Balance"]))
            applied = balance if r.random() < 0.6 else q2(balance * Decimal(r.randint(10, 90)) / 100)
            if applied <= 0:
                continue
            invoice["Balance"] = num(balance - applied)
            lines.append({"Amount": num(applied), "LinkedTxn": [{"TxnId": invoice["Id"], "TxnType": "Invoice"}]})
            total += applied
        if credit is not None and lines:
            remaining = Decimal(repr(credit["Balance"]))
            use = min(remaining, q2(total / 2))
            if use > 0:
                credit["Balance"] = num(remaining - use)
                credit["RemainingCredit"] = credit["Balance"]
                lines.append({"Amount": num(use), "LinkedTxn": [{"TxnId": credit["Id"], "TxnType": "CreditMemo"}]})
                total -= use
        unapplied = self.money(1, 50) if r.random() < 0.3 else Decimal(0)
        raw = {"Id": self.nid(), "TxnDate": "2026-04-01", "CustomerRef": {"value": customer["Id"], "name": customer["DisplayName"]},
               "Line": lines, "UnappliedAmt": num(unapplied), "TotalAmt": num(total + unapplied),
               "PrivateNote": f"Check from {customer['DisplayName']}"}
        return self.add("Payment", raw)

    def rollup_customers(self, customers):
        for customer in customers:
            balance = Decimal(0)
            for (t, _), raw in self.entities.items():
                if raw.get("CustomerRef", {}).get("value") != customer["Id"]:
                    continue
                if t == "Invoice":
                    balance += Decimal(repr(raw["Balance"]))
                elif t == "CreditMemo":
                    balance -= Decimal(repr(raw["Balance"]))
                elif t == "Payment":
                    balance -= Decimal(repr(raw["UnappliedAmt"]))
            customer["Balance"] = num(balance)
        for customer in customers:
            kids = [c for c in customers if c.get("ParentRef", {}).get("value") == customer["Id"]]
            customer["BalanceWithJobs"] = num(Decimal(repr(customer["Balance"])) + sum(Decimal(repr(k["Balance"])) for k in kids))

    # -- purchases ------------------------------------------------------------

    def vendor_docs(self, vendor, items):
        r = self.rng
        bills = []
        for _ in range(r.randint(2, 4)):
            lines, total = [], Decimal(0)
            for n in range(r.randint(1, 3)):
                if r.random() < 0.5:
                    item = r.choice(items)
                    qty, price = Decimal(r.randint(1, 20)), self.money(1, 200)
                    amount = q2(qty * price)
                    lines.append({"Amount": num(amount), "DetailType": "ItemBasedExpenseLineDetail",
                                  "ItemBasedExpenseLineDetail": {"ItemRef": {"value": item["Id"]}, "Qty": int(qty), "UnitPrice": num(price)}})
                else:
                    amount = self.money()
                    lines.append({"Amount": num(amount), "DetailType": "AccountBasedExpenseLineDetail",
                                  "AccountBasedExpenseLineDetail": {"AccountRef": {"value": "A0"}}})
                total += amount
            bills.append(self.add("Bill", {"Id": self.nid(), "TxnDate": "2026-03-10", "VendorRef": {"value": vendor["Id"], "name": vendor["DisplayName"]},
                                           "Line": lines, "TotalAmt": num(total), "Balance": num(total)}))
        credit_amount = self.money(5, 40)
        credit = self.add("VendorCredit", {"Id": self.nid(), "TxnDate": "2026-03-12", "VendorRef": {"value": vendor["Id"]},
                                           "Line": [{"Amount": num(credit_amount), "DetailType": "AccountBasedExpenseLineDetail",
                                                     "AccountBasedExpenseLineDetail": {"AccountRef": {"value": "A1"}}}],
                                           "TotalAmt": num(credit_amount), "Balance": num(credit_amount)})
        paid = bills[: r.randint(1, len(bills))]
        lines, total = [], Decimal(0)
        for bill in paid:
            balance = Decimal(repr(bill["Balance"]))
            applied = balance if r.random() < 0.5 else q2(balance / 3)
            bill["Balance"] = num(balance - applied)
            lines.append({"Amount": num(applied), "LinkedTxn": [{"TxnId": bill["Id"], "TxnType": "Bill"}]})
            total += applied
        if r.random() < 0.5 and total > credit_amount:
            lines.append({"Amount": num(credit_amount), "LinkedTxn": [{"TxnId": credit["Id"], "TxnType": "VendorCredit"}]})
            credit["Balance"] = 0
            total -= credit_amount
        self.add("BillPayment", {"Id": self.nid(), "TxnDate": "2026-03-20", "VendorRef": {"value": vendor["Id"]}, "PayType": "Check",
                                 "Line": lines, "TotalAmt": num(total)})
        purchase_amount = self.money()
        self.add("Purchase", {"Id": self.nid(), "TxnDate": "2026-03-22", "EntityRef": {"value": vendor["Id"], "type": "Vendor"},
                              "Line": [{"Amount": num(purchase_amount), "DetailType": "AccountBasedExpenseLineDetail",
                                        "AccountBasedExpenseLineDetail": {"AccountRef": {"value": "A0"}}}],
                              "TotalAmt": num(purchase_amount)})
        balance = sum(Decimal(repr(b["Balance"])) for b in bills) - Decimal(repr(credit["Balance"]))
        vendor["Balance"] = num(balance)

    # -- deposits and journals -------------------------------------------------

    def deposits(self):
        payments = [raw for (t, _), raw in sorted(self.entities.items()) if t == "Payment"]
        if not payments:
            return
        chosen = payments[: max(1, len(payments) // 2)]
        lines, total = [], Decimal(0)
        for p in chosen:
            lines.append({"Amount": p["TotalAmt"], "LinkedTxn": [{"TxnId": p["Id"], "TxnType": "Payment"}]})
            total += Decimal(repr(p["TotalAmt"]))
        other = self.money()
        lines.append({"Amount": num(other), "DetailType": "DepositLineDetail", "DepositLineDetail": {"AccountRef": {"value": "A1"}}})
        total += other
        back = self.money(1, 20)
        self.add("Deposit", {"Id": self.nid(), "TxnDate": "2026-04-02", "DepositToAccountRef": {"value": "A0"}, "Line": lines,
                             "CashBack": {"Amount": num(back), "AccountRef": {"value": "A1"}}, "TotalAmt": num(total - back)})

    def journal(self, customers):
        r = self.rng
        for _ in range(3):
            debits = [self.money() for _ in range(r.randint(1, 3))]
            total = sum(debits)
            k = r.randint(1, 3)
            credits = [q2(total / k)] * (k - 1)
            credits.append(total - sum(credits, Decimal(0)))
            lines = []
            for side, amounts in (("Debit", debits), ("Credit", credits)):
                for amount in amounts:
                    lines.append({"Amount": num(amount), "DetailType": "JournalEntryLineDetail",
                                  "JournalEntryLineDetail": {"PostingType": side, "AccountRef": {"value": "A0"},
                                                             "Entity": {"Type": "Customer", "EntityRef": {"value": customers[0]["Id"]}}}})
            self.add("JournalEntry", {"Id": self.nid(), "TxnDate": "2026-03-31", "TotalAmt": num(total), "Line": lines, "PrivateNote": f"Reclass {total} per Zephyrine"})


def write_book(root: pathlib.Path, entities: dict, realm: str = REAL_REALM) -> None:
    by_type = {}
    for (t, i), raw in sorted(entities.items()):
        by_type.setdefault(t, []).append({
            "entity_type": t, "qbo_id": i, "sync_token": "0",
            "last_updated_utc": "2026-05-01T00:00:00+00:00", "is_deleted": False, "raw_json": raw,
        })
    manifest = []
    for t, payloads in sorted(by_type.items()):
        rel = f"{t}/query-none-none-0.json"
        path = root / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(json.dumps(payloads))
        manifest.append({"call": "query", "entity_type": t, "response_file": rel,
                         "params": {"realm": realm, "entity_type": t, "start_position": 0, "max_results": 1000, "updated": None}})
    (root / "manifest.json").write_text(json.dumps(manifest))


def read_book(root: pathlib.Path) -> dict:
    entities = {}
    for path in sorted(root.rglob("query-*.json")):
        for payload in json.loads(path.read_text()):
            entities[(payload["entity_type"], payload["qbo_id"])] = payload["raw_json"]
    return entities


def money_fields(node, path="$"):
    if isinstance(node, dict):
        for k, v in node.items():
            if k in sf.MONEY_KEYS and sf.dec(v) is not None:
                yield f"{path}.{k}", sf.dec(v)
            else:
                yield from money_fields(v, f"{path}.{k}")
    elif isinstance(node, list):
        for i, v in enumerate(node):
            yield from money_fields(v, f"{path}[{i}]")


def strings(node):
    if isinstance(node, dict):
        for v in node.values():
            yield from strings(v)
    elif isinstance(node, list):
        for v in node:
            yield from strings(v)
    elif isinstance(node, str):
        yield node


class ScrubbedBookTests(unittest.TestCase):
    """For every generated book and every seed: invariants hold, every amount
    moved, and no original survives anywhere."""

    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.cases = []
        root = pathlib.Path(cls.tmp.name)
        for book in BOOKS:
            builder = BookBuilder(book)
            entities = builder.build()
            src = root / f"book{book}" / "raw"
            write_book(src, entities)
            for seed in SEEDS:
                dst = root / f"book{book}" / seed
                sf.run_scrub(src, dst, seed=seed)
                cls.cases.append((book, seed, builder, entities, read_book(dst), dst))

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def test_the_generated_books_are_consistent_to_begin_with(self):
        for book in BOOKS:
            entities = BookBuilder(book).build()
            self.assertEqual(sf.check_invariants(entities), set(), f"book {book}")

    def test_every_invariant_holds_after_scrubbing(self):
        for book, seed, _, _, scrubbed, _ in self.cases:
            self.assertEqual(sf.check_invariants(scrubbed), set(), f"book {book} seed {seed}")

    def test_every_amount_of_a_dollar_or_more_moved(self):
        for book, seed, _, original, scrubbed, _ in self.cases:
            self.assertEqual(set(original), set(scrubbed))
            for key in original:
                new = dict(money_fields(scrubbed[key]))
                for path, old in money_fields(original[key]):
                    if abs(old) >= 1:
                        self.assertNotEqual(new[path], old, f"book {book} seed {seed} {key} {path}")

    def test_amounts_keep_two_decimals(self):
        for _, _, _, _, scrubbed, _ in self.cases:
            for raw in scrubbed.values():
                for path, value in money_fields(raw):
                    if not path.endswith(("UnitPrice", "PurchaseCost")):
                        self.assertEqual(value, q2(value), path)

    def test_paid_state_is_preserved(self):
        for _, _, _, original, scrubbed, _ in self.cases:
            for key, raw in original.items():
                if key[0] not in ("Invoice", "Bill") or "Balance" not in raw:
                    continue
                old_b, old_t = Decimal(repr(raw["Balance"])), Decimal(repr(raw["TotalAmt"]))
                new_b = Decimal(repr(scrubbed[key]["Balance"]))
                new_t = Decimal(repr(scrubbed[key]["TotalAmt"]))
                if old_b == 0:
                    self.assertEqual(new_b, 0, key)
                elif old_b == old_t:
                    self.assertEqual(new_b, new_t, key)
                else:
                    self.assertTrue(0 < new_b < new_t, key)

    def test_no_original_survives_in_the_output_text(self):
        """Independent of the scrubber's own detector: search the files."""
        for book, seed, builder, original, _, dst in self.cases:
            text = "\n".join(p.read_text() for p in sorted(dst.rglob("*.json")))
            values = [s for p in dst.rglob("*.json") for s in strings(json.loads(p.read_text()))]
            for name in builder.names:
                if len(name) > 3:
                    self.assertNotIn(name, text, f"book {book} seed {seed}")
                else:
                    for value in values:
                        self.assertNotRegex(value, rf"(?<!\w){name}(?!\w)")
            for needle in builder.emails + builder.phones + builder.streets + [REAL_REALM]:
                self.assertNotIn(needle, text, f"book {book} seed {seed}")
            amounts = {str(abs(v)) for raw in original.values() for _, v in money_fields(raw) if abs(v) >= 10}
            for value in values:
                for match in sf.AMOUNT_TEXT_RE.findall(value):
                    self.assertNotIn(match.replace(",", ""), amounts, f"book {book} seed {seed}: {value!r}")

    def test_same_seed_is_byte_identical_and_another_seed_differs(self):
        root = pathlib.Path(self.tmp.name)
        src = root / "book0" / "raw"
        again = root / "book0-again"
        sf.run_scrub(src, again, seed=SEEDS[0])
        first = root / "book0" / SEEDS[0]
        for path in sorted(first.rglob("*.json")):
            self.assertEqual(path.read_text(), (again / path.relative_to(first)).read_text())
        other = root / "book0" / SEEDS[1]
        self.assertNotEqual(
            (first / "Invoice" / "query-none-none-0.json").read_text(),
            (other / "Invoice" / "query-none-none-0.json").read_text(),
        )


class LoudFailureTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = pathlib.Path(self.tmp.name)
        self.src = self.root / "raw"
        write_book(self.src, BookBuilder(3).build())
        self.dst = self.root / "out"

    def patched(self, owner, name, replacement):
        original = getattr(owner, name)
        setattr(owner, name, replacement)
        self.addCleanup(setattr, owner, name, original)

    def test_amounts_left_unscrubbed_fail_the_run(self):
        self.patched(sf, "scrub_amounts", lambda loaded, secret: loaded)
        with self.assertRaises(sf.ScrubError) as ctx:
            sf.run_scrub(self.src, self.dst, seed="s")
        self.assertIn("amount (unchanged)", str(ctx.exception))
        self.assertFalse(self.dst.exists())

    def test_a_broken_invariant_fails_the_run(self):
        # Without the application step, balances fall through to plain
        # scaling and stop matching what was paid.
        self.patched(sf.AmountScrubber, "_balances", lambda self, apps: None)
        with self.assertRaises(sf.ScrubError) as ctx:
            sf.run_scrub(self.src, self.dst, seed="s")
        self.assertIn("broke", str(ctx.exception))
        self.assertFalse(self.dst.exists())

    def test_an_email_left_in_place_fails_the_run(self):
        self.patched(sf.Scrubber, "carries_contact_or_amount", lambda self, text: False)
        with self.assertRaises(sf.ScrubError) as ctx:
            sf.run_scrub(self.src, self.dst, seed="s")
        self.assertRegex(str(ctx.exception), r"\[(email|phone|amount \(text\))\]")
        self.assertFalse(self.dst.exists())

    def test_reports_are_refused_when_amounts_are_scrubbed(self):
        report = self.src / "reports" / "TrialBalance-none-2026-09-10-Accrual-none-none.json"
        report.parent.mkdir(parents=True)
        report.write_text(json.dumps({"Rows": {"Row": [{"ColData": [{"value": "Checking"}, {"value": "1234.56"}]}]}}))
        with self.assertRaises(sf.ScrubError) as ctx:
            sf.run_scrub(self.src, self.dst, seed="s")
        self.assertIn("report", str(ctx.exception))

    def test_a_missing_seed_is_refused(self):
        with self.assertRaises(sf.ScrubError):
            sf.run_scrub(self.src, self.dst)


class NameMatchingTests(unittest.TestCase):
    def test_short_names_match_whole_words_only(self):
        matcher = sf.NameMatcher(["US", "Al", "Bluewater Pools"])
        self.assertFalse(matcher.contains("USD"))
        self.assertFalse(matcher.contains("Also a pool"))
        self.assertTrue(matcher.contains("Call Al today"))
        self.assertTrue(matcher.contains("Al's order"))
        self.assertTrue(matcher.contains("re: Bluewater Pools, Inc."))
        self.assertTrue(matcher.contains("xBluewater Poolsx"))

    def test_digit_only_names_are_not_matched_in_text(self):
        self.assertFalse(sf.NameMatcher(["1001"]).contains("Invoice 1001"))

    def test_a_fake_never_equals_an_original(self):
        reserved = frozenset(f"{a} {b}" for a in sf.FIRST_WORDS for b in sf.LAST_WORDS[:5])
        mapper = sf.NameMapper(reserved=reserved)
        for n in range(50):
            self.assertNotIn(mapper.fake_for(f"Customer {n}"), reserved)

    def test_a_currency_code_does_not_trip_a_two_letter_customer(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            entities = {
                ("Customer", "1"): {"Id": "1", "DisplayName": "US", "Balance": 0},
                ("Invoice", "2"): {"Id": "2", "CustomerRef": {"value": "1", "name": "US"},
                                   "CurrencyRef": {"value": "USD", "name": "United States Dollar"},
                                   "Line": [{"Amount": 40.0, "DetailType": "SalesItemLineDetail", "SalesItemLineDetail": {}}],
                                   "TotalAmt": 40.0, "Balance": 40.0},
            }
            entities[("Customer", "1")]["Balance"] = 40.0
            write_book(root / "raw", entities)
            sf.run_scrub(root / "raw", root / "out", seed="s")
            out = read_book(root / "out")
            self.assertEqual(out[("Invoice", "2")]["CurrencyRef"]["value"], "USD")
            self.assertNotEqual(out[("Invoice", "2")]["CustomerRef"]["name"], "US")

    def test_deny_list_names_are_scrubbed_from_text(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            entities = {("Item", "1"): {"Id": "1", "Name": "Tube", "Description": "Same as Kestrelmoor Lagoon ordered"}}
            write_book(root / "raw", entities)
            (root / "raw" / sf.DENY_NAMES_FILE).write_text("Kestrelmoor Lagoon\n")
            sf.run_scrub(root / "raw", root / "out", seed="s")
            text = (root / "out" / "Item" / "query-none-none-0.json").read_text()
            self.assertNotIn("Kestrelmoor", text)


class FirstRealRunRegressionTests(unittest.TestCase):
    """The first run against the real replica (17 Oct 2026) failed its own
    leak check on 31 values. Each shape is rebuilt here from invented data."""

    def scrub(self, entities, deny=(), extra=None):
        tmp = tempfile.TemporaryDirectory()
        self.addCleanup(tmp.cleanup)
        root = pathlib.Path(tmp.name)
        write_book(root / "raw", entities)
        if deny:
            (root / "raw" / sf.DENY_NAMES_FILE).write_text("\n".join(deny) + "\n")
        if extra:
            extra(root / "raw")
        sf.run_scrub(root / "raw", root / "out", seed="regression-seed")
        return read_book(root / "out"), root / "out"

    def test_city_and_every_address_line_are_faked(self):
        vendor = {
            "Id": "7", "DisplayName": "Tidewrack Coatings", "Balance": 0,
            "BillAddr": {"Line1": "Tidewrack Coatings Attn Payables", "Line2": "12 Tidewrack Way",
                         "City": "Tidewrack", "CountrySubDivisionCode": "ME", "PostalCode": "04101"},
            "ShipAddr": {"Line1": "Tidewrack Coatings", "City": "Gullhaven"},
        }
        customer = {"Id": "8", "DisplayName": "Gullhaven", "Balance": 0}
        out, _ = self.scrub({("Vendor", "7"): vendor, ("Customer", "8"): customer})
        addr = out[("Vendor", "7")]["BillAddr"]
        for key in ("Line1", "Line2", "City"):
            self.assertNotIn("Tidewrack", addr[key], key)
        self.assertRegex(addr["Line1"], r"^\d+ \w+$", "a line that only contains a name is a fake street")
        self.assertIn(addr["City"], sf.CITY_WORDS)
        self.assertEqual(addr["CountrySubDivisionCode"], "ME")
        ship = out[("Vendor", "7")]["ShipAddr"]
        self.assertEqual(ship["Line1"], out[("Vendor", "7")]["DisplayName"], "an exact name line gets its fake")
        self.assertNotEqual(ship["City"], "Gullhaven")

    def test_purchase_description_naming_a_vendor_is_redacted(self):
        item = {"Id": "1", "Name": "Hull Primer", "PurchaseDesc": "Order from Tidewrack Coatings, 5 gal",
                "Description": "Two-part hull primer"}
        out, _ = self.scrub({("Item", "1"): item}, deny=["Tidewrack Coatings"])
        self.assertEqual(out[("Item", "1")]["PurchaseDesc"], sf.SCRUBBED_MARK)
        self.assertEqual(out[("Item", "1")]["Description"], "Two-part hull primer")

    def test_a_contact_named_like_an_entity_type_or_enum_is_not_a_leak(self):
        entities = {
            ("Customer", "1"): {"Id": "1", "DisplayName": "Bill", "Balance": 0},
            ("Bill", "2"): {"Id": "2", "TxnDate": "2026-01-01", "Line": [], "TotalAmt": 0, "Balance": 0},
            ("TaxCode", "3"): {"Id": "3", "Name": "Local", "TaxCodeConfigType": "Bill"},
            ("Item", "4"): {"Id": "4", "Name": "Billing adjustment", "Description": "Ask Bill before shipping"},
        }
        out, root = self.scrub(entities, deny=["Bill", "Deposit", "Payment"])
        manifest = json.loads((root / "manifest.json").read_text())
        self.assertIn("Bill", [entry["entity_type"] for entry in manifest])
        self.assertTrue((root / "Bill" / "query-none-none-0.json").exists())
        self.assertEqual(out[("TaxCode", "3")]["TaxCodeConfigType"], "Bill")
        self.assertEqual(out[("Item", "4")]["Name"], "Billing adjustment", "no match inside a word")
        self.assertEqual(out[("Item", "4")]["Description"], sf.SCRUBBED_MARK, "the person is still caught")

    def test_free_text_with_a_short_name_is_still_caught_by_the_backstop(self):
        detector = sf.LeakDetector(sf.Originals(names={"Bill", "Kestrelmoor Lagoon"}), sf.NameMapper(), True)
        self.assertEqual(detector.string_leak("call Bill on Monday", "Description"), "name")
        self.assertEqual(detector.string_leak("Bill", "DisplayName"), "name")
        self.assertIsNone(detector.string_leak("Bill", "entity_type"))
        self.assertIsNone(detector.string_leak("BillPayment", "Note"))
        self.assertEqual(detector.string_leak("re:Kestrelmoor LagoonLLC", "Note"), "name", "long names: substring")
        self.assertEqual(detector.string_leak("Bill", "TaxCodeConfigType"), None)
        self.assertEqual(detector.string_leak("Bill Smith", "TaxCodeConfigType"), "name", "only one-token enums")

    def test_letters_next_to_a_fake_are_not_a_false_word_boundary(self):
        mapper = sf.NameMapper()
        fake = mapper.fake_for("Kestrelmoor Lagoon Supply")
        detector = sf.LeakDetector(sf.Originals(names={"a", "ix"}), mapper, True)
        self.assertIsNone(detector.string_leak(fake + "a", "Note"))
        self.assertIsNone(detector.string_leak("ix" + fake, "Note"))
        self.assertEqual(detector.string_leak(fake + " a", "Note"), "name")


if __name__ == "__main__":
    unittest.main()
