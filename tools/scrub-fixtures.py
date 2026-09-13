#!/usr/bin/env python3
"""Scrub a recorded QBO fixture directory into a directory safe to commit.

Reads   <recorded_dir>/**/*.json   (RecordingQbo's output — see fixture.rs)
Writes  <scrubbed_dir>/**/*.json   — NEVER in place, and only if the run is clean

    python3 tools/scrub-fixtures.py .local/fixtures/aquamentor apps/qbo-local/tests/fixtures/synthetic

HANDOFF.md §2.6 is the rule this script exists to satisfy: **a fixture that
reaches `git add` must not contain a real customer, vendor, balance or realm
id.** Scrub on the way in, not on the way out — this repository has already
had to clean a real name out of a committed file once (see
`prototype/README.md`, "The data in this prototype is fictional"), and finding
one after the fact is much more work than never writing it.

Deterministic: the same recorded input always produces the same scrubbed
output byte-for-byte, so a re-record diffs cleanly against what is already
committed. Every substitution is keyed off a hash of the ORIGINAL value, never
off anything positional, and both the file tree and every JSON object are
walked in a fixed order.

Rules:
  - DisplayName / CompanyName / GivenName / FamilyName / FullyQualifiedName /
    PrintOnCheckName / Name, read off a Customer, Vendor or Employee record ->
    a fake name from a fixed word list, keyed by a hash of the original
    string. The same original string always maps to the same fake, and that
    mapping is then applied wherever the exact original string recurs
    elsewhere in the fixture set -- inside CustomerRef.name, VendorRef.name,
    EntityRef.name, and an address's Line fields (a Line1 is often the
    recipient's name).
  - PrimaryEmailAddr.Address -> <fake>@example.com
  - Any FreeFormNumber (phone/fax) -> 555-01xx
  - Address lines that are not a name substitution get a fake street; City
    and the state code are kept as recorded; PostalCode -> 07000
  - DocNumber is kept -- invoice numbers are not personal -- but shifted by a
    constant offset, since the series must not match the real sequence
  - PrivateNote, CustomerMemo.value and a line's Description are kept UNLESS
    they contain one of the names being scrubbed, in which case the whole
    field becomes "[scrubbed]"
  - realmId (any casing of the key) and any realm-shaped numeric path segment
    -> a fixed placeholder realm id
  - Id, amounts, MetaData timestamps and TxnTaxDetail are never touched

The leak detector is the backstop, not a substitute for the rules above:
after scrubbing, every string in the OUTPUT is checked for a leftover
instance of an original scrubbed name. Any hit fails the whole run — nothing
is written to the destination — and names the file and JSON path, so a leak
can never pass silently.
"""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import sys
from dataclasses import dataclass, field

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

NAME_FIELDS = (
    "DisplayName",
    "CompanyName",
    "GivenName",
    "FamilyName",
    "FullyQualifiedName",
    "PrintOnCheckName",
    "Name",
)
NAME_ENTITY_TYPES = ("Customer", "Vendor", "Employee")
REF_NAME_KEY = "name"  # CustomerRef.name / VendorRef.name / EntityRef.name

ADDRESS_KEYS = ("BillAddr", "ShipAddr")
ADDRESS_LINE_KEYS = ("Line1", "Line2", "Line3", "Line4", "Line5")
STATE_KEY = "CountrySubDivisionCode"
CITY_KEY = "City"
POSTAL_KEY = "PostalCode"
FIXED_POSTAL_CODE = "07000"

DOC_NUMBER_OFFSET = 100_000
FIXED_REALM_ID = "1234567890123456"
SCRUBBED_MARK = "[scrubbed]"

TEXT_FIELD_KEYS = ("PrivateNote", "Description")  # CustomerMemo.value handled separately

# A realm id is a long run of digits. QBO's are 16 digits; keep the floor
# comfortably below that so an id from a differently-sized company still gets
# caught in a path segment.
REALM_PATH_RE = re.compile(r"^\d{10,}$")

# Deliberately plain, deliberately fictional. Two independent lists combine
# into FIRST_WORDS x LAST_WORDS candidates, which is plenty of headroom for a
# fixture set with a handful of names in it, and open addressing (see
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
STREET_WORDS = [
    "Ave", "Blvd", "Way", "Lane", "Drive", "Court", "Place", "Trail",
    "Road", "Circle", "Terrace", "Path", "Row", "Crossing", "Alley", "Loop",
]


class ScrubError(Exception):
    """Raised for anything that must stop the run before it writes a file —
    a bad invocation, or a leak the detector caught."""


# ---------------------------------------------------------------------------
# Deterministic, hash-keyed fakes
# ---------------------------------------------------------------------------


