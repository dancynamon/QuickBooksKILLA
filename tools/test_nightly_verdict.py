"""Unit tests for tools/nightly-verdict.py, the decisions behind tools/nightly.sh.

Run with:

    python3 -m unittest tools/test_nightly_verdict.py

(also run by `cargo test -p ledger --test nightly_sh`). Loaded by path, since
the module's filename has a hyphen. Every name and amount here is invented.
"""

import datetime as dt
import importlib.util
import json
import pathlib
import sqlite3
import sys
import tempfile
import unittest

_SCRIPT = pathlib.Path(__file__).resolve().parent / "nightly-verdict.py"
_spec = importlib.util.spec_from_file_location("nightly_verdict", _SCRIPT)
nv = importlib.util.module_from_spec(_spec)
sys.modules[_spec.name] = nv
_spec.loader.exec_module(nv)  # type: ignore[union-attr]

REALM = "1234567890123456"
NOW = dt.datetime(2027, 1, 15, 2, 30, tzinfo=dt.timezone.utc)
AGING_HEADER = "entity_id,name,current,1-30,31-60,61-90,over_90,total\n"
TB_HEADER = "account,tier,ledger,qbo,delta\n"
CLEAN_TB = (
    TB_HEADER
    + "Bank — Checking (1100),must,250.00,250.00,0.00\n"
    + "Total income (4xxx),must,-250.00,-250.00,0.00\n"
    + "Cost of goods sold (5000),may,0.00,0.00,0.00\n"
    + "3950 Opening Balance Equity,skip,10.00,0.00,0.00\n"
)
DECISIONS = "# Decisions\n\n## D34 — Income mapping\n\ntext\n\n## D40 — AR credits\n"


def scratch(case):
    holder = tempfile.TemporaryDirectory()
    case.addCleanup(holder.cleanup)
    return pathlib.Path(holder.name)


def replica_with_cursors(path, cursors):
    con = sqlite3.connect(path)
    con.execute(
        "CREATE TABLE sync_cursors (realm_id TEXT, entity_type TEXT, "
        "last_cdc_cursor TEXT, last_full_sweep TEXT)"
    )
    for entity, cursor in cursors.items():
        con.execute(
            "INSERT INTO sync_cursors VALUES (?, ?, ?, NULL)", (REALM, entity, cursor)
        )
    con.commit()
    con.close()


class Freshness(unittest.TestCase):
    def setUp(self):
        self.dir = scratch(self)
        self.replica = self.dir / "snap.db"

    def test_a_cursor_minutes_old_is_fresh(self):
        replica_with_cursors(
            self.replica,
            {
                "Invoice": "2027-01-15T02:25:00.123456789+00:00",
                "Customer": "2027-01-15T02:20:00Z",
            },
        )
        result = nv.freshness(self.replica, REALM, 360, NOW)
        self.assertTrue(result["fresh"], result)
        self.assertEqual(result["oldest_entity"], "Customer")
        self.assertEqual(result["age_minutes"], 10.0)

    def test_one_stale_entity_makes_the_replica_stale(self):
        replica_with_cursors(
            self.replica,
            {
                "Invoice": "2027-01-15T02:25:00+00:00",
                "Bill": "2027-01-13T02:25:00+00:00",
            },
        )
        result = nv.freshness(self.replica, REALM, 360, NOW)
        self.assertFalse(result["fresh"])
        self.assertEqual(result["oldest_entity"], "Bill")
        self.assertIn("stale", result["problem"])

    def test_never_synced_or_missing_is_stale(self):
        replica_with_cursors(self.replica, {"Invoice": None})
        self.assertFalse(nv.freshness(self.replica, REALM, 360, NOW)["fresh"])
        other = self.dir / "empty.db"
        replica_with_cursors(other, {})
        self.assertIn("never synced", nv.freshness(other, REALM, 360, NOW)["problem"])
        self.assertIn("cannot read", nv.freshness(self.dir / "absent.db", REALM, 360, NOW)["problem"])

    def test_the_cli_exits_3_when_stale(self):
        replica_with_cursors(self.replica, {"Invoice": "2027-01-01T00:00:00Z"})
        out = self.dir / "f.json"
        code = nv.main(
            ["freshness", "--replica", str(self.replica), "--realm", REALM,
             "--max-age-min", "360", "--out", str(out), "--now", NOW.isoformat()]
        )
        self.assertEqual(code, nv.EXIT_STALE)
        self.assertFalse(json.loads(out.read_text())["fresh"])


