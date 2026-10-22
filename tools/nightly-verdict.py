#!/usr/bin/env python3
"""The decisions behind tools/nightly.sh (docs/NIGHTLY.md, "Switching it on").

The shell script runs the steps; this file decides what they add up to, so the
decisions can be tested without a daemon, a replica or QBO:

    nightly-verdict.py freshness --replica SNAPSHOT --realm ID
                                 --max-age-min N --out FILE.json [--now ISO]
        Is the replica's CDC cursor fresh? Reads sync_cursors read-only from
        the snapshot the run just took (never the live replica) and judges
        the OLDEST entity cursor, so one entity type that stopped syncing is
        caught even while the rest are current. Exit 0 fresh, 3 stale.

    nightly-verdict.py finish --run-dir DIR --report-dir DIR --as-of DATE
                              [--from DATE] [--company ID] [--explained CSV]
                              [--decisions DECISIONS.md] [--keep N]
        Read what the steps left in --run-dir (steps.tsv, daemon.json,
        freshness.json, the tbdiff CSV, ledger nightly's stdout, the four
        aging CSVs), decide the night, write nightly-<as-of>.md and .json
        into --report-dir, keep the newest --keep nights, and exit with the
        night's code:

            0  green: every step ran, nothing unexplained
            1  red: an unexplained variance (trial balance or aging) or a
               document the importer rejected
            3  the daemon is down or the replica is stale
            4  a step failed (a command exited non-zero, or left no output)

An explanation file (CSV, header kind,key,delta,decision,note) is how a
known variance stops turning the night red. kind is tb, ar or ap; key is the
tbdiff account label exactly as the CSV prints it, or an aging contact's QBO
id (or `*`, any); delta is the exact ledger-minus-QBO amount or `*` (any
amount); decision must name a heading in DECISIONS.md (`## D34 — ...`), because
the go/no-go bar counts a variance with no recorded policy as unexplained.
"""

import argparse
import csv
import datetime as dt
import json
import pathlib
import re
import sqlite3
import sys
from decimal import Decimal, InvalidOperation

EXIT_GREEN = 0
EXIT_VARIANCE = 1
EXIT_USAGE = 2
EXIT_STALE = 3
EXIT_STEP_FAILED = 4

BUCKETS = ["current", "1-30", "31-60", "61-90", "over_90", "total"]
REPORT_GLOB = re.compile(r"^nightly-(\d{4}-\d{2}-\d{2})\.(md|json)$")


# -- small parsers ----------------------------------------------------------


def parse_ts(raw):
    """RFC 3339 as chrono writes it (nanoseconds, +00:00 or Z) to aware UTC."""
    text = raw.strip().replace("Z", "+00:00")
    text = re.sub(r"(\.\d{6})\d+", r"\1", text)
    stamp = dt.datetime.fromisoformat(text)
    if stamp.tzinfo is None:
        stamp = stamp.replace(tzinfo=dt.timezone.utc)
    return stamp.astimezone(dt.timezone.utc)


def money(raw):
    try:
        return Decimal((raw or "0").strip() or "0").quantize(Decimal("0.01"))
    except InvalidOperation:
        raise ValueError(f"not an amount: {raw!r}")


def fmt(amount):
    return f"{amount:,.2f}"


def read_json(path, default=None):
    try:
        return json.loads(pathlib.Path(path).read_text())
    except (FileNotFoundError, json.JSONDecodeError):
        return default


# -- freshness --------------------------------------------------------------


def read_cursors(replica, realm):
    """sync_cursors, read-only. The replica is in WAL mode, and so is a
    snapshot (the backup API copies the header): a read-only open of a WAL
    file with no -shm beside it fails, so a closed copy is opened immutable."""
    if not pathlib.Path(replica).is_file():
        raise sqlite3.OperationalError("no such file")
    base = pathlib.Path(replica).resolve().as_uri()
    last = None
    for query in ("?mode=ro", "?mode=ro&immutable=1"):
        try:
            con = sqlite3.connect(base + query, uri=True)
            try:
                return con.execute(
                    "SELECT entity_type, last_cdc_cursor FROM sync_cursors WHERE realm_id = ?",
                    (realm,),
                ).fetchall()
            finally:
                con.close()
        except sqlite3.OperationalError as err:
            last = err
    raise last


