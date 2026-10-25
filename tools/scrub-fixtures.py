#!/usr/bin/env python3
"""Scrub a recorded QBO fixture directory into a directory safe to commit.

Reads   <recorded_dir>/**/*.json   (RecordingQbo's output — see fixture.rs)
Writes  <scrubbed_dir>/**/*.json   — NEVER in place, and only if the run is clean

    # a real recording (normally a slice — `qbo-local slice`, HANDOFF.md §2.6):
    python3 tools/scrub-fixtures.py --seed-file .local/fixtures/scrub-seed \\
        .local/fixtures/slice-raw .local/fixtures/slice-scrubbed

    # invented data only (the committed synthetic set), amounts kept:
    python3 tools/scrub-fixtures.py --keep-amounts <recorded_dir> <scrubbed_dir>

HANDOFF.md §2.6 is the rule this script exists to satisfy: **a fixture that
reaches `git add` must not contain a real customer, vendor, balance or realm
id.** Scrub on the way in, not on the way out — this repository has already
had to clean a real name out of a committed file once (see
`prototype/README.md`, "The data in this prototype is fictional"), and finding
one after the fact is much more work than never writing it.

Deterministic: the same recorded input and the same seed always produce the
same scrubbed output byte-for-byte, so a re-record diffs cleanly against what
is already committed. Every substitution is keyed off a hash of the ORIGINAL
value (keyed with the seed when one is given), never off anything positional,
and both the file tree and every JSON object are walked in a fixed order.

The seed is a secret. Amount factors and fake names are HMAC-keyed with it, so
someone holding the scrubbed fixture and a guess ("is Acme Pools a customer?",
"the public price of a rescue tube is 24.99") cannot check the guess without
it. Keep it out of git — `.local/` is the place.

Name and contact rules:
  - DisplayName / CompanyName / GivenName / MiddleName / FamilyName /
    FullyQualifiedName / PrintOnCheckName / Name, read off a Customer, Vendor
    or Employee record, plus every line of `<recorded_dir>/deny-names.txt` if
    present (the slicer writes the whole book's names there) -> a fake name
    from a fixed word list. The same original always maps to the same fake,
    no fake ever equals an original, and the mapping is applied wherever the
    exact original recurs (CustomerRef.name, an address Line1, ...) and inside
    any other string that contains it (an account named after a lender).
  - Names of three characters or fewer match only as whole words (D26: the
    806 short real names fired on `USD` as substrings); longer names match as
    substrings. A name made only of digits and punctuation is never matched
    inside text — it cannot identify anyone and would match every id.
  - Email fields (PrimaryEmailAddr, BillEmail, BillEmailCc, BillEmailBcc,
    Email) -> contact-<hash>@example.com; WebAddr -> https://example.com
  - Any FreeFormNumber (phone/fax) -> 555-01xx
  - AcctNum / TaxIdentifier / PrimaryTaxIdentifier / ResaleNum /
    BusinessNumber / Notes -> "[scrubbed]"
  - Address lines (BillAddr, ShipAddr, ShipFromAddr, PrimaryAddr, OtherAddr,
    CompanyAddr, LegalAddr, CustomerCommunicationAddr) that are not a name
    substitution get a fake street; City and the state code are kept;
    PostalCode -> 07000
  - DocNumber is kept but shifted by a constant offset
  - PrivateNote, CustomerMemo.value and a line's Description are kept UNLESS
    they contain a scrubbed name, in which case the whole field becomes
    "[scrubbed]"; ANY string that contains an original email, phone number,
    street line, an email/phone-shaped value, or an original amount of 10.00
    or more becomes "[scrubbed]"
  - realmId (any casing of the key) and any realm-shaped numeric path segment
    -> a fixed placeholder realm id
  - Id, MetaData timestamps, quantities and rates are never touched

Amount rules (DECISIONS.md D31 has the reasoning) — skipped only with
--keep-amounts, which is for invented data:
  - Every entity gets its own factor f in [0.40, 0.80] or [1.25, 2.50],
    HMAC(seed, "Type:Id"). Never near 1, so every amount of 1.00 or more moves
    by at least 20%; never shared, so one known real amount gives away one
    document's factor and nothing else.
  - Leaves (item lines, expense lines, unit prices, unapplied amounts, ...)
    are scaled: round_half_up(x * f, 2). UnitPrice keeps its own decimal
    places and the line Amount is recomputed as Qty x UnitPrice.
  - Everything derived is RECOMPUTED from the new leaves: subtotal lines,
    percent discounts, NetAmountTaxable, each tax line at its stored
    TaxPercent, TotalTax, TotalAmt, payment applications, Balance,
    RemainingCredit, payment and deposit totals, contact balances and
    BalanceWithJobs, an account's CurrentBalanceWithSubAccounts.
  - Residual-preserving: whatever the original figure differed from its
    formula by is scaled and carried over. A consistent original (residual 0)
    stays exactly consistent; an inconsistent one stays inconsistent by a
    scaled amount, rather than being silently "fixed" or left holding a real
    number.
  - Applications: a target's (invoice, credit memo, bill, vendor credit)
    applied total keeps its shape — fully paid stays fully paid, unpaid stays
    unpaid, partly paid stays strictly partly paid — and is spread over the
    individual applications by largest remainder, so the cents add up.
  - Journal entries: debits are scaled as a group and credits allocated to
    the same total by largest remainder, so every entry still balances.
  - Report files carry amounts no formula ties to the entities; with amount
    scrubbing on they are refused rather than passed through.
  - After scrubbing, every invariant check that passed on the original must
    still pass (`check_invariants`); otherwise nothing is written.

The leak detector is the backstop, not a substitute for the rules above:
after scrubbing, every string in the OUTPUT is checked for an original name,
email, phone number, street line or realm id, any email address not at
example.com, any phone-shaped number, and any original amount of 10.00 or
more written as text; and every money field holding 1.00 or more is checked
against its own original value. Any hit fails the whole run — nothing is
written to the destination — and names the file and JSON path, so a leak can
never pass silently.
"""

from __future__ import annotations

import argparse
import copy
import hashlib
import hmac
import json
import pathlib
import re
import sys
from dataclasses import dataclass, field
from decimal import ROUND_FLOOR, ROUND_HALF_UP, Decimal

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

NAME_FIELDS = (
    "DisplayName",
    "CompanyName",
    "GivenName",
    "MiddleName",
    "FamilyName",
    "FullyQualifiedName",
    "PrintOnCheckName",
    "Name",
)
NAME_ENTITY_TYPES = ("Customer", "Vendor", "Employee")
REF_NAME_KEY = "name"  # CustomerRef.name / VendorRef.name / EntityRef.name

ADDRESS_KEYS = (
    "BillAddr",
    "ShipAddr",
    "ShipFromAddr",
    "PrimaryAddr",
    "OtherAddr",
    "CompanyAddr",
    "LegalAddr",
    "CustomerCommunicationAddr",
)
ADDRESS_LINE_KEYS = ("Line1", "Line2", "Line3", "Line4", "Line5")
STATE_KEY = "CountrySubDivisionCode"
CITY_KEY = "City"
POSTAL_KEY = "PostalCode"
FIXED_POSTAL_CODE = "07000"

EMAIL_KEYS = ("PrimaryEmailAddr", "BillEmail", "BillEmailCc", "BillEmailBcc", "Email")
ALWAYS_REDACTED_KEYS = (
    "AcctNum",
    "TaxIdentifier",
    "PrimaryTaxIdentifier",
    "ResaleNum",
    "BusinessNumber",
    "Notes",
)

DOC_NUMBER_OFFSET = 100_000
FIXED_REALM_ID = "1234567890123456"
SCRUBBED_MARK = "[scrubbed]"
DENY_NAMES_FILE = "deny-names.txt"

# CustomerMemo.value handled separately. PurchaseDesc is an item's text on
# purchase documents and often names the vendor it is bought from.
TEXT_FIELD_KEYS = ("PrivateNote", "Description", "PurchaseDesc")

