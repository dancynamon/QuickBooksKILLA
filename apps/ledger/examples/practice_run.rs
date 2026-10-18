//! The practice parallel run (ROADMAP §E, docs/PRACTICE-RUN.md), as one
//! reproducible command against a scratch ledger file.
//!
//! It mimics the Q1 2027 run D29 describes, moved back two quarters: one
//! opening-balance entry from QBO's trial balance at `--opening`, then every
//! document dated in `--from ..= --to` replayed under the full posting gate.
//! D28's import-only fallback class covers only lines dated before `--from`,
//! so an unclassed line inside the window is rejected and counted, which is
//! the point.
//!
//! ```text
//! cargo run --release -p ledger --example practice_run -- \
//!     --db SCRATCH/ledger.db --replica SCRATCH/replica.db --realm ID \
//!     --opening 2026-06-30 --from 2026-07-01 --to 2026-10-16 \
//!     --qbo-dir SCRATCH/qbo --out SCRATCH/out \
//!     --as-of 2026-07-31 --as-of 2026-08-31 --as-of 2026-09-30 --as-of 2026-10-16
//! ```
//!
//! `--fallback-before DATE` is a diagnostic: it stretches the fallback class
//! over the window too, so the diff that is left is posting policy rather
//! than missing classes.
//!
//! `--qbo-dir` holds `tb-<date>.csv` files from `qbo-local report --name
//! trial-balance` for the opening date and every `--as-of`. Writes into
//! `--out`: `import.txt`, `rejected.csv`, `tbdiff-<date>.{txt,csv}`,
//! `tb-<date>.csv` (the ledger's own trial balance, per account) and
//! `aging-{ar,ap}-<date>.csv`. Never opens QBO, never writes to the replica's
//! outbox (the import runs with replaying set), and refuses to touch a
//! ledger file that already exists.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use chrono::{NaiveDate, Utc};
use ledger::books::{self, Book};
use ledger::chart;
use ledger::import::{ClassFallback, ImportOptions};
use ledger::pipeline;
use ledger::report::{self, QboTbRow, TierRules};
use ledger::store::Ledger;
use ledger::types::ClassId;
use ledger_core::Money;
use qbo_local::domain::RealmId;
use qbo_local::store::Store;

const COMPANY: &str = "aquamentor";

struct Args {
    db: PathBuf,
    replica: PathBuf,
    realm: String,
    opening: NaiveDate,
    from: NaiveDate,
    to: NaiveDate,
    qbo_dir: PathBuf,
    out: PathBuf,
    as_of: Vec<NaiveDate>,
    /// Diagnostic only: extend the fallback class to lines dated before this
    /// (instead of `--from`), to see what is left once class is out of the way.
    fallback_before: Option<NaiveDate>,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let (mut db, mut replica, mut realm, mut opening, mut from, mut to) =
        (None, None, None, None, None, None);
    let (mut qbo_dir, mut out, mut as_of, mut fallback_before) = (None, None, Vec::new(), None);
    let date = |raw: Option<String>, flag: &str| {
        raw.ok_or(format!("{flag} needs a value")).and_then(|raw| {
            NaiveDate::parse_from_str(&raw, "%Y-%m-%d").map_err(|_| format!("bad {flag}: {raw}"))
        })
    };
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = args.next().map(PathBuf::from),
            "--replica" => replica = args.next().map(PathBuf::from),
            "--realm" => realm = args.next(),
            "--opening" => opening = Some(date(args.next(), "--opening")?),
            "--from" => from = Some(date(args.next(), "--from")?),
            "--to" => to = Some(date(args.next(), "--to")?),
            "--qbo-dir" => qbo_dir = args.next().map(PathBuf::from),
            "--out" => out = args.next().map(PathBuf::from),
            "--as-of" => as_of.push(date(args.next(), "--as-of")?),
            "--fallback-before" => fallback_before = Some(date(args.next(), "--fallback-before")?),
            other => return Err(format!("unknown flag {other}")),
        }
    }
    let need = |flag: &str| format!("missing {flag}");
    Ok(Args {
        db: db.ok_or_else(|| need("--db"))?,
        replica: replica.ok_or_else(|| need("--replica"))?,
        realm: realm.ok_or_else(|| need("--realm"))?,
        opening: opening.ok_or_else(|| need("--opening"))?,
        from: from.ok_or_else(|| need("--from"))?,
        to: to.ok_or_else(|| need("--to"))?,
        qbo_dir: qbo_dir.ok_or_else(|| need("--qbo-dir"))?,
        out: out.ok_or_else(|| need("--out"))?,
        as_of,
        fallback_before,
    })
}

fn money(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

/// `qbo_account_id,name,balance`, the shape `qbo-local report` writes.
fn read_qbo_tb(path: &Path) -> Result<Vec<QboTbRow>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for line in raw.lines().skip(1).filter(|l| !l.trim().is_empty()) {
        let fields: Vec<&str> = line.split(',').collect();
        let [id, name, balance] = fields[..] else {
            return Err(format!("{}: bad row {line:?}", path.display()));
        };
        let value: rust_decimal::Decimal = balance
            .trim()
            .parse()
            .map_err(|e| format!("{balance:?}: {e}"))?;
        let minor = (value * rust_decimal::Decimal::from(100))
            .round()
            .try_into()
            .map_err(|_| format!("{balance:?} out of range"))?;
        rows.push(QboTbRow {
            qbo_account_id: id.to_string(),
            name: name.to_string(),
            balance: Money::from_minor(minor),
        });
    }
    Ok(rows)
}