def freshness(replica, realm, max_age_min, now):
    """The replica's CDC cursors, oldest first judged against max_age_min."""
    result = {
        "snapshot": str(replica),
        "max_age_minutes": max_age_min,
        "checked_at": now.isoformat(),
        "fresh": False,
        "oldest_entity": None,
        "oldest_cursor": None,
        "newest_cursor": None,
        "age_minutes": None,
        "problem": None,
    }
    try:
        rows = read_cursors(replica, realm)
    except sqlite3.Error as err:
        result["problem"] = f"cannot read sync_cursors from {replica}: {err}"
        return result
    if not rows:
        result["problem"] = f"no CDC cursor for realm {realm}: the replica has never synced"
        return result
    never = sorted(entity for entity, cursor in rows if not cursor)
    if never:
        result["problem"] = "no CDC cursor yet for " + ", ".join(never)
        return result
    stamped = sorted((parse_ts(cursor), entity) for entity, cursor in rows)
    oldest, oldest_entity = stamped[0]
    age = (now - oldest).total_seconds() / 60
    result.update(
        oldest_entity=oldest_entity,
        oldest_cursor=oldest.isoformat(),
        newest_cursor=stamped[-1][0].isoformat(),
        age_minutes=round(age, 1),
        fresh=age <= max_age_min,
    )
    if not result["fresh"]:
        result["problem"] = (
            f"replica stale: {oldest_entity} cursor is {age:.0f} min old "
            f"(limit {max_age_min} min) — is the qbo-local daemon polling?"
        )
    return result


# -- explanations -----------------------------------------------------------


def decision_ids(decisions_path):
    if not decisions_path or not pathlib.Path(decisions_path).exists():
        return None
    text = pathlib.Path(decisions_path).read_text()
    return set(re.findall(r"^#+\s*(D\d+)\b", text, flags=re.M))


def load_explanations(path, known_decisions):
    """(valid, invalid) explanation rows. A missing file is no explanations."""
    valid, invalid = [], []
    if not path or not pathlib.Path(path).exists():
        return valid, invalid
    with open(path, newline="") as handle:
        rows = [r for r in csv.reader(handle) if r and not r[0].lstrip().startswith("#")]
    if rows and [c.strip().lower() for c in rows[0][:3]] == ["kind", "key", "delta"]:
        rows = rows[1:]
    for number, row in enumerate(rows, start=1):
        cells = [c.strip() for c in row] + [""] * 5
        kind, key, delta, decision, note = cells[:5]
        entry = {"kind": kind, "key": key, "delta": delta, "decision": decision, "note": note}
        problem = None
        if kind not in ("tb", "ar", "ap"):
            problem = f"kind {kind!r} is not tb, ar or ap"
        elif not key:
            problem = "no key"
        elif not decision:
            problem = "no decision id"
        elif known_decisions is not None and decision not in known_decisions:
            problem = f"{decision} is not a heading in DECISIONS.md"
        elif delta != "*":
            try:
                entry["delta"] = str(money(delta))
            except ValueError as err:
                problem = str(err)
        if problem:
            entry["problem"] = f"row {number}: {problem}"
            invalid.append(entry)
        else:
            entry["used"] = False
            valid.append(entry)
    return valid, invalid


def explain(explanations, kind, key, delta):
    for entry in explanations:
        if entry["kind"] != kind:
            continue
        if entry["key"] not in ("*", key):
            continue
        if entry["delta"] not in ("*", str(delta)):
            continue
        entry["used"] = True
        return entry["decision"]
    return None


# -- trial balance ----------------------------------------------------------