# Keys whose string values are identifiers or timestamps, never free text.
STRUCTURAL_KEYS = frozenset(
    (
        "Id",
        "qbo_id",
        "sync_token",
        "SyncToken",
        "last_updated_utc",
        "entity_type",
        "DocNumber",
        "TxnDate",
        "DueDate",
        "TxnId",
        "TxnType",
        "TxnLineId",
        "CreateTime",
        "LastUpdatedTime",
        "DetailType",
        "PostingType",
        "domain",
        "call",
        "response_file",
    )
)

# An enum-shaped value: one identifier token under a key that names a type
# ("TaxCodeConfigType": "…", "EntityRef.type": "Vendor"). It cannot carry a
# contact's name — a one-word contact named like the enum value is a
# coincidence, not a leak.
ENUM_VALUE_RE = re.compile(r"^[A-Za-z][A-Za-z0-9_]*$")


def is_structural(key: str, value: str) -> bool:
    """A string that is an identifier, timestamp, file name or enum, never
    free text: the name rules neither rewrite it nor flag it."""
    if key in STRUCTURAL_KEYS:
        return True
    return key.lower().endswith(("type", "status")) and bool(ENUM_VALUE_RE.match(value))


# A realm id is a long run of digits. QBO's are 16 digits; keep the floor
# comfortably below that so an id from a differently-sized company still gets
# caught in a path segment.
REALM_PATH_RE = re.compile(r"^\d{10,}$")
EMAIL_RE = re.compile(r"[A-Za-z0-9._%+'-]+@[A-Za-z0-9.-]+\.[A-Za-z]{2,}")
PHONE_RE = re.compile(r"(?<!\d)\(?\d{3}\)?[-. ]?\d{3}[-. ]\d{4}(?!\d)")
AMOUNT_TEXT_RE = re.compile(r"(?<![\d.,])(?:\d{1,3}(?:,\d{3})+|\d+)\.\d{2}(?![\d])")

# Deliberately plain, deliberately fictional. Two independent lists combine
# into FIRST_WORDS x LAST_WORDS candidates, then the same again with a numeric
# suffix for as many rounds as a whole book needs; open addressing (see
# NameMapper) guarantees no two different originals ever collide regardless.
FIRST_WORDS = [
    "Harbor", "Meadow", "Cobalt", "Marigold", "Amber", "Cedar", "Willow",
    "Granite", "Lantern", "Bramble", "Fernwood", "Ironwood", "Slate",
    "Hollow", "Driftwood", "Alder", "Birch", "Copper", "Thistle", "Hazel",
    "Juniper", "Maple", "Pebble", "Quartz",
]
LAST_WORDS = [
    "Point", "Crossing", "Harbor", "Ridge", "Cove", "Bend", "Grove",
    "Landing", "Springs", "Falls", "Meadows", "Junction", "Yard", "Works",
    "Supply", "Traders", "Marine", "Outfitters", "Studio", "Fabrication",
    "Depot", "Foundry", "Freight", "Holdings",
]
CITY_WORDS = [
    "Fairhaven", "Brookmere", "Eastvale", "Northgate", "Westbury", "Lakemont",
    "Riverton", "Ashford", "Kingsbridge", "Millbrook", "Oakdale", "Stonefield",
]
STREET_WORDS = [
    "Ave", "Blvd", "Way", "Lane", "Drive", "Court", "Place", "Trail",
    "Road", "Circle", "Terrace", "Path", "Row", "Crossing", "Alley", "Loop",
]

# Money. Rates, percentages and quantities are deliberately absent.
MONEY_KEYS = frozenset(
    (
        "Amount",
        "TotalAmt",
        "Balance",
        "HomeBalance",
        "HomeTotalAmt",
        "HomeAmount",
        "UnappliedAmt",
        "TotalTax",
        "NetAmountTaxable",
        "UnitPrice",
        "PurchaseCost",
        "CurrentBalance",
        "CurrentBalanceWithSubAccounts",
        "BalanceWithJobs",
        "RemainingCredit",
        "Deposit",
        "CreditLimit",
        "TaxInclusiveAmt",
        "OpenBalance",
    )
)
# Prices keep their own decimal places (QBO allows more than two); every
# other money field is two decimals (D9).
PRICE_KEYS = frozenset(("UnitPrice", "PurchaseCost"))

SALES_DOCS = ("Invoice", "SalesReceipt", "CreditMemo", "RefundReceipt", "Estimate")
PURCHASE_DOCS = ("Bill", "VendorCredit", "Purchase", "PurchaseOrder")
LINE_DOCS = SALES_DOCS + PURCHASE_DOCS
PAYERS = {"Payment": ("CreditMemo",), "BillPayment": ("VendorCredit",)}
# LinkedTxn.TxnType -> the entity type it names, where they differ.
TXN_TYPE_ALIASES = {
    "BillPaymentCheck": "BillPayment",
    "BillPaymentCreditCard": "BillPayment",
    "Check": "Purchase",
    "Expense": "Purchase",
    "CreditCardCredit": "Purchase",
}

CENT = Decimal("0.01")
ZERO = Decimal("0")
LEAK_AMOUNT_FIELD_FLOOR = Decimal("1.00")
LEAK_AMOUNT_TEXT_FLOOR = Decimal("10.00")


class ScrubError(Exception):
    """Raised for anything that must stop the run before it writes a file —
    a bad invocation, a broken invariant, or a leak the detector caught."""


# ---------------------------------------------------------------------------
# Deterministic, hash-keyed fakes
# ---------------------------------------------------------------------------


def _digest(value: str, key: str = "") -> bytes:
    if key:
        return hmac.new(key.encode("utf-8"), value.encode("utf-8"), hashlib.sha256).digest()
    return hashlib.sha256(value.encode("utf-8")).digest()


def _stable_int(value: str, key: str = "") -> int:
    return int.from_bytes(_digest(value, key)[:8], "big")


class NameMapper:
    """original name string -> fake name string.

    Open-addressed: if two different originals hash to the same candidate,
    the second probes forward deterministically until it finds a fake that is
    not already taken, so two different real names can never end up sharing
    one fake — that would merge two different customers into one in the
    scrubbed output, which is a worse bug than a slow collision search.
    A candidate equal to any `reserved` (original) name is never handed out,
    so a fake can never be mistaken for, or hide, a real name.
    """

    def __init__(self, key: str = "", reserved: frozenset = frozenset()) -> None:
        self.map: dict[str, str] = {}
        self.key = key
        self._taken: set[str] = set(reserved)
        # Fakes handed out per round; round 0 is the bare "First Last" names,
        # round r > 0 suffixes them " r+1". Lets a full round be skipped
        # rather than probed slot by slot.
        self._round_fill: list[int] = []
        self._reserved_per_round: dict[int, int] = {}

    def fake_for(self, original: str) -> str:
        if original in self.map:
            return self.map[original]
        span = len(FIRST_WORDS) * len(LAST_WORDS)
        index = _stable_int(original, self.key) % span
        # A whole real book has thousands of customers and vendors (M0, 14 Oct
        # 2026: ~11,400 against 576 bare names), so once every bare name is
        # taken the next round reuses the list with a numeric suffix. Still
        # deterministic for the same input, still never two originals on one
        # fake.
        round_no = 0
        while True:
            if round_no == len(self._round_fill):
                self._round_fill.append(0)
            suffix = "" if round_no == 0 else f" {round_no + 1}"
            candidate = None
            if self._round_fill[round_no] < span:
                probe = index
                for _ in range(span):
                    base = f"{FIRST_WORDS[probe // len(LAST_WORDS)]} {LAST_WORDS[probe % len(LAST_WORDS)]}"
                    if base + suffix not in self._taken:
                        candidate = base + suffix
                        break
                    probe = (probe + 1) % span
                if candidate is None:
                    # Every slot is taken (some by reserved originals).
                    self._round_fill[round_no] = span
            if candidate is not None:
                break
            round_no += 1
        self._round_fill[round_no] += 1
        self._taken.add(candidate)
        self.map[original] = candidate
        return candidate

    def originals(self) -> list[str]:
        return list(self.map.keys())

    def fakes(self) -> list[str]:
        return list(self.map.values())