def _stable_int(value: str) -> int:
    digest = hashlib.sha256(value.encode("utf-8")).digest()
    return int.from_bytes(digest[:8], "big")


class NameMapper:
    """original name string -> fake name string.

    Open-addressed: if two different originals hash to the same candidate,
    the second probes forward deterministically until it finds a fake that is
    not already taken, so two different real names can never end up sharing
    one fake — that would merge two different customers into one in the
    scrubbed output, which is a worse bug than a slow collision search.
    """

    def __init__(self) -> None:
        self.map: dict[str, str] = {}
        self._taken: set[str] = set()

    def fake_for(self, original: str) -> str:
        if original in self.map:
            return self.map[original]
        span = len(FIRST_WORDS) * len(LAST_WORDS)
        index = _stable_int(original) % span
        for _ in range(span):
            candidate = f"{FIRST_WORDS[index // len(LAST_WORDS)]} {LAST_WORDS[index % len(LAST_WORDS)]}"
            if candidate not in self._taken:
                break
            index = (index + 1) % span
        else:  # pragma: no cover — would need more distinct names than the word list can hold
            raise ScrubError("name word list exhausted — add more words to FIRST_WORDS/LAST_WORDS")
        self._taken.add(candidate)
        self.map[original] = candidate
        return candidate

    def originals(self) -> list[str]:
        return list(self.map.keys())


def fake_street(original: str) -> str:
    digest = _stable_int(original)
    number = 100 + (digest % 900)
    word = STREET_WORDS[digest % len(STREET_WORDS)]
    return f"{number} {word}"


def fake_email(original: str) -> str:
    digest = hashlib.sha256(original.encode("utf-8")).hexdigest()[:10]
    return f"contact-{digest}@example.com"


def fake_phone(original: str) -> str:
    digest = _stable_int(original) % 100
    return f"555-01{digest:02d}"


def shift_doc_number(value):
    try:
        return str(int(value) + DOC_NUMBER_OFFSET)
    except (TypeError, ValueError):
        # Not purely numeric (e.g. "EST-1001") — DocNumber is kept as-is
        # rather than guessing at a format-preserving shift.
        return value


def replace_names_in_text(text: str, mapper: NameMapper) -> str:
    """Substring replacement of every collected original name, longest first
    so a shorter name that happens to be a prefix of a longer one never
    shadows it."""
    result = text
    for original in sorted(mapper.originals(), key=len, reverse=True):
        if original and original in result:
            result = result.replace(original, mapper.fake_for(original))
    return result


def redact_if_named(text: str, mapper: NameMapper) -> str:
    for original in mapper.originals():
        if original and original in text:
            return SCRUBBED_MARK
    return text


# ---------------------------------------------------------------------------
# Pass 1 — collect every name worth scrubbing
# ---------------------------------------------------------------------------


def collect_names(node, mapper: NameMapper, nested: bool = False) -> None:
    if isinstance(node, dict):
        entity_type = node.get("entity_type")
        is_named_entity = nested or entity_type in NAME_ENTITY_TYPES
        for key, value in node.items():
            if is_named_entity and key in NAME_FIELDS and isinstance(value, str) and value:
                mapper.fake_for(value)
            collect_names(value, mapper, is_named_entity)
    elif isinstance(node, list):
        for item in node:
            collect_names(item, mapper, nested)


# ---------------------------------------------------------------------------
# Pass 2 — apply every rule
# ---------------------------------------------------------------------------


def scrub_node(node, mapper: NameMapper):
    if isinstance(node, list):
        return [scrub_node(item, mapper) for item in node]
    if not isinstance(node, dict):
        return node

    out = {}
    for key, value in node.items():
        if key in ADDRESS_KEYS and isinstance(value, dict):
            out[key] = scrub_address(value, mapper)
        elif key in NAME_FIELDS and isinstance(value, str) and value in mapper.map:
            out[key] = mapper.fake_for(value)
        elif key == REF_NAME_KEY and isinstance(value, str) and value in mapper.map:
            out[key] = mapper.fake_for(value)
        elif key == "PrimaryEmailAddr" and isinstance(value, dict):
            address = value.get("Address")
            scrubbed = scrub_node(value, mapper)
            if isinstance(address, str) and address:
                scrubbed["Address"] = fake_email(address)
            out[key] = scrubbed
        elif key == "FreeFormNumber" and isinstance(value, str) and value:
            out[key] = fake_phone(value)
        elif key == "DocNumber" and isinstance(value, (str, int)):
            out[key] = shift_doc_number(value)
        elif key.lower() == "realmid" and isinstance(value, str):
            out[key] = FIXED_REALM_ID
        elif key == "realm" and isinstance(value, str) and REALM_PATH_RE.match(value):
            out[key] = FIXED_REALM_ID
        elif key == "CustomerMemo" and isinstance(value, dict):
            memo = dict(value)
            if isinstance(memo.get("value"), str):
                memo["value"] = redact_if_named(memo["value"], mapper)
            out[key] = memo
        elif key in TEXT_FIELD_KEYS and isinstance(value, str):
            out[key] = redact_if_named(value, mapper)
        else:
            out[key] = scrub_node(value, mapper)
    return out