def read_tbdiff(path, explanations):
    rows = []
    with open(path, newline="") as handle:
        for row in csv.DictReader(handle):
            delta = money(row["delta"])
            tier = row["tier"]
            entry = {
                "account": row["account"],
                "tier": tier,
                "ledger": str(money(row["ledger"])),
                "qbo": str(money(row["qbo"])),
                "delta": str(delta),
                "explained_by": None,
            }
            if tier in ("must", "may") and delta != 0:
                entry["explained_by"] = explain(explanations, "tb", row["account"], delta)
            rows.append(entry)
    variances = [r for r in rows if r["tier"] in ("must", "may") and money(r["delta"]) != 0]
    return {
        "csv": str(path),
        "rows": rows,
        "must_failures": sum(1 for r in variances if r["tier"] == "must"),
        "may_differ": sum(1 for r in variances if r["tier"] == "may"),
        "unexplained": sum(1 for r in variances if not r["explained_by"]),
    }


def read_rejected(stdout_path):
    """`ledger nightly` prints `import rejected N document(s):` then `  - id: reason`."""
    out = {"count": 0, "documents": []}
    try:
        lines = pathlib.Path(stdout_path).read_text().splitlines()
    except FileNotFoundError:
        return out
    inside = False
    for line in lines:
        if re.match(r"^import rejected \d+ document", line):
            inside = True
            continue
        if inside and line.startswith("  - "):
            doc, _, reason = line[4:].partition(": ")
            out["documents"].append({"document_id": doc, "reason": reason})
        elif inside and line.strip():
            inside = False
    out["count"] = len(out["documents"])
    return out


# -- aging --------------------------------------------------------------------


def read_aging(path):
    """{key: (name, [6 buckets])} from an entity_id,name,current,... CSV."""
    out = {}
    with open(path, newline="") as handle:
        for row in csv.DictReader(handle):
            entity = (row.get("entity_id") or "").strip()
            name = (row.get("name") or "").strip()
            if not entity and name.upper() == "TOTAL":
                continue
            key = entity or f"name:{name}"
            amounts = [money(row.get(b)) for b in BUCKETS]
            if key in out:
                amounts = [a + b for a, b in zip(out[key][1], amounts)]
            out[key] = (name, amounts)
    return out


def aging_diff(kind, ledger_path, qbo_path, explanations):
    ledger = read_aging(ledger_path)
    qbo = read_aging(qbo_path)
    # A QBO row without an id matches the ledger contact carrying its name.
    by_name = {name: key for key, (name, _) in ledger.items()}
    for key in [k for k in qbo if k.startswith("name:")]:
        target = by_name.get(key[5:])
        if target and target not in qbo:
            qbo[target] = qbo.pop(key)
    zero = [Decimal("0.00")] * len(BUCKETS)
    rows = []
    for key in sorted(set(ledger) | set(qbo)):
        name = (ledger.get(key) or qbo.get(key))[0]
        ours = ledger.get(key, (name, zero))[1]
        theirs = qbo.get(key, (name, zero))[1]
        if ours == theirs:
            continue
        delta = ours[-1] - theirs[-1]
        rows.append(
            {
                "contact": key,
                "name": name,
                "ledger": str(ours[-1]),
                "qbo": str(theirs[-1]),
                "delta": str(delta),
                "buckets_differ": [b for b, x, y in zip(BUCKETS, ours, theirs) if x != y],
                "explained_by": explain(explanations, kind, key, delta),
            }
        )
    return {
        "ledger_csv": str(ledger_path),
        "qbo_csv": str(qbo_path),
        "ledger_total": str(sum((v[1][-1] for v in ledger.values()), Decimal("0.00"))),
        "qbo_total": str(sum((v[1][-1] for v in qbo.values()), Decimal("0.00"))),
        "contacts": len(set(ledger) | set(qbo)),
        "differing": len(rows),
        "unexplained": sum(1 for r in rows if not r["explained_by"]),
        "rows": rows,
    }


# -- steps ------------------------------------------------------------------