def _textual(name: str) -> bool:
    """A name worth looking for inside free text: it has a letter in it."""
    return any(ch.isalpha() for ch in name)


class NameMatcher:
    """Finds original names inside text. Names of up to seven characters
    (stripped) match only on word boundaries — a contact named "Bill" must not
    match "BillPayment" or "Billing" (first real run, 17 Oct 2026); longer,
    distinctive names still match anywhere, so a real name glued to other
    text ("…Corp" without a space) is still caught. A boundary is the edge of
    the text or a non-alphanumeric character. Leftmost-longest,
    non-overlapping. Indexed by first character so a whole book's names
    (~19k) stay cheap against every string of a slice."""

    WHOLE_WORD_MAX = 7

    def __init__(self, names) -> None:
        self._by_first: dict[str, list[str]] = {}
        for name in set(names):
            if name and _textual(name):
                self._by_first.setdefault(name[0], []).append(name)
        for bucket in self._by_first.values():
            bucket.sort(key=lambda n: (-len(n), n))

    def __bool__(self) -> bool:
        return bool(self._by_first)

    @classmethod
    def _bounded(cls, text: str, start: int, name: str) -> bool:
        if len(name.strip()) > cls.WHOLE_WORD_MAX:
            return True
        end = start + len(name)
        before_ok = start == 0 or not text[start - 1].isalnum() or not name[0].isalnum()
        after_ok = end == len(text) or not text[end].isalnum() or not name[-1].isalnum()
        return before_ok and after_ok

    def find(self, text: str) -> list[tuple[int, int, str]]:
        hits = []
        i = 0
        while i < len(text):
            bucket = self._by_first.get(text[i])
            hit = None
            if bucket:
                for name in bucket:
                    if text.startswith(name, i) and self._bounded(text, i, name):
                        hit = name
                        break
            if hit:
                hits.append((i, i + len(hit), hit))
                i += len(hit)
            else:
                i += 1
        return hits

    def contains(self, text: str) -> bool:
        return bool(self.find(text))


def fake_street(original: str, key: str = "") -> str:
    digest = _stable_int(original, key)
    number = 100 + (digest % 900)
    word = STREET_WORDS[digest % len(STREET_WORDS)]
    return f"{number} {word}"


def fake_city(original: str, key: str = "") -> str:
    return CITY_WORDS[_stable_int("city:" + original, key) % len(CITY_WORDS)]


def fake_email(original: str, key: str = "") -> str:
    digest = _digest(original, key).hex()[:10]
    return f"contact-{digest}@example.com"


def fake_phone(original: str, key: str = "") -> str:
    digest = _stable_int(original, key) % 100
    return f"555-01{digest:02d}"


def shift_doc_number(value):
    try:
        return str(int(value) + DOC_NUMBER_OFFSET)
    except (TypeError, ValueError):
        # Not purely numeric (e.g. "EST-1001") — DocNumber is kept as-is
        # rather than guessing at a format-preserving shift.
        return value


# ---------------------------------------------------------------------------
# Pass 1 — collect every original worth scrubbing
# ---------------------------------------------------------------------------


@dataclass
class Originals:
    names: set = field(default_factory=set)
    emails: set = field(default_factory=set)
    phones: set = field(default_factory=set)
    streets: set = field(default_factory=set)
    realms: set = field(default_factory=set)
    # Decimal amounts as normalized text, e.g. "1234.56" — for text matching.
    amount_texts: set = field(default_factory=set)


def _is_number(value) -> bool:
    return isinstance(value, (int, float)) and not isinstance(value, bool)


def dec(value):
    """A JSON number as an exact Decimal (via its shortest repr), else None."""
    if _is_number(value):
        return Decimal(repr(value)) if isinstance(value, float) else Decimal(value)
    return None


def collect_originals(node, found: Originals, nested: bool = False) -> None:
    if isinstance(node, dict):
        entity_type = node.get("entity_type")
        is_named_entity = nested or entity_type in NAME_ENTITY_TYPES
        for key, value in node.items():
            if is_named_entity and key in NAME_FIELDS and isinstance(value, str) and value:
                found.names.add(value)
            if key in EMAIL_KEYS and isinstance(value, dict):
                address = value.get("Address")
                if isinstance(address, str) and address:
                    found.emails.add(address)
            if key == "FreeFormNumber" and isinstance(value, str) and value:
                found.phones.add(value)
            if key in ADDRESS_KEYS and isinstance(value, dict):
                for line_key in ADDRESS_LINE_KEYS:
                    line = value.get(line_key)
                    if isinstance(line, str) and len(line.strip()) >= 6:
                        found.streets.add(line)
            if isinstance(value, str) and (
                key.lower() == "realmid" or (key == "realm" and REALM_PATH_RE.match(value))
            ):
                found.realms.add(value)
            if key in MONEY_KEYS:
                amount = dec(value)
                if amount is not None and abs(amount) >= LEAK_AMOUNT_TEXT_FLOOR:
                    found.amount_texts.add(_amount_text(amount))
            collect_originals(value, found, is_named_entity)
    elif isinstance(node, list):
        for item in node:
            collect_originals(item, found, nested)


def _amount_text(amount: Decimal) -> str:
    return str(abs(amount).quantize(CENT, rounding=ROUND_HALF_UP))


def collect_names_into(node, mapper: NameMapper, nested: bool = False) -> None:
    """Assign fakes in traversal order (stable for a given input)."""
    if isinstance(node, dict):
        entity_type = node.get("entity_type")
        is_named_entity = nested or entity_type in NAME_ENTITY_TYPES
        for key, value in node.items():
            if is_named_entity and key in NAME_FIELDS and isinstance(value, str) and value:
                mapper.fake_for(value)
            collect_names_into(value, mapper, is_named_entity)
    elif isinstance(node, list):
        for item in node:
            collect_names_into(item, mapper, nested)


# ---------------------------------------------------------------------------
# Amounts
# ---------------------------------------------------------------------------


def q2(value: Decimal) -> Decimal:
    return value.quantize(CENT, rounding=ROUND_HALF_UP)


def amount_factor(secret: str, kind: str, ident: str) -> Decimal:
    """f in [0.40, 0.80] or [1.25, 2.50], in millionths — integer arithmetic
    only, so the same seed gives the same factor on every platform."""
    digest = _digest(f"{kind}:{ident}", secret or "amounts")
    n = int.from_bytes(digest[:8], "big")
    if digest[8] & 1:
        low, high = 400_000, 800_000
    else:
        low, high = 1_250_000, 2_500_000
    return Decimal(low + n % (high - low + 1)) / Decimal(1_000_000)


def to_json_number(amount: Decimal, like):
    if isinstance(like, int) and not isinstance(like, bool) and amount == amount.to_integral_value():
        return int(amount)
    return float(amount)


def allocate(parts_old: list, total_new: Decimal) -> list:
    """Spread `total_new` over parts in proportion to `parts_old`, to the
    cent, by largest remainder: the results always sum to exactly
    `total_new`."""
    if not parts_old:
        return []
    total_old = sum(parts_old, ZERO)
    if total_old == 0:
        shares = [Decimal(1) / len(parts_old)] * len(parts_old)
    else:
        shares = [p / total_old for p in parts_old]
    raw = [s * total_new * 100 for s in shares]
    floors = [r.to_integral_value(rounding=ROUND_FLOOR) for r in raw]
    remainder = int(total_new * 100) - int(sum(floors, ZERO))
    by_fraction = sorted(range(len(raw)), key=lambda i: (-(raw[i] - floors[i]), i))
    cents = list(floors)
    i = 0
    while remainder > 0:
        cents[by_fraction[i % len(cents)]] += 1
        remainder -= 1
        i += 1
    i = 0
    while remainder < 0:
        cents[by_fraction[-1 - (i % len(cents))]] -= 1
        remainder += 1
        i += 1
    return [Decimal(c) / 100 for c in cents]


