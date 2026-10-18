"""Unit tests for tools/scrub-fixtures.py.

Run with:

    python3 -m unittest tools/test_scrub_fixtures.py

The module under test has a hyphen in its filename (matching
tools/build-snapshot.py's naming), so it cannot be `import`ed normally — it is
loaded here via importlib, by path.

Every name, address and note used below is invented for this test file only.
"""

import importlib.util
import json
import pathlib
import sys
import tempfile
import unittest

_SCRIPT = pathlib.Path(__file__).resolve().parent / "scrub-fixtures.py"
_spec = importlib.util.spec_from_file_location("scrub_fixtures", _SCRIPT)
scrub_fixtures = importlib.util.module_from_spec(_spec)
# dataclasses needs the module registered under its own name to resolve
# annotations — spec_from_file_location alone does not add it to sys.modules.
sys.modules[_spec.name] = scrub_fixtures
_spec.loader.exec_module(scrub_fixtures)  # type: ignore[union-attr]

ScrubError = scrub_fixtures.ScrubError
TEST_SEED = "unit-test-seed"


def run_scrub(src, dst, **kwargs):
    """The scrubber as a real recording runs it: amounts scrubbed, keyed by a
    seed. Pass keep_amounts=True for the invented-data path."""
    if not kwargs.get("keep_amounts"):
        kwargs.setdefault("seed", TEST_SEED)
    return scrub_fixtures.run_scrub(src, dst, **kwargs)


def write_json(path: pathlib.Path, data) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data))


def customer_fixture(display_name: str, realm_id: str = "1234567890123456") -> dict:
    return {
        "entity_type": "Customer",
        "qbo_id": "1",
        "sync_token": "0",
        "last_updated_utc": "2026-01-01T00:00:00+00:00",
        "is_deleted": False,
        "raw_json": {
            "Id": "1",
            "DisplayName": display_name,
            "RealmId": realm_id,
            "PrimaryEmailAddr": {"Address": "ap@example-source.test"},
            "PrimaryPhone": {"FreeFormNumber": "555-123-4567"},
            "BillAddr": {
                "Line1": display_name,
                "Line2": "500 Dock Street",
                "City": "Metropolis",
                "CountrySubDivisionCode": "NY",
                "PostalCode": "10001",
            },
        },
    }


def invoice_fixture(customer_name: str) -> dict:
    return {
        "entity_type": "Invoice",
        "qbo_id": "101",
        "sync_token": "0",
        "last_updated_utc": "2026-01-02T00:00:00+00:00",
        "is_deleted": False,
        "raw_json": {
            "Id": "101",
            "DocNumber": "1001",
            "CustomerRef": {"value": "1", "name": customer_name},
            "TotalAmt": 249.0,
            "Balance": 249.0,
            "PrivateNote": f"Ship early per {customer_name} request",
            "CustomerMemo": {"value": f"Thanks, {customer_name}!"},
            "Line": [{"Amount": 249.0, "Description": "Standard item, nothing sensitive here"}],
            "MetaData": {"CreateTime": "2026-01-02T00:00:00-05:00"},
        },
    }


def write_basic_fixture_set(root: pathlib.Path, customer_name: str = "TEST HARBOR CO") -> None:
    write_json(root / "Customer" / "fetch-1.json", customer_fixture(customer_name))
    write_json(root / "Invoice" / "fetch-101.json", invoice_fixture(customer_name))
    write_json(
        root / "manifest.json",
        [
            {
                "call": "fetch",
                "entity_type": "Customer",
                "params": {"realm": "1234567890123456", "entity_type": "Customer", "qbo_id": "1"},
                "response_file": "Customer/fetch-1.json",
            }
        ],
    )