def read_steps(path):
    steps = []
    try:
        lines = pathlib.Path(path).read_text().splitlines()
    except FileNotFoundError:
        return steps
    for line in lines:
        parts = line.split("\t")
        if len(parts) < 5:
            continue
        name, status, code, seconds, log = parts[:5]
        steps.append(
            {
                "name": name,
                "status": status,
                "exit": int(code) if code.lstrip("-").isdigit() else None,
                "seconds": int(seconds) if seconds.isdigit() else None,
                "log": log,
                "detail": parts[5] if len(parts) > 5 else "",
            }
        )
    return steps


# -- the night --------------------------------------------------------------


def decide(run_dir, as_of, window_from, company, explained_path, decisions_path):
    run_dir = pathlib.Path(run_dir)
    known = decision_ids(decisions_path)
    explanations, invalid = load_explanations(explained_path, known)
    steps = read_steps(run_dir / "steps.tsv")
    daemon = read_json(run_dir / "daemon.json", {"alive": None, "pids": []})
    replica = read_json(run_dir / "freshness.json")

    report = {
        "schema": 1,
        "company": company,
        "as_of": as_of,
        "from": window_from or None,
        "started_at": (read_json(run_dir / "run.json", {}) or {}).get("started_at"),
        "finished_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "verdict": "green",
        "exit_code": EXIT_GREEN,
        "reasons": [],
        "steps": steps,
        "daemon": daemon,
        "replica": replica,
        "tb": None,
        "rejected": {"count": 0, "documents": []},
        "aging": {"ar": None, "ap": None},
        "unexplained": 0,
        "explanations": {
            "file": str(explained_path) if explained_path else None,
            "loaded": len(explanations),
            "invalid": invalid,
            "unused": [],
        },
        "artifacts": {"run_dir": str(run_dir)},
    }
    codes = []

    if daemon.get("alive") is False:
        codes.append(EXIT_STALE)
        report["reasons"].append("qbo-local daemon is not running")
    if replica and not replica.get("fresh"):
        codes.append(EXIT_STALE)
        report["reasons"].append(replica.get("problem") or "replica stale")

    for step in steps:
        if step["status"] == "failed":
            codes.append(EXIT_STEP_FAILED)
            why = f" — {step['detail']}" if step["detail"] else ""
            report["reasons"].append(
                f"step {step['name']} failed (exit {step['exit']}){why}; log {step['log']}"
            )

    tb_csv = run_dir / "tbdiff.csv"
    if tb_csv.exists():
        report["tb"] = read_tbdiff(tb_csv, explanations)
        report["tb"]["text"] = str(run_dir / "tbdiff.txt")
        report["rejected"] = read_rejected(run_dir / "ledger-nightly.out")
        for row in report["tb"]["rows"]:
            if row["tier"] in ("must", "may") and money(row["delta"]) != 0 and not row["explained_by"]:
                report["reasons"].append(
                    f"trial balance: {row['account']} ({row['tier']} tier) off by {fmt(money(row['delta']))}, unexplained"
                )
        if report["rejected"]["count"]:
            report["reasons"].append(
                f"import rejected {report['rejected']['count']} document(s)"
            )

    for kind in ("ar", "ap"):
        ledger_csv, qbo_csv = run_dir / f"ledger-{kind}.csv", run_dir / f"qbo-{kind}.csv"
        if ledger_csv.exists() and qbo_csv.exists():
            diff = aging_diff(kind, ledger_csv, qbo_csv, explanations)
            report["aging"][kind] = diff
            if diff["unexplained"]:
                report["reasons"].append(
                    f"{kind.upper()} aging: {diff['unexplained']} contact(s) differ from QBO, unexplained"
                )

    if invalid:
        report["reasons"].append(
            f"{len(invalid)} explanation row(s) invalid and ignored (see explanations.invalid)"
        )

    unexplained = (report["tb"] or {}).get("unexplained", 0) + sum(
        (report["aging"][k] or {}).get("unexplained", 0) for k in ("ar", "ap")
    )
    report["unexplained"] = unexplained
    if unexplained or report["rejected"]["count"]:
        codes.append(EXIT_VARIANCE)
    report["explanations"]["unused"] = [
        {k: e[k] for k in ("kind", "key", "delta", "decision")} for e in explanations if not e["used"]
    ]

    # The most serious cause names the exit code: infrastructure first, since
    # a night with a dead daemon or a failed step proves nothing about the books.
    for code in (EXIT_STALE, EXIT_STEP_FAILED, EXIT_VARIANCE):
        if code in codes:
            report["exit_code"] = code
            report["verdict"] = "red"
            break
    return report