def _linked(line: dict):
    """The single (entity_type, id) a line's LinkedTxn names, or None."""
    linked = line.get("LinkedTxn")
    if isinstance(linked, list) and len(linked) == 1 and isinstance(linked[0], dict):
        txn_type = linked[0].get("TxnType")
        txn_id = linked[0].get("TxnId")
        if isinstance(txn_type, str) and isinstance(txn_id, str):
            return (TXN_TYPE_ALIASES.get(txn_type, txn_type), txn_id)
    return None


def _ref(raw: dict, key: str):
    ref = raw.get(key)
    if isinstance(ref, dict) and isinstance(ref.get("value"), str):
        return ref["value"]
    return None


def _detail(line: dict) -> dict:
    detail = line.get(line.get("DetailType") or "")
    return detail if isinstance(detail, dict) else {}


def _taxable(line: dict) -> bool:
    code = _ref(_detail(line), "TaxCodeRef")
    return code is not None and code != "NON"


def _is_leaf(line: dict) -> bool:
    return line.get("DetailType") not in ("SubTotalLineDetail", "DiscountLineDetail")


def _price_places(price: Decimal) -> int:
    return max(2, -price.as_tuple().exponent)


class AmountScrubber:
    """Computes scrubbed copies of every entity's raw_json. See the module
    docstring for the rules; `check_invariants` below is the definition of
    "consistent" this preserves."""

    def __init__(self, entities: dict, secret: str) -> None:
        self.old = entities
        self.new = {k: copy.deepcopy(v) for k, v in entities.items()}
        self.secret = secret
        self.handled: set = set()

    # -- primitives ---------------------------------------------------------

    def f(self, key) -> Decimal:
        return amount_factor(self.secret, key[0], key[1])

    def _set(self, container: dict, name: str, amount: Decimal) -> None:
        container[name] = to_json_number(amount, container.get(name))
        self.handled.add((id(container), name))

    def leaf(self, container: dict, name: str, f: Decimal):
        """Scale a free amount; nudge by a cent if scaling left it equal."""
        old = dec(container.get(name))
        if old is None:
            return None
        # `container` is the copy, still holding the original until set here.
        assert (id(container), name) not in self.handled, name
        new = q2(old * f)
        if new == old and old != 0:
            new += CENT if old > 0 else -CENT
        self._set(container, name, new)
        return old, new

    def derived(self, container: dict, name: str, new_formula, old_formula, f: Decimal):
        old = dec(container.get(name))
        if old is None:
            return None
        new = new_formula + q2((old - old_formula) * f)
        self._set(container, name, new)
        return old, new

    # -- documents with lines -------------------------------------------------

    def _line_leaf(self, old_line: dict, line: dict, f: Decimal):
        amount = dec(old_line.get("Amount"))
        old_detail = _detail(old_line)
        detail = _detail(line)
        qty = dec(old_detail.get("Qty"))
        price = dec(old_detail.get("UnitPrice"))
        if amount is not None and qty is not None and price is not None and qty != 0:
            places = Decimal(1).scaleb(-_price_places(price))
            new_price = (price * f).quantize(places, rounding=ROUND_HALF_UP)
            residual = q2((amount - q2(qty * price)) * f)
            new_amount = q2(qty * new_price) + residual
            for _ in range(5):
                if not (amount != 0 and new_amount == amount):
                    break
                new_price += places if price >= 0 else -places
                new_amount = q2(qty * new_price) + residual
            self._set(detail, "UnitPrice", new_price)
            self._set(line, "Amount", new_amount)
            return amount, new_amount
        result = self.leaf(line, "Amount", f)
        return result if result else (ZERO, ZERO)

    def _lines(self, key, f: Decimal) -> dict:
        old_raw, raw = self.old[key], self.new[key]
        old_lines = old_raw.get("Line") if isinstance(old_raw.get("Line"), list) else []
        lines = raw.get("Line") if isinstance(raw.get("Line"), list) else []
        sums = {"leaf": [ZERO, ZERO], "taxable": [ZERO, ZERO], "discount": [ZERO, ZERO]}
        subtotals, discounts = [], []
        for old_line, line in zip(old_lines, lines):
            if not isinstance(line, dict):
                continue
            kind = line.get("DetailType")
            if kind == "SubTotalLineDetail":
                subtotals.append(line)
                continue
            if kind == "DiscountLineDetail":
                discounts.append((old_line, line))
                continue
            if kind == "GroupLineDetail":
                o, n = self._group(old_line, line, f)
            else:
                o, n = self._line_leaf(old_line, line, f)
            sums["leaf"][0] += o
            sums["leaf"][1] += n
            if _taxable(old_line):
                sums["taxable"][0] += o
                sums["taxable"][1] += n
        for line in subtotals:
            self.derived(line, "Amount", sums["leaf"][1], sums["leaf"][0], f)
        for old_line, line in discounts:
            detail = _detail(old_line)
            pct = dec(detail.get("DiscountPercent"))
            if detail.get("PercentBased") and pct is not None:
                pair = self.derived(
                    line,
                    "Amount",
                    q2(sums["leaf"][1] * pct / 100),
                    q2(sums["leaf"][0] * pct / 100),
                    f,
                )
            else:
                pair = self.leaf(line, "Amount", f)
            if pair:
                sums["discount"][0] += pair[0]
                sums["discount"][1] += pair[1]
        return sums

    def _group(self, old_line: dict, line: dict, f: Decimal):
        old_detail, detail = _detail(old_line), _detail(line)
        old_subs = old_detail.get("Line") if isinstance(old_detail.get("Line"), list) else []
        subs = detail.get("Line") if isinstance(detail.get("Line"), list) else []
        total_old = total_new = ZERO
        for old_sub, sub in zip(old_subs, subs):
            if isinstance(sub, dict):
                o, n = self._line_leaf(old_sub, sub, f)
                total_old += o
                total_new += n
        pair = self.derived(line, "Amount", total_new, total_old, f)
        return pair if pair else (total_old, total_new)

    def _tax(self, key, f: Decimal, taxable) -> tuple:
        old_detail = self.old[key].get("TxnTaxDetail")
        detail = self.new[key].get("TxnTaxDetail")
        if not isinstance(detail, dict):
            return ZERO, ZERO
        sum_old = sum_new = ZERO
        old_lines = old_detail.get("TaxLine") if isinstance(old_detail.get("TaxLine"), list) else []
        lines = detail.get("TaxLine") if isinstance(detail.get("TaxLine"), list) else []
        for old_line, line in zip(old_lines, lines):
            if not isinstance(line, dict):
                continue
            old_tld, tld = _detail(old_line), _detail(line)
            nat = self.derived(tld, "NetAmountTaxable", taxable[1], taxable[0], f)
            pct = dec(old_tld.get("TaxPercent"))
            if nat and pct is not None:
                pair = self.derived(
                    line, "Amount", q2(nat[1] * pct / 100), q2(nat[0] * pct / 100), f
                )
            else:
                pair = self.leaf(line, "Amount", f)
            if pair:
                sum_old += pair[0]
                sum_new += pair[1]
        pair = self.derived(detail, "TotalTax", sum_new, sum_old, f)
        return pair if pair else (sum_old, sum_new)

    def _line_doc(self, key) -> None:
        f = self.f(key)
        raw = self.new[key]
        sums = self._lines(key, f)
        tax = self._tax(key, f, sums["taxable"])
        if raw.get("GlobalTaxCalculation") == "TaxInclusive":
            tax = (ZERO, ZERO)
        self.derived(
            raw,
            "TotalAmt",
            sums["leaf"][1] - sums["discount"][1] + tax[1],
            sums["leaf"][0] - sums["discount"][0] + tax[0],
            f,
        )
        self.leaf(raw, "Deposit", f)

    def _journal(self, key) -> None:
        f = self.f(key)
        old_lines = self.old[key].get("Line") or []
        lines = self.new[key].get("Line") or []
        sides = {"Debit": ([], []), "Credit": ([], [])}
        for old_line, line in zip(old_lines, lines):
            if not isinstance(line, dict):
                continue
            side = _detail(old_line).get("PostingType")
            amount = dec(old_line.get("Amount"))
            if side in sides and amount is not None:
                sides[side][0].append(amount)
                sides[side][1].append(line)
            else:
                self.leaf(line, "Amount", f)
        debit_old = sum(sides["Debit"][0], ZERO)
        credit_old = sum(sides["Credit"][0], ZERO)
        debit_new = q2(debit_old * f)
        if debit_new == debit_old and debit_old != 0:
            debit_new += CENT
        credit_new = debit_new + q2((credit_old - debit_old) * f)
        for side, total in (("Debit", debit_new), ("Credit", credit_new)):
            olds, targets = sides[side]
            for line, amount in zip(targets, allocate(olds, total)):
                self._set(line, "Amount", amount)
        self.derived(self.new[key], "TotalAmt", debit_new, debit_old, f)

    # -- applications and balances -------------------------------------------

    def _applications(self) -> dict:
        """target key -> [(old amount, payer line dict)] from every in-set
        Payment / BillPayment line that names exactly one in-set target."""
        apps: dict = {}
        for key in sorted(self.old):
            if key[0] not in PAYERS:
                continue
            for old_line, line in zip(self.old[key].get("Line") or [], self.new[key].get("Line") or []):
                if not isinstance(line, dict):
                    continue
                target = _linked(old_line)
                amount = dec(old_line.get("Amount"))
                if target in self.old and amount is not None and "TotalAmt" in self.old[target]:
                    apps.setdefault(target, []).append((amount, line))
        return apps

    def _balances(self, apps: dict) -> None:
        for key in sorted(self.old):
            if key[0] not in LINE_DOCS:
                continue
            old_raw, raw = self.old[key], self.new[key]
            f = self.f(key)
            total_old = dec(old_raw.get("TotalAmt")) or ZERO
            total_new = dec(raw.get("TotalAmt")) or ZERO
            base_old = total_old - (dec(old_raw.get("Deposit")) or ZERO)
            base_new = total_new - (dec(raw.get("Deposit")) or ZERO)
            mine = apps.get(key, [])
            applied_old = sum((a for a, _ in mine), ZERO)
            if applied_old == base_old:
                applied_new = base_new
            elif applied_old == 0:
                applied_new = ZERO
            elif base_old != 0:
                applied_new = q2(applied_old * base_new / base_old)
                if 0 < applied_old < base_old and base_new >= Decimal("0.02"):
                    applied_new = min(max(applied_new, CENT), base_new - CENT)
            else:
                applied_new = q2(applied_old * f)
            for (_, line), amount in zip(mine, allocate([a for a, _ in mine], applied_new)):
                self._set(line, "Amount", amount)
            balance = self.derived(raw, "Balance", base_new - applied_new, base_old - applied_old, f)
            if balance:
                self.derived(raw, "RemainingCredit", balance[1], balance[0], f)
            else:
                self.derived(raw, "RemainingCredit", base_new - applied_new, base_old - applied_old, f)

    def _payer(self, key) -> None:
        f = self.f(key)
        negative = PAYERS[key[0]]
        sum_old = sum_new = ZERO
        for old_line, line in zip(self.old[key].get("Line") or [], self.new[key].get("Line") or []):
            if not isinstance(line, dict):
                continue
            if (id(line), "Amount") in self.handled:  # an application, set above
                pair = (dec(old_line.get("Amount")), dec(line.get("Amount")))
            else:
                pair = self.leaf(line, "Amount", f)
            if not pair:
                continue
            target = _linked(old_line)
            sign = -1 if target and target[0] in negative else 1
            sum_old += sign * pair[0]
            sum_new += sign * pair[1]
        raw = self.new[key]
        unapplied = self.leaf(raw, "UnappliedAmt", f) or (ZERO, ZERO)
        self.derived(raw, "TotalAmt", sum_new + unapplied[1], sum_old + unapplied[0], f)

    def _deposit(self, key) -> None:
        f = self.f(key)
        sum_old = sum_new = ZERO
        for old_line, line in zip(self.old[key].get("Line") or [], self.new[key].get("Line") or []):
            if not isinstance(line, dict):
                continue
            target = _linked(old_line)
            if target in self.old and "TotalAmt" in self.old[target]:
                pair = self.derived(
                    line,
                    "Amount",
                    dec(self.new[target]["TotalAmt"]),
                    dec(self.old[target]["TotalAmt"]),
                    f,
                )
            else:
                pair = self.leaf(line, "Amount", f)
            if pair:
                sum_old += pair[0]
                sum_new += pair[1]
        raw = self.new[key]
        cash_back = raw.get("CashBack")
        back = (ZERO, ZERO)
        if isinstance(cash_back, dict):
            back = self.leaf(cash_back, "Amount", f) or back
        self.derived(raw, "TotalAmt", sum_new - back[1], sum_old - back[0], f)

    def _contacts(self) -> None:
        open_items: dict = {}

        def add(contact, field_old, field_new, sign):
            if field_old is None or contact is None:
                return
            pair = open_items.setdefault(contact, [ZERO, ZERO])
            pair[0] += sign * field_old
            pair[1] += sign * field_new

        rules = (
            ("Invoice", "CustomerRef", "Balance", 1),
            ("CreditMemo", "CustomerRef", "Balance", -1),
            ("Payment", "CustomerRef", "UnappliedAmt", -1),
            ("Bill", "VendorRef", "Balance", 1),
            ("VendorCredit", "VendorRef", "Balance", -1),
        )
        for key in sorted(self.old):
            for doc_type, ref_key, name, sign in rules:
                if key[0] == doc_type:
                    contact_type = "Customer" if ref_key == "CustomerRef" else "Vendor"
                    contact = _ref(self.old[key], ref_key)
                    add(
                        (contact_type, contact) if contact else None,
                        dec(self.old[key].get(name)),
                        dec(self.new[key].get(name)),
                        sign,
                    )
        for key in sorted(self.old):
            if key[0] in ("Customer", "Vendor"):
                items = open_items.get(key, [ZERO, ZERO])
                self.derived(self.new[key], "Balance", items[1], items[0], self.f(key))
        self._rollup("Customer", "Balance", "BalanceWithJobs")
        self._rollup("Account", "CurrentBalance", "CurrentBalanceWithSubAccounts")

    def _rollup(self, entity_type: str, own: str, total: str) -> None:
        """total = own + the children's totals, children first."""
        children: dict = {}
        for key in sorted(self.old):
            if key[0] == entity_type:
                parent = _ref(self.old[key], "ParentRef")
                if parent:
                    children.setdefault((entity_type, parent), []).append(key)
        done: dict = {}

        def visit(key, depth=0):
            if key in done or depth > 64:
                return done.get(key, (ZERO, ZERO))
            f = self.f(key)
            own_pair = self.leaf(self.new[key], own, f) if (id(self.new[key]), own) not in self.handled else (
                dec(self.old[key].get(own)),
                dec(self.new[key].get(own)),
            )
            own_pair = own_pair if own_pair and own_pair[0] is not None else (ZERO, ZERO)
            kids_old = kids_new = ZERO
            for child in children.get(key, []):
                o, n = visit(child, depth + 1)
                kids_old += o
                kids_new += n
            pair = self.derived(self.new[key], total, own_pair[1] + kids_new, own_pair[0] + kids_old, f)
            done[key] = pair if pair else (own_pair[0] + kids_old, own_pair[1] + kids_new)
            return done[key]

        for key in sorted(self.old):
            if key[0] == entity_type:
                visit(key)

    # -- the rest ------------------------------------------------------------

    def _home(self, key) -> None:
        raw, old_raw = self.new[key], self.old[key]
        rate = dec(old_raw.get("ExchangeRate"))
        for home, base in (("HomeBalance", "Balance"), ("HomeTotalAmt", "TotalAmt")):
            if home in old_raw and rate is not None and dec(old_raw.get(base)) is not None:
                self.derived(
                    raw,
                    home,
                    q2(dec(raw[base]) * rate),
                    q2(dec(old_raw[base]) * rate),
                    self.f(key),
                )

    def _fallback(self, old, new, f: Decimal) -> None:
        if isinstance(old, dict) and isinstance(new, dict):
            for name in old:
                if name in MONEY_KEYS and (id(new), name) not in self.handled and dec(old[name]) is not None:
                    if name in PRICE_KEYS:
                        price = dec(old[name])
                        places = Decimal(1).scaleb(-_price_places(price))
                        scaled = (price * f).quantize(places, rounding=ROUND_HALF_UP)
                        if scaled == price and price != 0:
                            scaled += places
                        self._set(new, name, scaled)
                    else:
                        self.leaf(new, name, f)
                else:
                    self._fallback(old[name], new.get(name), f)
        elif isinstance(old, list) and isinstance(new, list):
            for o, n in zip(old, new):
                self._fallback(o, n, f)

    def run(self) -> dict:
        for key in sorted(self.old):
            if key[0] in LINE_DOCS:
                self._line_doc(key)
            elif key[0] == "JournalEntry":
                self._journal(key)
        self._balances(self._applications())
        for key in sorted(self.old):
            if key[0] in PAYERS:
                self._payer(key)
        for key in sorted(self.old):
            if key[0] == "Deposit":
                self._deposit(key)
        self._contacts()
        for key in sorted(self.old):
            self._home(key)
            self._fallback(self.old[key], self.new[key], self.f(key))
        return self.new