fn csv_field(raw: &str) -> String {
    if raw.contains([',', '"', '\n']) {
        format!("\"{}\"", raw.replace('"', "\"\""))
    } else {
        raw.to_string()
    }
}

fn run(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    if args.db.exists() {
        return Err(format!(
            "{} exists; the practice run only builds a fresh ledger",
            args.db.display()
        )
        .into());
    }
    fs::create_dir_all(&args.out)?;
    let realm = RealmId::parse(args.realm.clone())?;
    let ledger = Ledger::open(&args.db)?;
    let replica = Store::open_read_only(&args.replica)?;
    ledger.create_company(COMPANY, "Aquamentor, Inc.", Some(&args.realm), Utc::now())?;

    let started = Instant::now();
    let options = ImportOptions {
        from: Some(args.from),
        to: Some(args.to),
        class_fallback: Some(ClassFallback {
            class: ClassId(chart::IMPORT_FALLBACK_CLASS.0.to_string()),
            before: args.fallback_before.unwrap_or(args.from),
        }),
        ..ImportOptions::default()
    };
    let import =
        pipeline::import_replica(&ledger, &replica, &realm, COMPANY, &options, Utc::now())?;
    let import_secs = started.elapsed().as_secs_f64();
    fs::write(args.out.join("import.txt"), import.render_text())?;
    let mut rejected = String::from("document_id,reason\n");
    for row in &import.rejected {
        rejected.push_str(&format!(
            "{},{}\n",
            csv_field(&row.document_id),
            csv_field(&row.reason)
        ));
    }
    fs::write(args.out.join("rejected.csv"), rejected)?;
    println!(
        "import {}..{}: {} translated, {} posted, {} non-posting, {} rejected, {:.1}s",
        args.from,
        args.to,
        import.translated,
        import.posted,
        import.non_posting,
        import.rejected.len(),
        import_secs
    );

    let started = Instant::now();
    let opening_rows = read_qbo_tb(&args.qbo_dir.join(format!("tb-{}.csv", args.opening)))?;
    let opening =
        pipeline::apply_opening_balance(&ledger, COMPANY, args.opening, &opening_rows, Utc::now())?;
    println!(
        "opening {} as of {}: {:.1}s",
        opening.document_id,
        args.opening,
        started.elapsed().as_secs_f64()
    );

    for &as_of in &args.as_of {
        let started = Instant::now();
        let tb = report::trial_balance(&ledger, COMPANY, as_of)?;
        let qbo = read_qbo_tb(&args.qbo_dir.join(format!("tb-{as_of}.csv")))?;
        let diff = report::tb_diff(&tb, &qbo, &TierRules::default())?;
        fs::write(
            args.out.join(format!("tbdiff-{as_of}.txt")),
            diff.render_text(COMPANY, as_of),
        )?;
        fs::write(
            args.out.join(format!("tbdiff-{as_of}.csv")),
            diff.render_csv(),
        )?;

        let mut per_account = String::from("number,source_ref,needs_mapping,name,balance\n");
        for row in tb.rows.iter().filter(|r| !r.balance.is_zero()) {
            per_account.push_str(&format!(
                "{},{},{},{},{}\n",
                row.number,
                row.source_ref.as_deref().unwrap_or(""),
                row.needs_mapping,
                csv_field(&row.name),
                money(row.balance)
            ));
        }
        fs::write(args.out.join(format!("tb-{as_of}.csv")), per_account)?;

        for (book, tag) in [(Book::Receivable, "ar"), (Book::Payable, "ap")] {
            let aging = books::aging(&ledger, COMPANY, book, as_of)?;
            let mut out = String::from("contact_id,name,total\n");
            for row in &aging.rows {
                out.push_str(&format!(
                    "{},{},{}\n",
                    row.contact_id,
                    csv_field(&row.contact_name),
                    money(row.total)
                ));
            }
            out.push_str(&format!(",TOTAL,{}\n", money(aging.totals.total)));
            fs::write(args.out.join(format!("aging-{tag}-{as_of}.csv")), out)?;
        }
        println!(
            "tbdiff {as_of}: {} must-tier failure(s), {} may-differ unexplained, {:.1}s",
            diff.must_failures.len(),
            diff.may_unexplained.len(),
            started.elapsed().as_secs_f64()
        );
    }
    Ok(())
}

fn main() -> std::process::ExitCode {
    match parse_args() {
        Ok(args) => match run(&args) {
            Ok(()) => std::process::ExitCode::SUCCESS,
            Err(err) => {
                eprintln!("practice_run: {err}");
                std::process::ExitCode::from(1)
            }
        },
        Err(usage) => {
            eprintln!("practice_run: {usage}");
            std::process::ExitCode::from(2)
        }
    }
}