class DeterminismTests(unittest.TestCase):
    def test_same_input_produces_byte_identical_output(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            write_basic_fixture_set(src)

            dst_a = root / "scrubbed-a"
            dst_b = root / "scrubbed-b"
            run_scrub(src, dst_a)
            run_scrub(src, dst_b)

            files_a = sorted(p.relative_to(dst_a) for p in dst_a.rglob("*.json"))
            files_b = sorted(p.relative_to(dst_b) for p in dst_b.rglob("*.json"))
            self.assertEqual(files_a, files_b)
            for rel in files_a:
                self.assertEqual(
                    (dst_a / rel).read_text(),
                    (dst_b / rel).read_text(),
                    f"{rel} differed between two runs on the same input",
                )

    def test_scrub_never_writes_in_place(self):
        with tempfile.TemporaryDirectory() as tmp:
            src = pathlib.Path(tmp)
            write_basic_fixture_set(src)
            with self.assertRaises(ScrubError):
                run_scrub(src, src)

    def test_empty_directory_is_refused(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "empty"
            src.mkdir()
            with self.assertRaises(ScrubError):
                run_scrub(src, root / "out")


class ReferentialConsistencyTests(unittest.TestCase):
    def test_the_same_original_name_maps_to_the_same_fake_everywhere(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            write_basic_fixture_set(src, customer_name="TEST HARBOR CO")
            run_scrub(src, dst)

            customer = json.loads((dst / "Customer" / "fetch-1.json").read_text())
            invoice = json.loads((dst / "Invoice" / "fetch-101.json").read_text())

            fake_name = customer["raw_json"]["DisplayName"]
            self.assertNotEqual(fake_name, "TEST HARBOR CO")

            # CustomerRef.name on a wholly different document...
            self.assertEqual(invoice["raw_json"]["CustomerRef"]["name"], fake_name)
            # ...and the address line that happened to repeat the name...
            self.assertEqual(customer["raw_json"]["BillAddr"]["Line1"], fake_name)

    def test_two_different_names_never_collide(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            write_json(src / "Customer" / "fetch-1.json", customer_fixture("ALPHA HARBOR CO"))
            write_json(src / "Customer" / "fetch-2.json", customer_fixture("BETA HARBOR CO"))
            run_scrub(src, dst)

            one = json.loads((dst / "Customer" / "fetch-1.json").read_text())["raw_json"]["DisplayName"]
            two = json.loads((dst / "Customer" / "fetch-2.json").read_text())["raw_json"]["DisplayName"]
            self.assertNotEqual(one, two)


class LeakDetectorTests(unittest.TestCase):
    def test_more_names_than_the_word_list_still_get_distinct_fakes(self):
        mapper = scrub_fixtures.NameMapper()
        span = len(scrub_fixtures.FIRST_WORDS) * len(scrub_fixtures.LAST_WORDS)
        originals = [f"Invented Customer {n}" for n in range(span * 3 + 7)]
        fakes = [mapper.fake_for(name) for name in originals]
        self.assertEqual(len(set(fakes)), len(originals))
        self.assertEqual(mapper.fake_for(originals[0]), fakes[0])
        again = scrub_fixtures.NameMapper()
        self.assertEqual([again.fake_for(name) for name in originals], fakes)
        self.assertTrue(any(fake.endswith(" 4") for fake in fakes))

    def test_a_name_hidden_in_an_unhandled_field_is_replaced(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            invoice = invoice_fixture("LEAK HARBOR CO")
            # A field this script has no specific rule for: the generic
            # string rule still replaces the name inside it.
            invoice["raw_json"]["CustomField"] = [
                {"DefinitionId": "1", "StringValue": "Please route through LEAK HARBOR CO's dock"}
            ]
            write_json(src / "Customer" / "fetch-1.json", customer_fixture("LEAK HARBOR CO"))
            write_json(src / "Invoice" / "fetch-101.json", invoice)
            run_scrub(src, dst)
            out = json.loads((dst / "Invoice" / "fetch-101.json").read_text())
            value = out["raw_json"]["CustomField"][0]["StringValue"]
            self.assertNotIn("LEAK HARBOR CO", value)
            self.assertIn("'s dock", value)

    def test_a_name_that_survives_scrubbing_fails_the_run(self):
        # Break the scrubber's string rule on purpose: if a name really does
        # get through, the backstop must catch it and write nothing.
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            invoice = invoice_fixture("LEAK HARBOR CO")
            invoice["raw_json"]["CustomField"] = [
                {"DefinitionId": "1", "StringValue": "Please route through LEAK HARBOR CO's dock"}
            ]
            write_json(src / "Customer" / "fetch-1.json", customer_fixture("LEAK HARBOR CO"))
            write_json(src / "Invoice" / "fetch-101.json", invoice)

            original = scrub_fixtures.Scrubber.scrub_string
            scrub_fixtures.Scrubber.scrub_string = lambda self, key, text: text
            try:
                with self.assertRaises(ScrubError) as ctx:
                    run_scrub(src, dst)
            finally:
                scrub_fixtures.Scrubber.scrub_string = original

            message = str(ctx.exception)
            self.assertIn("leaked", message)
            self.assertIn("Invoice", message)
            self.assertIn("[name]", message)
            # And nothing was written — a failed run must not leave a partial
            # scrubbed tree lying around to be committed by mistake.
            self.assertFalse(dst.exists())

    def test_a_clean_fixture_set_passes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            write_basic_fixture_set(src)
            result = run_scrub(src, dst)
            self.assertGreater(result.file_count, 0)


class FieldRuleTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        root = pathlib.Path(self.tmp.name)
        self.src = root / "recorded"
        self.dst = root / "scrubbed"
        write_basic_fixture_set(self.src, customer_name="FIELD HARBOR CO")
        run_scrub(self.src, self.dst)
        self.customer = json.loads((self.dst / "Customer" / "fetch-1.json").read_text())["raw_json"]
        self.invoice = json.loads((self.dst / "Invoice" / "fetch-101.json").read_text())["raw_json"]

    def test_email_becomes_a_fake_at_example_dot_com(self):
        self.assertTrue(self.customer["PrimaryEmailAddr"]["Address"].endswith("@example.com"))
        self.assertNotIn("example-source", self.customer["PrimaryEmailAddr"]["Address"])

    def test_phone_matches_the_555_01xx_pattern(self):
        self.assertRegex(self.customer["PrimaryPhone"]["FreeFormNumber"], r"^555-01\d{2}$")

    def test_postal_code_is_fixed_city_is_faked_and_state_survives(self):
        addr = self.customer["BillAddr"]
        self.assertEqual(addr["PostalCode"], "07000")
        self.assertNotEqual(addr["City"], "Metropolis")
        self.assertIn(addr["City"], scrub_fixtures.CITY_WORDS)
        self.assertEqual(addr["CountrySubDivisionCode"], "NY")

    def test_a_street_line_with_no_name_in_it_becomes_a_fake_street(self):
        line2 = self.customer["BillAddr"]["Line2"]
        self.assertNotEqual(line2, "500 Dock Street")
        self.assertRegex(line2, r"^\d+ \w+$")

    def test_doc_number_is_kept_but_shifted(self):
        self.assertEqual(self.invoice["DocNumber"], str(1001 + scrub_fixtures.DOC_NUMBER_OFFSET))

    def test_realm_id_field_is_replaced(self):
        self.assertEqual(self.customer["RealmId"], scrub_fixtures.FIXED_REALM_ID)

    def test_private_note_and_memo_are_redacted_when_they_name_the_customer(self):
        self.assertEqual(self.invoice["PrivateNote"], "[scrubbed]")
        self.assertEqual(self.invoice["CustomerMemo"]["value"], "[scrubbed]")

    def test_a_description_with_no_name_in_it_is_kept(self):
        self.assertEqual(
            self.invoice["Line"][0]["Description"],
            "Standard item, nothing sensitive here",
        )

    def test_ids_and_metadata_timestamps_are_untouched(self):
        self.assertEqual(self.customer["Id"], "1")
        self.assertEqual(self.invoice["Id"], "101")
        self.assertEqual(self.invoice["MetaData"]["CreateTime"], "2026-01-02T00:00:00-05:00")

    def test_amounts_move_but_stay_consistent(self):
        total = self.invoice["TotalAmt"]
        self.assertNotEqual(total, 249.0)
        self.assertEqual(self.invoice["Balance"], total)
        self.assertEqual(self.invoice["Line"][0]["Amount"], total)

    def test_keep_amounts_leaves_them_as_recorded(self):
        dst = pathlib.Path(self.tmp.name) / "kept"
        run_scrub(self.src, dst, keep_amounts=True)
        invoice = json.loads((dst / "Invoice" / "fetch-101.json").read_text())["raw_json"]
        self.assertEqual(invoice["TotalAmt"], 249.0)
        self.assertEqual(invoice["Balance"], 249.0)


class PathScrubTests(unittest.TestCase):
    def test_a_realm_shaped_path_segment_is_renamed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            src = root / "recorded"
            dst = root / "scrubbed"
            real_realm = "9876543210123456"
            write_json(
                src / real_realm / "Customer" / "fetch-1.json",
                customer_fixture("PATH HARBOR CO", realm_id=real_realm),
            )
            run_scrub(src, dst)

            renamed = dst / scrub_fixtures.FIXED_REALM_ID / "Customer" / "fetch-1.json"
            self.assertTrue(renamed.exists(), f"expected {renamed} to exist")
            self.assertFalse((dst / real_realm).exists())


if __name__ == "__main__":
    unittest.main()