# ---------------------------------------------------------------------------
# Invariants — what "internally consistent" means, checked before and after
# ---------------------------------------------------------------------------


def check_invariants(entities: dict) -> set:
    """Every violated rule, as (entity_type, id, rule). A consistent book
    returns an empty set. The scrubber requires that scrubbing violates no
    rule the original satisfied."""
    bad: set = set()

    def require(key, rule, ok):
        if not ok:
            bad.add((key[0], key[1], rule))

    apps: dict = {}
    for key in sorted(entities):
        if key[0] in PAYERS:
            for line in entities[key].get("Line") or []:
                target = _linked(line) if isinstance(line, dict) else None
                amount = dec(line.get("Amount")) if isinstance(line, dict) else None
                if target in entities and amount is not None and "TotalAmt" in entities[target]:
                    apps.setdefault(target, []).append(amount)

    def precision(key, node, path):
        if isinstance(node, dict):
            for name, value in node.items():
                amount = dec(value) if name in MONEY_KEYS and name not in PRICE_KEYS else None
                if amount is not None and amount != q2(amount):
                    bad.add((key[0], key[1], f"precision {path}.{name}"))
                precision(key, value, f"{path}.{name}")
        elif isinstance(node, list):
            for i, item in enumerate(node):
                precision(key, item, f"{path}[{i}]")

    contacts: dict = {}
    for key in sorted(entities):
        raw = entities[key]
        precision(key, raw, "$")
        lines = [l for l in (raw.get("Line") or []) if isinstance(l, dict)]
        if key[0] in LINE_DOCS:
            leaf = taxable = discount = ZERO
            for i, line in enumerate(lines):
                amount = dec(line.get("Amount")) or ZERO
                detail = _detail(line)
                kind = line.get("DetailType")
                if kind == "SubTotalLineDetail":
                    require(key, f"subtotal line {i}", amount == leaf)
                    continue
                if kind == "DiscountLineDetail":
                    pct = dec(detail.get("DiscountPercent"))
                    if detail.get("PercentBased") and pct is not None:
                        require(key, f"discount line {i}", amount == q2(leaf_total(lines) * pct / 100))
                    discount += amount
                    continue
                if kind == "GroupLineDetail":
                    subs = [s for s in (detail.get("Line") or []) if isinstance(s, dict)]
                    require(key, f"group line {i}", amount == sum((dec(s.get("Amount")) or ZERO for s in subs), ZERO))
                    for j, sub in enumerate(subs):
                        _check_qty(require, key, f"group line {i}.{j}", sub)
                else:
                    _check_qty(require, key, f"line {i}", line)
                leaf += amount
                if _taxable(line):
                    taxable += amount
            tax = ZERO
            detail = raw.get("TxnTaxDetail")
            if isinstance(detail, dict):
                tax_sum = ZERO
                for i, tline in enumerate(detail.get("TaxLine") or []):
                    tld = _detail(tline)
                    nat, pct = dec(tld.get("NetAmountTaxable")), dec(tld.get("TaxPercent"))
                    amount = dec(tline.get("Amount")) or ZERO
                    if nat is not None:
                        require(key, f"tax base {i}", nat == taxable)
                        if pct is not None:
                            require(key, f"tax line {i}", amount == q2(nat * pct / 100))
                    tax_sum += amount
                if dec(detail.get("TotalTax")) is not None:
                    require(key, "total tax", dec(detail["TotalTax"]) == tax_sum)
                    tax = dec(detail["TotalTax"])
            if raw.get("GlobalTaxCalculation") == "TaxInclusive":
                tax = ZERO
            total = dec(raw.get("TotalAmt"))
            if total is not None:
                require(key, "total", total == leaf - discount + tax)
                base = total - (dec(raw.get("Deposit")) or ZERO)
                applied = sum(apps.get(key, []), ZERO)
                if dec(raw.get("Balance")) is not None:
                    require(key, "balance", dec(raw["Balance"]) == base - applied)
                if dec(raw.get("RemainingCredit")) is not None:
                    require(key, "remaining credit", dec(raw["RemainingCredit"]) == base - applied)
        elif key[0] == "JournalEntry":
            sides = {"Debit": ZERO, "Credit": ZERO}
            for line in lines:
                side = _detail(line).get("PostingType")
                if side in sides:
                    sides[side] += dec(line.get("Amount")) or ZERO
            require(key, "debits = credits", sides["Debit"] == sides["Credit"])
        elif key[0] in PAYERS:
            total = ZERO
            for line in lines:
                target = _linked(line)
                sign = -1 if target and target[0] in PAYERS[key[0]] else 1
                total += sign * (dec(line.get("Amount")) or ZERO)
            total += dec(raw.get("UnappliedAmt")) or ZERO
            if dec(raw.get("TotalAmt")) is not None:
                require(key, "payment total", dec(raw["TotalAmt"]) == total)
        elif key[0] == "Deposit":
            total = ZERO
            for i, line in enumerate(lines):
                amount = dec(line.get("Amount")) or ZERO
                target = _linked(line)
                if target in entities and dec(entities[target].get("TotalAmt")) is not None:
                    require(key, f"deposit line {i}", amount == dec(entities[target]["TotalAmt"]))
                total += amount
            back = raw.get("CashBack")
            if isinstance(back, dict):
                total -= dec(back.get("Amount")) or ZERO
            if dec(raw.get("TotalAmt")) is not None:
                require(key, "deposit total", dec(raw["TotalAmt"]) == total)
        for doc_type, ref_key, name, sign in (
            ("Invoice", "CustomerRef", "Balance", 1),
            ("CreditMemo", "CustomerRef", "Balance", -1),
            ("Payment", "CustomerRef", "UnappliedAmt", -1),
            ("Bill", "VendorRef", "Balance", 1),
            ("VendorCredit", "VendorRef", "Balance", -1),
        ):
            contact = _ref(raw, ref_key)
            if key[0] == doc_type and contact and dec(raw.get(name)) is not None:
                contact_key = ("Customer" if ref_key == "CustomerRef" else "Vendor", contact)
                contacts[contact_key] = contacts.get(contact_key, ZERO) + sign * dec(raw[name])

    for key in sorted(entities):
        raw = entities[key]
        if key[0] in ("Customer", "Vendor") and dec(raw.get("Balance")) is not None:
            require(key, "contact balance", dec(raw["Balance"]) == contacts.get(key, ZERO))
        for entity_type, own, total in (
            ("Customer", "Balance", "BalanceWithJobs"),
            ("Account", "CurrentBalance", "CurrentBalanceWithSubAccounts"),
        ):
            if key[0] == entity_type and dec(raw.get(total)) is not None:
                kids = sum(
                    (
                        dec(entities[k].get(total)) or ZERO
                        for k in entities
                        if k[0] == entity_type and _ref(entities[k], "ParentRef") == key[1]
                    ),
                    ZERO,
                )
                require(key, total, dec(raw[total]) == (dec(raw.get(own)) or ZERO) + kids)
    return bad


