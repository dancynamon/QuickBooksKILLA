//! AR and AP aging from a ledger file, one CSV per book (`DIR/ar-<as-of>.csv`
//! and `DIR/ap-<as-of>.csv`), in the column shape `qbo-local report --name
//! ar-aging|ap-aging` writes, so the two can be diffed by contact. The
//! nightly run's aging diff (`tools/nightly.sh`, docs/NIGHTLY.md) reads these;
//! docs/PRACTICE-RUN.md used it as the `aging_csv` example. Reads only.
//!
//! ```text
//! ledger-aging --db LEDGER.db --company aquamentor --as-of 2026-07-31 --out DIR
//! ```

use std::path::PathBuf;

use chrono::NaiveDate;
use ledger::books::{self, AgingRow, Book};
use ledger::store::Ledger;
use ledger_core::Money;

fn row(r: &AgingRow) -> String {
    format!(
        "{},{},{},{},{},{},{},{}\n",
        r.contact_id,
        r.contact_name.replace(',', " "),
        Money::display(r.current),
        Money::display(r.days_1_30),
        Money::display(r.days_31_60),
        Money::display(r.days_61_90),
        Money::display(r.over_90),
        Money::display(r.total)
    )
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut company, mut as_of, mut out) = (None, None, None, None);
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = args.next().map(PathBuf::from),
            "--company" => company = args.next(),
            "--as-of" => as_of = args.next(),
            "--out" => out = args.next().map(PathBuf::from),
            other => return Err(format!("unknown flag {other}").into()),
        }
    }
    let ledger = Ledger::open(db.ok_or("missing --db")?)?;
    let company = company.ok_or("missing --company")?;
    let as_of = NaiveDate::parse_from_str(&as_of.ok_or("missing --as-of")?, "%Y-%m-%d")?;
    let out = out.ok_or("missing --out")?;
    std::fs::create_dir_all(&out)?;
    for (book, tag) in [(Book::Receivable, "ar"), (Book::Payable, "ap")] {
        let aging = books::aging(&ledger, &company, book, as_of)?;
        let mut csv = String::from("entity_id,name,current,1-30,31-60,61-90,over_90,total\n");
        for r in &aging.rows {
            csv.push_str(&row(r));
        }
        std::fs::write(out.join(format!("{tag}-{as_of}.csv")), csv)?;
        println!(
            "{tag} {as_of}: {} contacts, total {}",
            aging.rows.len(),
            Money::display(aging.totals.total)
        );
    }
    Ok(())
}