def render_markdown(report):
    out = []
    verdict = "GREEN" if report["verdict"] == "green" else "RED"
    window = f", replay from {report['from']}" if report["from"] else ""
    out.append(f"# Nightly parallel run — {report['as_of']}: {verdict}")
    out.append("")
    out.append(
        f"Company `{report['company']}`, as of {report['as_of']}{window}. "
        f"Exit code {report['exit_code']}. Finished {report['finished_at']}."
    )
    out.append("")
    if report["reasons"]:
        out.append("## Why it is red")
        out.append("")
        out.extend(f"- {r}" for r in report["reasons"])
        out.append("")

    out.append("## Replica")
    out.append("")
    daemon = report["daemon"] or {}
    alive = {True: "running", False: "NOT RUNNING", None: "not checked"}[daemon.get("alive")]
    pids = ", ".join(str(p) for p in daemon.get("pids") or [])
    out.append(f"- daemon: {alive}{f' (pid {pids})' if pids else ''}")
    replica = report["replica"]
    if replica and replica.get("oldest_cursor"):
        state = "fresh" if replica["fresh"] else "STALE"
        out.append(
            f"- oldest CDC cursor: {replica['oldest_cursor']} ({replica['oldest_entity']}), "
            f"{replica['age_minutes']} min old, limit {replica['max_age_minutes']} — {state}"
        )
        out.append(f"- snapshot: `{replica['snapshot']}`")
    elif replica:
        out.append(f"- cursor: {replica.get('problem')}")
    else:
        out.append("- cursor: not checked")
    out.append("")

    out.append("## Steps")
    out.append("")
    out.append("| step | status | exit | seconds |")
    out.append("|---|---|---:|---:|")
    for step in report["steps"]:
        out.append(f"| {step['name']} | {step['status']} | {step['exit']} | {step['seconds']} |")
    out.append("")

    tb = report["tb"]
    out.append("## Trial balance (ledger − QBO)")
    out.append("")
    if tb:
        out.append(
            f"{tb['must_failures']} must-tier and {tb['may_differ']} may-differ line(s) non-zero, "
            f"{tb['unexplained']} unexplained. {report['rejected']['count']} document(s) rejected."
        )
        out.append("")
        out.append("| account | tier | ledger | QBO | delta | explained by |")
        out.append("|---|---|---:|---:|---:|---|")
        for row in tb["rows"]:
            if row["tier"] == "skip":
                continue
            delta = money(row["delta"])
            flag = row["explained_by"] or ("**UNEXPLAINED**" if delta != 0 else "")
            out.append(
                f"| {row['account']} | {row['tier']} | {fmt(money(row['ledger']))} | "
                f"{fmt(money(row['qbo']))} | {fmt(delta)} | {flag} |"
            )
        for doc in report["rejected"]["documents"]:
            out.append(f"- rejected `{doc['document_id']}`: {doc['reason']}")
    else:
        out.append("Not produced this run.")
    out.append("")

    for kind, title in (("ar", "AR aging"), ("ap", "AP aging")):
        diff = report["aging"][kind]
        out.append(f"## {title} by contact")
        out.append("")
        if not diff:
            out.append("Not produced this run.")
            out.append("")
            continue
        out.append(
            f"Ledger {fmt(money(diff['ledger_total']))}, QBO {fmt(money(diff['qbo_total']))}; "
            f"{diff['contacts']} contact(s), {diff['differing']} differ, {diff['unexplained']} unexplained."
        )
        if diff["rows"]:
            out.append("")
            out.append("| contact | name | ledger | QBO | delta | buckets | explained by |")
            out.append("|---|---|---:|---:|---:|---|---|")
            for row in diff["rows"]:
                out.append(
                    f"| {row['contact']} | {row['name']} | {fmt(money(row['ledger']))} | "
                    f"{fmt(money(row['qbo']))} | {fmt(money(row['delta']))} | "
                    f"{', '.join(row['buckets_differ'])} | {row['explained_by'] or '**UNEXPLAINED**'} |"
                )
        out.append("")

    exp = report["explanations"]
    if exp["invalid"] or exp["unused"]:
        out.append("## Explanations")
        out.append("")
        for entry in exp["invalid"]:
            out.append(f"- invalid, ignored: {entry['problem']}")
        for entry in exp["unused"]:
            out.append(
                f"- unused tonight: {entry['kind']} {entry['key']} {entry['delta']} ({entry['decision']})"
            )
        out.append("")
    out.append(f"Run files: `{report['artifacts']['run_dir']}`")
    out.append("")
    return "\n".join(out)