def leaf_total(lines: list) -> Decimal:
    return sum(
        (dec(l.get("Amount")) or ZERO for l in lines if isinstance(l, dict) and _is_leaf(l)),
        ZERO,
    )


def _check_qty(require, key, label, line) -> None:
    detail = _detail(line)
    qty, price, amount = dec(detail.get("Qty")), dec(detail.get("UnitPrice")), dec(line.get("Amount"))
    if qty is not None and price is not None and amount is not None and qty != 0:
        require(key, f"{label} qty x price", amount == q2(qty * price))


# ---------------------------------------------------------------------------
# Pass 2 — apply every name / contact rule
# ---------------------------------------------------------------------------


@dataclass
class Scrubber:
    mapper: NameMapper
    matcher: NameMatcher
    originals: Originals
    key: str = ""

    def replace_names(self, text: str) -> str:
        hits = self.matcher.find(text)
        if not hits:
            return text
        out, last = [], 0
        for start, end, name in hits:
            out.append(text[last:start])
            out.append(self.mapper.fake_for(name))
            last = end
        out.append(text[last:])
        return "".join(out)

    def carries_contact_or_amount(self, text: str) -> bool:
        o = self.originals
        if EMAIL_RE.search(text) or PHONE_RE.search(text):
            return True
        if any(s in text for s in o.emails) or any(s in text for s in o.phones):
            return True
        if any(s in text for s in o.streets) or any(s in text for s in o.realms):
            return True
        for match in AMOUNT_TEXT_RE.finditer(text):
            if match.group(0).replace(",", "") in o.amount_texts:
                return True
        return False

    def redact_if_sensitive(self, text: str) -> str:
        if self.matcher.contains(text) or self.carries_contact_or_amount(text):
            return SCRUBBED_MARK
        return text

    def scrub_string(self, key: str, text: str) -> str:
        if not text or is_structural(key, text):
            return text
        replaced = self.replace_names(text)
        return SCRUBBED_MARK if self.carries_contact_or_amount(replaced) else replaced

    def node(self, node, key: str = ""):
        if isinstance(node, list):
            return [self.node(item, key) for item in node]
        if isinstance(node, str):
            return self.scrub_string(key, node)
        if not isinstance(node, dict):
            return node

        out = {}
        for key, value in node.items():
            if key in ADDRESS_KEYS and isinstance(value, dict):
                out[key] = self.address(value)
            elif key in NAME_FIELDS and isinstance(value, str) and value in self.mapper.map:
                out[key] = self.mapper.fake_for(value)
            elif key == REF_NAME_KEY and isinstance(value, str) and value in self.mapper.map:
                out[key] = self.mapper.fake_for(value)
            elif key in EMAIL_KEYS and isinstance(value, dict):
                address = value.get("Address")
                scrubbed = self.node(value)
                if isinstance(address, str) and address:
                    scrubbed["Address"] = fake_email(address, self.key)
                out[key] = scrubbed
            elif key == "WebAddr" and isinstance(value, dict):
                out[key] = {**value, "URI": "https://example.com"} if value.get("URI") else dict(value)
            elif key == "FreeFormNumber" and isinstance(value, str) and value:
                out[key] = fake_phone(value, self.key)
            elif key in ALWAYS_REDACTED_KEYS and isinstance(value, str) and value:
                out[key] = SCRUBBED_MARK
            elif key == "DocNumber" and isinstance(value, (str, int)):
                out[key] = shift_doc_number(value)
            elif key.lower() == "realmid" and isinstance(value, str):
                out[key] = FIXED_REALM_ID
            elif key == "realm" and isinstance(value, str) and REALM_PATH_RE.match(value):
                out[key] = FIXED_REALM_ID
            elif key == "CustomerMemo" and isinstance(value, dict):
                memo = dict(value)
                if isinstance(memo.get("value"), str):
                    memo["value"] = self.redact_if_sensitive(memo["value"])
                out[key] = memo
            elif key in TEXT_FIELD_KEYS and isinstance(value, str):
                out[key] = self.redact_if_sensitive(value)
            else:
                out[key] = self.node(value, key)
        return out

    def address(self, addr: dict) -> dict:
        out = {}
        for key, value in addr.items():
            if key in ADDRESS_LINE_KEYS and isinstance(value, str) and value:
                # A line that is exactly a contact's name becomes that
                # contact's fake (so Line1 still reads like a recipient);
                # every other line is a fake street. Never a partial edit:
                # a street with one word swapped is still that street.
                if value in self.mapper.map:
                    out[key] = self.mapper.fake_for(value)
                else:
                    out[key] = fake_street(value, self.key)
            elif key == CITY_KEY and isinstance(value, str) and value:
                out[key] = fake_city(value, self.key)
            elif key == STATE_KEY:
                out[key] = value
            elif key == POSTAL_KEY:
                out[key] = FIXED_POSTAL_CODE
            else:
                out[key] = self.node(value, key)
        return out