class Night(unittest.TestCase):
    """`decide` over a run directory shaped as tools/nightly.sh leaves it."""

    def setUp(self):
        self.root = scratch(self)
        self.run = self.root / "runs" / "2027-01-14"
        self.run.mkdir(parents=True)
        (self.root / "DECISIONS.md").write_text(DECISIONS)
        self.explained = self.root / "explained.csv"
        self.steps = [
            ("config", "ok", 0), ("daemon", "ok", 0), ("snapshot", "ok", 0),
            ("freshness", "ok", 0), ("qbo-tb", "ok", 0), ("qbo-ar", "ok", 0),
            ("qbo-ap", "ok", 0), ("ledger-nightly", "ok", 0), ("ledger-aging", "ok", 0),
        ]
        self.write(daemon={"alive": True, "pids": [42]}, fresh=True)
        (self.run / "tbdiff.csv").write_text(CLEAN_TB)
        (self.run / "ledger-nightly.out").write_text("nightly: green\n")
        for kind in ("ar", "ap"):
            body = AGING_HEADER + "31,Blue Harbor Swim Club,100.00,0.00,0.00,0.00,0.00,100.00\n"
            (self.run / f"ledger-{kind}.csv").write_text(body)
            (self.run / f"qbo-{kind}.csv").write_text(body)

    def write(self, daemon=None, fresh=None):
        lines = [f"{n}\t{s}\t{c}\t1\t/log/{n}.log\t" for n, s, c in self.steps]
        (self.run / "steps.tsv").write_text("\n".join(lines) + "\n")
        if daemon is not None:
            (self.run / "daemon.json").write_text(json.dumps(daemon))
        if fresh is not None:
            (self.run / "freshness.json").write_text(json.dumps({
                "fresh": fresh, "problem": None if fresh else "replica stale: Invoice cursor is 2000 min old",
                "oldest_cursor": "2027-01-15T02:00:00+00:00", "oldest_entity": "Invoice",
                "age_minutes": 30.0 if fresh else 2000.0, "max_age_minutes": 360,
                "snapshot": "/snap.db",
            }))

    def decide(self):
        return nv.decide(self.run, "2027-01-14", "2027-01-01", "aquamentor",
                         self.explained, self.root / "DECISIONS.md")

    def test_a_clean_night_is_green(self):
        report = self.decide()
        self.assertEqual(report["verdict"], "green", report["reasons"])
        self.assertEqual(report["exit_code"], 0)
        self.assertEqual(report["unexplained"], 0)
        self.assertEqual(report["aging"]["ar"]["differing"], 0)

    def test_a_stale_replica_is_exit_3_even_if_the_books_tie(self):
        self.steps = self.steps[:3] + [("freshness", "stale", 3)]
        self.write(fresh=False)
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_STALE)
        self.assertTrue(any("stale" in r for r in report["reasons"]))

    def test_a_dead_daemon_is_exit_3(self):
        self.steps = self.steps[:1] + [("daemon", "down", 1)]
        self.write(daemon={"alive": False, "pids": []})
        (self.run / "freshness.json").unlink()
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_STALE)
        self.assertIn("qbo-local daemon is not running", report["reasons"])

    def test_a_must_tier_variance_is_red_until_explained(self):
        (self.run / "tbdiff.csv").write_text(
            CLEAN_TB.replace("Total income (4xxx),must,-250.00,-250.00,0.00",
                             "Total income (4xxx),must,-250.00,-262.50,12.50")
        )
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_VARIANCE)
        self.assertEqual(report["tb"]["must_failures"], 1)
        self.assertTrue(any("Total income" in r and "12.50" in r for r in report["reasons"]))

        self.explained.write_text(
            "kind,key,delta,decision,note\ntb,Total income (4xxx),12.50,D34,shipping items\n"
        )
        report = self.decide()
        self.assertEqual(report["exit_code"], 0, report["reasons"])
        row = next(r for r in report["tb"]["rows"] if r["account"].startswith("Total income"))
        self.assertEqual(row["explained_by"], "D34")

    def test_an_explanation_for_a_different_amount_does_not_explain(self):
        (self.run / "tbdiff.csv").write_text(
            CLEAN_TB.replace("Cost of goods sold (5000),may,0.00,0.00,0.00",
                             "Cost of goods sold (5000),may,30.00,0.00,30.00")
        )
        self.explained.write_text("tb,Cost of goods sold (5000),29.99,D34,\n")
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_VARIANCE)
        self.assertEqual(report["tb"]["may_differ"], 1)
        self.assertEqual(len(report["explanations"]["unused"]), 1)
        self.explained.write_text("tb,Cost of goods sold (5000),*,D34,\n")
        self.assertEqual(self.decide()["exit_code"], 0)

    def test_an_explanation_naming_no_recorded_decision_is_ignored(self):
        (self.run / "tbdiff.csv").write_text(
            CLEAN_TB.replace("Total income (4xxx),must,-250.00,-250.00,0.00",
                             "Total income (4xxx),must,-250.00,-262.50,12.50")
        )
        self.explained.write_text("tb,Total income (4xxx),*,D999,made up\n")
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_VARIANCE)
        self.assertIn("D999 is not a heading", report["explanations"]["invalid"][0]["problem"])

    def test_a_rejected_document_is_red(self):
        (self.run / "ledger-nightly.out").write_text(
            "nightly: RED — 0 must-tier failure(s), 1 rejected document(s)\n"
            "wrote x\n\nimport rejected 1 document(s):\n  - qbo:Invoice:77: no class on line 1\n"
        )
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_VARIANCE)
        self.assertEqual(report["rejected"]["documents"][0]["document_id"], "qbo:Invoice:77")

    def test_an_aging_difference_by_contact_is_red_until_explained(self):
        (self.run / "qbo-ar.csv").write_text(
            AGING_HEADER
            + "31,Blue Harbor Swim Club,60.00,40.00,0.00,0.00,0.00,100.00\n"
            + "44,Gull Pond Aquatics,0.00,0.00,0.00,0.00,25.00,25.00\n"
        )
        report = self.decide()
        ar = report["aging"]["ar"]
        self.assertEqual(report["exit_code"], nv.EXIT_VARIANCE)
        self.assertEqual(ar["differing"], 2)
        blue = next(r for r in ar["rows"] if r["contact"] == "31")
        self.assertEqual(blue["delta"], "0.00")
        self.assertEqual(blue["buckets_differ"], ["current", "1-30"])
        self.explained.write_text("ar,*,*,D40,unapplied credits\n")
        self.assertEqual(self.decide()["exit_code"], 0)

    def test_a_qbo_row_without_an_id_matches_by_name(self):
        (self.run / "qbo-ap.csv").write_text(
            AGING_HEADER + ",Blue Harbor Swim Club,100.00,0.00,0.00,0.00,0.00,100.00\n"
            + ",TOTAL,100.00,0.00,0.00,0.00,0.00,100.00\n"
        )
        self.assertEqual(self.decide()["aging"]["ap"]["differing"], 0)

    def test_a_failed_step_outranks_a_variance(self):
        self.steps[4] = ("qbo-tb", "failed", 1)
        self.write()
        (self.run / "tbdiff.csv").unlink()
        report = self.decide()
        self.assertEqual(report["exit_code"], nv.EXIT_STEP_FAILED)
        self.assertTrue(any("step qbo-tb failed" in r for r in report["reasons"]))