def scrub_address(addr: dict, mapper: NameMapper) -> dict:
    out = {}
    for key, value in addr.items():
        if key in ADDRESS_LINE_KEYS and isinstance(value, str) and value:
            replaced = replace_names_in_text(value, mapper)
            out[key] = replaced if replaced != value else fake_street(value)
        elif key == CITY_KEY:
            out[key] = value
        elif key == STATE_KEY:
            out[key] = value
        elif key == POSTAL_KEY:
            out[key] = FIXED_POSTAL_CODE
        else:
            out[key] = scrub_node(value, mapper)
    return out


# ---------------------------------------------------------------------------
# Leak detector — the backstop
# ---------------------------------------------------------------------------


def find_leaks(docs: dict[str, object], mapper: NameMapper) -> list[tuple[str, str, str]]:
    originals = [o for o in mapper.originals() if o]
    leaks: list[tuple[str, str, str]] = []
    for relpath in sorted(docs):
        _walk_for_leaks(docs[relpath], "$", originals, relpath, leaks)
    return leaks


def _walk_for_leaks(node, path, originals, relpath, leaks):
    if isinstance(node, dict):
        for key in sorted(node.keys()):
            _walk_for_leaks(node[key], f"{path}.{key}", originals, relpath, leaks)
    elif isinstance(node, list):
        for i, item in enumerate(node):
            _walk_for_leaks(item, f"{path}[{i}]", originals, relpath, leaks)
    elif isinstance(node, str):
        for original in originals:
            if original in node:
                leaks.append((relpath, path, node))
                break


# ---------------------------------------------------------------------------
# Directory walking
# ---------------------------------------------------------------------------


def scrub_relpath(rel: pathlib.PurePosixPath) -> pathlib.PurePosixPath:
    """Rename any realm-shaped numeric path segment to the fixed realm id."""
    parts = [FIXED_REALM_ID if REALM_PATH_RE.match(part) else part for part in rel.parts]
    return pathlib.PurePosixPath(*parts)


@dataclass
class ScrubResult:
    file_count: int
    name_count: int
    files_written: list[str] = field(default_factory=list)


def run_scrub(src: pathlib.Path, dst: pathlib.Path) -> ScrubResult:
    """Scrub every `*.json` file under `src` into `dst`. Raises [`ScrubError`]
    — writing nothing — on a bad invocation or a leaked name; never writes a
    partial result."""
    if not src.is_dir():
        raise ScrubError(f"not a directory: {src}")
    if src.resolve() == dst.resolve():
        raise ScrubError("the scrubbed output must not be written in place")

    files = sorted(p for p in src.rglob("*.json") if p.is_file())
    if not files:
        raise ScrubError(f"no .json files found under {src}")

    mapper = NameMapper()
    loaded: dict[str, object] = {}
    for path in files:
        rel = scrub_relpath(pathlib.PurePosixPath(path.relative_to(src).as_posix()))
        try:
            data = json.loads(path.read_text())
        except json.JSONDecodeError as error:
            raise ScrubError(f"{path}: not valid JSON: {error}") from error
        collect_names(data, mapper)
        loaded[str(rel)] = data

    scrubbed = {rel: scrub_node(data, mapper) for rel, data in loaded.items()}

    leaks = find_leaks(scrubbed, mapper)
    if leaks:
        detail = "\n".join(f"  {rel} at {path}: {snippet!r}" for rel, path, snippet in leaks)
        raise ScrubError(
            f"scrub failed: {len(leaks)} leaked name(s) found — nothing written to {dst}\n{detail}"
        )

    written = []
    for rel in sorted(scrubbed):
        out_path = dst / rel
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(json.dumps(scrubbed[rel], indent=2, sort_keys=True) + "\n")
        written.append(rel)

    return ScrubResult(file_count=len(files), name_count=len(mapper.map), files_written=written)


def main(argv: list[str]) -> int:
    if len(argv) != 2:
        print("usage: scrub-fixtures.py <recorded_dir> <scrubbed_dir>", file=sys.stderr)
        return 2

    src = pathlib.Path(argv[0])
    dst = pathlib.Path(argv[1])
    try:
        result = run_scrub(src, dst)
    except ScrubError as error:
        print(str(error), file=sys.stderr)
        return 1

    print(
        f"scrubbed {result.file_count} file(s) -> {dst} "
        f"({result.name_count} name(s) mapped)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