# ---------------------------------------------------------------------------
# Leak detector — the backstop
# ---------------------------------------------------------------------------


@dataclass
class Leak:
    relpath: str
    path: str
    kind: str
    snippet: str


class LeakDetector:
    def __init__(self, originals: Originals, mapper: NameMapper, check_amounts: bool) -> None:
        self.o = originals
        self.names = NameMatcher(originals.names)
        self.fakes = NameMatcher(mapper.fakes())
        self.check_amounts = check_amounts

    def real_names_in(self, text: str) -> list:
        """Original names in `text`, ignoring any that sit inside a fake
        ("Cove" inside "Harbor Cove"; fakes never equal an original). Matched
        on the text as it is: blanking the fakes out first would turn the
        letters next to one into a false word boundary."""
        fakes = self.fakes.find(text)
        return [
            hit
            for hit in self.names.find(text)
            if not any(start < hit[1] and hit[0] < end for start, end, _ in fakes)
        ]

    def string_leak(self, text: str, key: str = ""):
        if not is_structural(key, text) and self.real_names_in(text):
            return "name"
        for email in EMAIL_RE.findall(text):
            if not email.lower().endswith("@example.com"):
                return "email"
        if any(e in text for e in self.o.emails):
            return "email"
        if PHONE_RE.search(text) or any(p in text for p in self.o.phones):
            return "phone"
        if any(s in text for s in self.o.streets):
            return "address"
        if any(r in text for r in self.o.realms):
            return "realm id"
        if self.check_amounts:
            for match in AMOUNT_TEXT_RE.finditer(text):
                if match.group(0).replace(",", "") in self.o.amount_texts:
                    return "amount (text)"
        return None

    def scan(self, relpath: str, scrubbed, original) -> list:
        leaks: list = []
        if any(r in relpath for r in self.o.realms):
            leaks.append(Leak(relpath, "(path)", "realm id", relpath))
        self._walk(relpath, scrubbed, original, "$", "", leaks)
        return leaks

    def _walk(self, relpath, node, original, path, key, leaks) -> None:
        if isinstance(node, dict):
            for name in sorted(node):
                orig = original.get(name) if isinstance(original, dict) else None
                self._walk(relpath, node[name], orig, f"{path}.{name}", name, leaks)
        elif isinstance(node, list):
            for i, item in enumerate(node):
                orig = original[i] if isinstance(original, list) and i < len(original) else None
                self._walk(relpath, item, orig, f"{path}[{i}]", key, leaks)
        elif isinstance(node, str):
            kind = self.string_leak(node, key)
            if kind:
                leaks.append(Leak(relpath, path, kind, node))
        elif self.check_amounts and key in MONEY_KEYS:
            new, old = dec(node), dec(original)
            if new is not None and old is not None and abs(old) >= LEAK_AMOUNT_FIELD_FLOOR and new == old:
                leaks.append(Leak(relpath, path, "amount (unchanged)", repr(node)))


