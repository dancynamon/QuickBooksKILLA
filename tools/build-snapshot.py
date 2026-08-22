#!/usr/bin/env python3
"""Build a local snapshot page from data pulled out of the live QuickBooks book.

Reads   .local/qbo/ar.json, .local/qbo/invoices.json
Writes  .local/snapshot.html

Nothing this touches is tracked by git — `.local/` is ignored in full. The
script is committed because it is code; the data it reads and writes never is.

To refresh the inputs, ask Claude in a session connected to QuickBooks:

    Refresh the QBO snapshot — pull A/R aging and recent invoices into
    .local/qbo/ and rebuild .local/snapshot.html

The pull goes through the QuickBooks MCP rather than this script, because it
needs OAuth credentials this script deliberately does not hold. Once qbo-local
can authenticate on its own (see HANDOFF.md), it replaces this step entirely
and the data stops being a snapshot.
"""

import json
import pathlib
import sys
from datetime import date

ROOT = pathlib.Path(__file__).resolve().parent.parent
QBO = ROOT / ".local" / "qbo"
OUT = ROOT / ".local" / "snapshot.html"

BUCKETS = [("c", "Current"), ("a", "1–30"), ("b", "31–60"), ("d", "61–90"), ("e", "91+")]


def load(name):
    path = QBO / name
    if not path.exists():
        sys.exit(
            f"missing {path.relative_to(ROOT)}\n"
            "Ask Claude to refresh the snapshot in a session connected to QuickBooks."
        )
    return json.loads(path.read_text())


def money(v):
    return f"{v:,.2f}"


def main():
    ar = load("ar.json")
    invoices = load("invoices.json")

    total = sum(x["t"] for x in ar)
    if not total:
        sys.exit("A/R extract has no balances — nothing to build.")
    current = sum(x["c"] for x in ar)
    overdue = total - current
    late = [x for x in ar if x["t"] - x["c"] > 0]

    ar_rows = "".join(
        "<tr><td class='trunc'>{n}</td>{cells}"
        "<td class='r num' style='font-weight:500'>{t}</td>"
        "<td>{state}</td></tr>".format(
            n=x["n"],
            cells="".join(
                f"<td class='r num'>{money(x[k]) if x[k] else '—'}</td>" for k, _ in BUCKETS
            ),
            t=money(x["t"]),
            state=(
                f"<span class='tag-crit'>{round((x['t'] - x['c']) / x['t'] * 100)}% late</span>"
                if x["t"] - x["c"] > 0
                else "<span class='dim'>current</span>"
            ),
        )
        for x in ar
    )

    inv_rows = "".join(
        "<tr><td class='mono'>{no}</td><td class='mono dim'>{d}</td>"
        "<td class='trunc'>{cu}</td><td class='mono dim'>{po}</td>"
        "<td class='r num'>{t}</td><td class='r num'>{b}</td><td>{state}</td></tr>".format(
            no=x["no"], d=x["d"], cu=x["cu"], po=x["po"] or "—",
            t=money(x["t"]),
            b="<span class='dim'>—</span>" if x["b"] <= 0 else money(x["b"]),
            state="<span class='tag-ok'>paid</span>" if x["b"] <= 0 else "<span class='tag-warn'>open</span>",
        )
        for x in invoices
    )

    template = (ROOT / "tools" / "snapshot-template.html").read_text()
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(
        template
        .replace("{{PULLED}}", date.today().isoformat())
        .replace("{{TOTAL}}", money(total))
        .replace("{{CURRENT}}", money(current))
        .replace("{{OVERDUE}}", money(overdue))
        .replace("{{OVERDUE_PCT}}", str(round(overdue / total * 100)))
        .replace("{{CUSTOMERS}}", str(len(ar)))
        .replace("{{LATE_COUNT}}", str(len(late)))
        .replace("{{AR_ROWS}}", ar_rows)
        .replace("{{INV_COUNT}}", str(len(invoices)))
        .replace("{{INV_BILLED}}", money(sum(x["t"] for x in invoices)))
        .replace("{{INV_OPEN}}", money(sum(x["b"] for x in invoices)))
        .replace("{{INV_WITH_PO}}", str(sum(1 for x in invoices if x["po"])))
        .replace("{{INV_ROWS}}", inv_rows)
    )
    print(f"wrote {OUT.relative_to(ROOT)} — {len(ar)} customers, {len(invoices)} invoices, ${money(total)} receivable")


if __name__ == "__main__":
    main()