class Finish(unittest.TestCase):
    def test_writes_both_files_and_keeps_the_newest_n(self):
        root = scratch(self)
        reports = root / "reports"
        for day in ("2027-01-12", "2027-01-13", "2027-01-14"):
            run = reports / "runs" / day
            run.mkdir(parents=True)
            (run / "steps.tsv").write_text("daemon\tok\t0\t0\t-\t\n")
            (run / "tbdiff.csv").write_text(CLEAN_TB)
            code = nv.main(["finish", "--run-dir", str(run), "--report-dir", str(reports),
                            "--as-of", day, "--keep", "2"])
            self.assertEqual(code, 0)
        names = sorted(p.name for p in reports.iterdir() if p.is_file())
        self.assertEqual(names, ["nightly-2027-01-13.json", "nightly-2027-01-13.md",
                                 "nightly-2027-01-14.json", "nightly-2027-01-14.md"])
        self.assertEqual(sorted(p.name for p in (reports / "runs").iterdir()),
                         ["2027-01-13", "2027-01-14"])
        md = (reports / "nightly-2027-01-14.md").read_text()
        self.assertIn("2027-01-14: GREEN", md)
        report = json.loads((reports / "nightly-2027-01-14.json").read_text())
        self.assertEqual(report["schema"], 1)


if __name__ == "__main__":
    unittest.main()