# ---------------------------------------------------------------------------
# Directory walking
# ---------------------------------------------------------------------------


def scrub_relpath(rel: pathlib.PurePosixPath) -> pathlib.PurePosixPath:
    """Rename any realm-shaped numeric path segment to the fixed realm id."""
    parts = [FIXED_REALM_ID if REALM_PATH_RE.match(part) else part for part in rel.parts]
    return pathlib.PurePosixPath(*parts)


def _payloads(node, out: list) -> None:
    """Every recorded entity payload ({entity_type, qbo_id, raw_json}) in a
    loaded fixture file, in traversal order."""
    if isinstance(node, dict):
        if "entity_type" in node and "raw_json" in node and isinstance(node.get("raw_json"), dict):
            out.append(node)
            return
        for value in node.values():
            _payloads(value, out)
    elif isinstance(node, list):
        for item in node:
            _payloads(item, out)


@dataclass
class ScrubResult:
    file_count: int
    name_count: int
    entity_count: int = 0
    files_written: list = field(default_factory=list)


def scrub_amounts(loaded: dict, secret: str) -> dict:
    """Replace every payload's raw_json with its amount-scrubbed copy (in a
    deep copy of `loaded`). Raises ScrubError if scrubbing breaks an
    invariant the original kept."""
    loaded = copy.deepcopy(loaded)
    payloads: list = []
    for rel in sorted(loaded):
        _payloads(loaded[rel], payloads)
    # One canonical version per entity: the highest SyncToken. Other copies of
    # the same version (a query page and a CDC page) get the same result;
    # older versions are scrubbed on their own, without cross-document rules.
    canonical: dict = {}
    for payload in payloads:
        key = (payload["entity_type"], str(payload.get("qbo_id")))
        current = canonical.get(key)
        if current is None or _token(payload) > _token(current):
            canonical[key] = payload
    entities = {k: p["raw_json"] for k, p in canonical.items()}
    scrubbed = AmountScrubber(entities, secret).run()

    before, after = check_invariants(entities), check_invariants(scrubbed)
    broken = sorted(after - before)
    if broken:
        detail = "\n".join(f"  {t} {i}: {rule}" for t, i, rule in broken[:50])
        raise ScrubError(f"amount scrub broke {len(broken)} invariant(s) — nothing written\n{detail}")

    for payload in payloads:
        key = (payload["entity_type"], str(payload.get("qbo_id")))
        if payload["raw_json"] == entities[key]:
            payload["raw_json"] = copy.deepcopy(scrubbed[key])
        else:
            payload["raw_json"] = AmountScrubber({key: payload["raw_json"]}, secret).run()[key]
    return loaded


def _token(payload: dict) -> int:
    try:
        return int(payload.get("sync_token", 0))
    except (TypeError, ValueError):
        return 0


def run_scrub(
    src: pathlib.Path,
    dst: pathlib.Path,
    seed: str = "",
    keep_amounts: bool = False,
) -> ScrubResult:
    """Scrub every `*.json` file under `src` into `dst`. Raises
    [`ScrubError`] — writing nothing — on a bad invocation, a broken
    invariant or a leak; never writes a partial result."""
    if not src.is_dir():
        raise ScrubError(f"not a directory: {src}")
    if src.resolve() == dst.resolve():
        raise ScrubError("the scrubbed output must not be written in place")
    if not keep_amounts and not seed:
        raise ScrubError(
            "amount scrubbing needs a secret seed (--seed-file); pass --keep-amounts only for invented data"
        )

    files = sorted(p for p in src.rglob("*.json") if p.is_file())
    if not files:
        raise ScrubError(f"no .json files found under {src}")

    loaded: dict = {}
    for path in files:
        rel = scrub_relpath(pathlib.PurePosixPath(path.relative_to(src).as_posix()))
        try:
            loaded[str(rel)] = json.loads(path.read_text())
        except json.JSONDecodeError as error:
            raise ScrubError(f"{path}: not valid JSON: {error}") from error
    if not keep_amounts:
        reports = [rel for rel in loaded if rel.startswith("reports/")]
        if reports:
            raise ScrubError(
                f"{len(reports)} report file(s) under {src}/reports carry amounts no rule ties to "
                "the entities; record a slice without reports, or --keep-amounts for invented data"
            )

    originals = Originals()
    for rel in sorted(loaded):
        collect_originals(loaded[rel], originals)
    if keep_amounts:
        originals.amount_texts.clear()
    # Already the placeholder (invented data recorded under it): not a leak.
    originals.realms.discard(FIXED_REALM_ID)
    deny = src / DENY_NAMES_FILE
    deny_names = []
    if deny.is_file():
        deny_names = sorted({line.strip() for line in deny.read_text().splitlines() if line.strip()})
        originals.names.update(deny_names)
    for path in files:
        for part in path.relative_to(src).parts:
            if REALM_PATH_RE.match(part) and part != FIXED_REALM_ID:
                originals.realms.add(part)

    mapper = NameMapper(key=seed, reserved=frozenset(originals.names))
    for rel in sorted(loaded):
        collect_names_into(loaded[rel], mapper)
    for name in deny_names:
        mapper.fake_for(name)

    amounted = loaded if keep_amounts else scrub_amounts(loaded, seed)
    scrubber = Scrubber(mapper, NameMatcher(originals.names), originals, seed)
    scrubbed = {rel: scrubber.node(data) for rel, data in amounted.items()}

    detector = LeakDetector(originals, mapper, check_amounts=not keep_amounts)
    leaks = []
    for rel in sorted(scrubbed):
        leaks.extend(detector.scan(rel, scrubbed[rel], loaded[rel]))
    if leaks:
        shown = "\n".join(f"  {l.relpath} at {l.path} [{l.kind}]: {l.snippet!r}" for l in leaks[:200])
        more = f"\n  ... and {len(leaks) - 200} more" if len(leaks) > 200 else ""
        raise ScrubError(
            f"scrub failed: {len(leaks)} leaked value(s) found — nothing written to {dst}\n{shown}{more}"
        )

    written = []
    for rel in sorted(scrubbed):
        out_path = dst / rel
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(scrubbed[rel], indent=2, sort_keys=True) + "\n")
        written.append(rel)

    entity_count = 0
    for rel in scrubbed:
        found: list = []
        _payloads(scrubbed[rel], found)
        entity_count += len(found)
    return ScrubResult(
        file_count=len(files),
        name_count=len(mapper.map),
        entity_count=entity_count,
        files_written=written,
    )


def main(argv: list) -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("recorded_dir", type=pathlib.Path)
    parser.add_argument("scrubbed_dir", type=pathlib.Path)
    parser.add_argument(
        "--seed-file",
        type=pathlib.Path,
        help="file holding the secret seed (first line); keep it under .local/",
    )
    parser.add_argument(
        "--keep-amounts",
        action="store_true",
        help="leave amounts as recorded — invented data only, never a real recording",
    )
    args = parser.parse_args(argv)

    seed = ""
    if args.seed_file:
        try:
            seed = args.seed_file.read_text().splitlines()[0].strip()
        except (OSError, IndexError):
            print(f"cannot read a seed from {args.seed_file}", file=sys.stderr)
            return 2
    try:
        result = run_scrub(args.recorded_dir, args.scrubbed_dir, seed=seed, keep_amounts=args.keep_amounts)
    except ScrubError as error:
        print(str(error), file=sys.stderr)
        return 1

    print(
        f"scrubbed {result.file_count} file(s), {result.entity_count} entity payload(s) -> "
        f"{args.scrubbed_dir} ({result.name_count} name(s) mapped, amounts "
        f"{'kept' if args.keep_amounts else 'scrubbed'})"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