def prune(report_dir, keep):
    """Keep the newest `keep` nights: their .md/.json and runs/<date>/."""
    report_dir = pathlib.Path(report_dir)
    dates = sorted(
        {m.group(1) for p in report_dir.iterdir() if (m := REPORT_GLOB.match(p.name))}
    )
    removed = []
    for date in dates[: max(0, len(dates) - keep)]:
        for suffix in ("md", "json"):
            path = report_dir / f"nightly-{date}.{suffix}"
            if path.exists():
                path.unlink()
                removed.append(str(path))
        run = report_dir / "runs" / date
        if run.is_dir():
            for child in sorted(run.rglob("*"), reverse=True):
                child.rmdir() if child.is_dir() else child.unlink()
            run.rmdir()
            removed.append(str(run))
    return removed


def finish(args):
    report = decide(
        args.run_dir, args.as_of, args.window_from, args.company, args.explained, args.decisions
    )
    report_dir = pathlib.Path(args.report_dir)
    report_dir.mkdir(parents=True, exist_ok=True)
    md_path = report_dir / f"nightly-{args.as_of}.md"
    json_path = report_dir / f"nightly-{args.as_of}.json"
    report["artifacts"].update(markdown=str(md_path), json=str(json_path))
    json_path.write_text(json.dumps(report, indent=2) + "\n")
    md_path.write_text(render_markdown(report))
    pruned = prune(report_dir, args.keep) if args.keep else []
    headline = f"nightly {args.as_of}: {report['verdict'].upper()} (exit {report['exit_code']})"
    print(headline)
    for reason in report["reasons"]:
        print(f"  - {reason}")
    print(f"report {md_path}")
    for path in pruned:
        print(f"pruned {path}")
    return report["exit_code"]


def main(argv=None):
    parser = argparse.ArgumentParser(prog="nightly-verdict.py")
    sub = parser.add_subparsers(dest="command", required=True)

    fresh = sub.add_parser("freshness")
    fresh.add_argument("--replica", required=True)
    fresh.add_argument("--realm", required=True)
    fresh.add_argument("--max-age-min", type=int, required=True)
    fresh.add_argument("--out", required=True)
    fresh.add_argument("--now")

    fin = sub.add_parser("finish")
    fin.add_argument("--run-dir", required=True)
    fin.add_argument("--report-dir", required=True)
    fin.add_argument("--as-of", required=True)
    fin.add_argument("--from", dest="window_from", default="")
    fin.add_argument("--company", default="aquamentor")
    fin.add_argument("--explained")
    fin.add_argument("--decisions")
    fin.add_argument("--keep", type=int, default=0)

    args = parser.parse_args(argv)
    if args.command == "freshness":
        now = parse_ts(args.now) if args.now else dt.datetime.now(dt.timezone.utc)
        result = freshness(args.replica, args.realm, args.max_age_min, now)
        pathlib.Path(args.out).write_text(json.dumps(result, indent=2) + "\n")
        print(result["problem"] or f"replica fresh: oldest cursor {result['age_minutes']} min old")
        return EXIT_GREEN if result["fresh"] else EXIT_STALE
    return finish(args)


if __name__ == "__main__":
    sys.exit(main())
