//! `ledger` — the subcommand binary. `LEDGER-DESIGN.md`, `apps/ledger/src/pipeline.rs`.
//!
//! No `clap`: `std::env::args` and a hand-rolled [`parse`], matching
//! `apps/qbo-local/src/main.rs`'s no-new-dependencies discipline and its
//! usage-error/exit-code shape.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;
use std::str::FromStr;

use chrono::{NaiveDate, Utc};
use rust_decimal::Decimal;

use ledger::import::ImportOptions;
use ledger::pipeline;
use ledger::report::{self, BalanceSheet, Pnl, QboTbRow, SalesTaxLines, TierRules, TrialBalance};
use ledger::store::Ledger;
use ledger_core::Money;
use qbo_local::domain::RealmId;
use qbo_local::store::Store;

const SNAPSHOT_PREFIX: &str = "tb-";
const SNAPSHOT_SUFFIX: &str = ".csv";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    let command = match parse(&args) {
        Ok(command) => command,
        Err(usage_error) => {
            eprint!("{usage_error}");
            return ExitCode::from(2);
        }
    };

    match execute(command) {
        Ok(code) => code,
        Err(CommandError::Runtime(message)) => {
            eprintln!("{message}");
            ExitCode::from(1)
        }
    }
}

// ---------------------------------------------------------------------------
// Command line
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
enum Command {
    Init {
        db: PathBuf,
        company: String,
        name: String,
        realm: Option<String>,
    },
    Import {
        db: PathBuf,
        company: String,
        replica: PathBuf,
        realm: RealmId,
        from: Option<NaiveDate>,
        to: Option<NaiveDate>,
    },
    Tb {
        db: PathBuf,
        company: String,
        as_of: NaiveDate,
    },
    Pnl {
        db: PathBuf,
        company: String,
        from: NaiveDate,
        to: NaiveDate,
    },
    Bs {
        db: PathBuf,
        company: String,
        as_of: NaiveDate,
    },
    Tax {
        db: PathBuf,
        company: String,
        year: i32,
        quarter: u32,
        rate: Decimal,
    },
    Close {
        db: PathBuf,
        company: String,
        period_end: NaiveDate,
        note: String,
    },
    Reopen {
        db: PathBuf,
        company: String,
        period_end: NaiveDate,
        note: String,
    },
    TbDiff {
        db: PathBuf,
        company: String,
        as_of: NaiveDate,
        qbo_csv: PathBuf,
    },
    Opening {
        db: PathBuf,
        company: String,
        as_of: NaiveDate,
        qbo_csv: PathBuf,
    },
    Boundary {
        db: PathBuf,
        company: String,
        replica: PathBuf,
        realm: RealmId,
        snapshots: PathBuf,
    },
}

const DEFAULT_TAX_RATE: &str = "0.06625";

const USAGE: &str = "\
ledger — the double-entry book that replaces QuickBooks

USAGE:
    ledger init    --db PATH --company ID --name \"Display Name\" [--realm ID]
    ledger import  --db PATH --company ID --replica PATH --realm ID
                   [--from YYYY-MM-DD] [--to YYYY-MM-DD]
    ledger tb      --db PATH --company ID --as-of YYYY-MM-DD
    ledger pnl     --db PATH --company ID --from YYYY-MM-DD --to YYYY-MM-DD
    ledger bs      --db PATH --company ID --as-of YYYY-MM-DD
    ledger tax     --db PATH --company ID --year YYYY --quarter 1..4 [--rate R]
    ledger close   --db PATH --company ID --period-end YYYY-MM-DD --note \"...\"
    ledger reopen  --db PATH --company ID --period-end YYYY-MM-DD --note \"...\"
    ledger tbdiff  --db PATH --company ID --as-of YYYY-MM-DD --qbo-csv PATH
    ledger opening --db PATH --company ID --as-of YYYY-MM-DD --qbo-csv PATH
    ledger boundary --db PATH --company ID --replica PATH --realm ID
                   --snapshots DIR
    ledger --help

SUBCOMMANDS:
    init      Open (creating if absent) a ledger file and seed one company's
              chart and classes (§2, §3). Idempotent: re-running it for the
              same company id updates the display name and realm and leaves
              every already-seeded account and class alone.
    import    Import a qbo-local replica through post() into this ledger
              (§6), printing the pipeline report.
    tb        Print the trial balance as of a date (§7).
    pnl       Print income, COGS and expense between two dates (§1, §9 A).
    bs        Print the balance sheet as of a date (§7), current-year net
              income folded into equity.
    tax       Print the §9 Sales Tax Liability Report for one quarter, plus
              the ST-50 line mapping.
    close     Close a period through the given date (§5). Never quiet.
    reopen    Reopen a closed period (§5). Never quiet; always logged.
    tbdiff    Diff this ledger's trial balance against a QBO trial balance
              CSV (qbo_account_id,name,balance; first line is a header) and
              print the §7 report.
    opening   Apply (or idempotently skip/replace) the §6 opening balance
              entry as of a date, from a QBO trial balance CSV of the same
              shape tbdiff reads.
    boundary  Walk the §6 boundary-year procedure against a directory of
              QBO trial balance CSVs named tb-YYYY.csv, one per year, and
              print the per-year agreement table and the boundary year.

FLAGS:
    --db PATH            Path to the SQLite ledger file.
    --company ID         Company id (\"aquamentor\" or \"waterline\").
    --name \"Name\"         Company display name (init only).
    --realm ID            QBO realm id (init: optional; import/boundary: required).
    --replica PATH        Path to the qbo-local replica file (import, boundary).
    --from YYYY-MM-DD     Start date (import: optional; pnl: required).
    --to YYYY-MM-DD       End date (import: optional; pnl: required).
    --as-of YYYY-MM-DD    As-of date (tb, bs, tbdiff, opening).
    --year YYYY           Calendar year (tax only).
    --quarter 1..4        Calendar quarter (tax only).
    --rate R              Sales tax rate as a decimal fraction, default
                           0.06625 (tax only).
    --period-end YYYY-MM-DD  The period's last day (close, reopen).
    --note \"...\"          Required note (close, reopen).
    --qbo-csv PATH        CSV of qbo_account_id,name,balance (tbdiff, opening).
    --snapshots DIR       Directory of tb-YYYY.csv files, same CSV shape as
                           --qbo-csv (boundary only).

EXIT CODES:
    0   ok
    1   runtime error, or tbdiff found a must-match failure (§7)
    2   usage error (this message)
";

#[derive(Clone, Debug, PartialEq, Eq)]
struct UsageError {
    reason: Option<String>,
}

impl UsageError {
    fn usage() -> Self {
        UsageError { reason: None }
    }

    fn new(reason: impl Into<String>) -> Self {
        UsageError {
            reason: Some(reason.into()),
        }
    }

    fn missing_flag(flag: &str) -> Self {
        Self::new(format!("missing required flag {flag}"))
    }

    fn missing_value(flag: &str) -> Self {
        Self::new(format!("{flag} requires a value"))
    }

    fn unknown_flag(flag: &str) -> Self {
        Self::new(format!("unknown flag {flag}"))
    }

    fn unknown_subcommand(name: &str) -> Self {
        Self::new(format!("unknown subcommand {name:?}"))
    }

    fn invalid_value(flag: &str, value: &str) -> Self {
        Self::new(format!("invalid value for {flag}: {value:?}"))
    }
}

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if let Some(reason) = &self.reason {
            writeln!(f, "error: {reason}")?;
            writeln!(f)?;
        }
        write!(f, "{USAGE}")
    }
}

type Args<'a> = std::slice::Iter<'a, String>;

fn next_value(args: &mut Args<'_>, flag: &str) -> Result<String, UsageError> {
    args.next()
        .cloned()
        .ok_or_else(|| UsageError::missing_value(flag))
}

fn parse_date(raw: &str, flag: &str) -> Result<NaiveDate, UsageError> {
    NaiveDate::parse_from_str(raw, "%Y-%m-%d").map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_realm(raw: &str) -> Result<RealmId, UsageError> {
    RealmId::parse(raw).map_err(|_| UsageError::invalid_value("--realm", raw))
}

fn parse_i32(raw: &str, flag: &str) -> Result<i32, UsageError> {
    raw.parse()
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_u32(raw: &str, flag: &str) -> Result<u32, UsageError> {
    raw.parse()
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_decimal(raw: &str, flag: &str) -> Result<Decimal, UsageError> {
    Decimal::from_str(raw).map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse(args: &[String]) -> Result<Command, UsageError> {
    let mut args = args.iter();
    let Some(subcommand) = args.next() else {
        return Err(UsageError::usage());
    };

    match subcommand.as_str() {
        "--help" | "-h" => Err(UsageError::usage()),
        "init" => parse_init(args),
        "import" => parse_import(args),
        "tb" => parse_tb(args),
        "pnl" => parse_pnl(args),
        "bs" => parse_bs(args),
        "tax" => parse_tax(args),
        "close" => parse_close(args),
        "reopen" => parse_reopen(args),
        "tbdiff" => parse_tbdiff(args),
        "opening" => parse_opening(args),
        "boundary" => parse_boundary(args),
        other => Err(UsageError::unknown_subcommand(other)),
    }
}

fn parse_init(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut name = None;
    let mut realm = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--name" => name = Some(next_value(&mut args, "--name")?),
            "--realm" => realm = Some(next_value(&mut args, "--realm")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Init {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        name: name.ok_or_else(|| UsageError::missing_flag("--name"))?,
        realm,
    })
}

fn parse_import(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut replica = None;
    let mut realm = None;
    let mut from = None;
    let mut to = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--replica" => replica = Some(PathBuf::from(next_value(&mut args, "--replica")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--from" => from = Some(parse_date(&next_value(&mut args, "--from")?, "--from")?),
            "--to" => to = Some(parse_date(&next_value(&mut args, "--to")?, "--to")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Import {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        replica: replica.ok_or_else(|| UsageError::missing_flag("--replica"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        from,
        to,
    })
}

fn parse_tb(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut as_of = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--as-of" => as_of = Some(parse_date(&next_value(&mut args, "--as-of")?, "--as-of")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Tb {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        as_of: as_of.ok_or_else(|| UsageError::missing_flag("--as-of"))?,
    })
}

fn parse_pnl(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut from = None;
    let mut to = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--from" => from = Some(parse_date(&next_value(&mut args, "--from")?, "--from")?),
            "--to" => to = Some(parse_date(&next_value(&mut args, "--to")?, "--to")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Pnl {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        from: from.ok_or_else(|| UsageError::missing_flag("--from"))?,
        to: to.ok_or_else(|| UsageError::missing_flag("--to"))?,
    })
}

fn parse_bs(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut as_of = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--as-of" => as_of = Some(parse_date(&next_value(&mut args, "--as-of")?, "--as-of")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Bs {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        as_of: as_of.ok_or_else(|| UsageError::missing_flag("--as-of"))?,
    })
}

fn parse_tax(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut year = None;
    let mut quarter = None;
    let mut rate = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--year" => year = Some(parse_i32(&next_value(&mut args, "--year")?, "--year")?),
            "--quarter" => {
                quarter = Some(parse_u32(
                    &next_value(&mut args, "--quarter")?,
                    "--quarter",
                )?)
            }
            "--rate" => rate = Some(parse_decimal(&next_value(&mut args, "--rate")?, "--rate")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    let quarter = quarter.ok_or_else(|| UsageError::missing_flag("--quarter"))?;
    if !(1..=4).contains(&quarter) {
        return Err(UsageError::invalid_value("--quarter", &quarter.to_string()));
    }
    Ok(Command::Tax {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        year: year.ok_or_else(|| UsageError::missing_flag("--year"))?,
        quarter,
        rate: rate.unwrap_or_else(|| Decimal::from_str(DEFAULT_TAX_RATE).expect("valid literal")),
    })
}

fn parse_close(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut period_end = None;
    let mut note = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--period-end" => {
                period_end = Some(parse_date(
                    &next_value(&mut args, "--period-end")?,
                    "--period-end",
                )?)
            }
            "--note" => note = Some(next_value(&mut args, "--note")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Close {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        period_end: period_end.ok_or_else(|| UsageError::missing_flag("--period-end"))?,
        note: note.ok_or_else(|| UsageError::missing_flag("--note"))?,
    })
}

fn parse_reopen(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut period_end = None;
    let mut note = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--period-end" => {
                period_end = Some(parse_date(
                    &next_value(&mut args, "--period-end")?,
                    "--period-end",
                )?)
            }
            "--note" => note = Some(next_value(&mut args, "--note")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Reopen {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        period_end: period_end.ok_or_else(|| UsageError::missing_flag("--period-end"))?,
        note: note.ok_or_else(|| UsageError::missing_flag("--note"))?,
    })
}

fn parse_tbdiff(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut as_of = None;
    let mut qbo_csv = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--as-of" => as_of = Some(parse_date(&next_value(&mut args, "--as-of")?, "--as-of")?),
            "--qbo-csv" => qbo_csv = Some(PathBuf::from(next_value(&mut args, "--qbo-csv")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::TbDiff {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        as_of: as_of.ok_or_else(|| UsageError::missing_flag("--as-of"))?,
        qbo_csv: qbo_csv.ok_or_else(|| UsageError::missing_flag("--qbo-csv"))?,
    })
}

fn parse_opening(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut as_of = None;
    let mut qbo_csv = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--as-of" => as_of = Some(parse_date(&next_value(&mut args, "--as-of")?, "--as-of")?),
            "--qbo-csv" => qbo_csv = Some(PathBuf::from(next_value(&mut args, "--qbo-csv")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Opening {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        as_of: as_of.ok_or_else(|| UsageError::missing_flag("--as-of"))?,
        qbo_csv: qbo_csv.ok_or_else(|| UsageError::missing_flag("--qbo-csv"))?,
    })
}

fn parse_boundary(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut replica = None;
    let mut realm = None;
    let mut snapshots = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--replica" => replica = Some(PathBuf::from(next_value(&mut args, "--replica")?)),
            "--realm" => realm = Some(parse_realm(&next_value(&mut args, "--realm")?)?),
            "--snapshots" => snapshots = Some(PathBuf::from(next_value(&mut args, "--snapshots")?)),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::Boundary {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        replica: replica.ok_or_else(|| UsageError::missing_flag("--replica"))?,
        realm: realm.ok_or_else(|| UsageError::missing_flag("--realm"))?,
        snapshots: snapshots.ok_or_else(|| UsageError::missing_flag("--snapshots"))?,
    })
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

enum CommandError {
    Runtime(String),
}

fn runtime<E: std::fmt::Display>(error: E) -> CommandError {
    CommandError::Runtime(error.to_string())
}

fn ok((): ()) -> ExitCode {
    ExitCode::SUCCESS
}

fn execute(command: Command) -> Result<ExitCode, CommandError> {
    match command {
        Command::Init {
            db,
            company,
            name,
            realm,
        } => run_init(&db, &company, &name, realm.as_deref()).map(ok),
        Command::Import {
            db,
            company,
            replica,
            realm,
            from,
            to,
        } => run_import(&db, &company, &replica, &realm, from, to).map(ok),
        Command::Tb { db, company, as_of } => run_tb(&db, &company, as_of).map(ok),
        Command::Pnl {
            db,
            company,
            from,
            to,
        } => run_pnl(&db, &company, from, to).map(ok),
        Command::Bs { db, company, as_of } => run_bs(&db, &company, as_of).map(ok),
        Command::Tax {
            db,
            company,
            year,
            quarter,
            rate,
        } => run_tax(&db, &company, year, quarter, rate).map(ok),
        Command::Close {
            db,
            company,
            period_end,
            note,
        } => run_close(&db, &company, period_end, &note).map(ok),
        Command::Reopen {
            db,
            company,
            period_end,
            note,
        } => run_reopen(&db, &company, period_end, &note).map(ok),
        Command::TbDiff {
            db,
            company,
            as_of,
            qbo_csv,
        } => run_tbdiff(&db, &company, as_of, &qbo_csv),
        Command::Opening {
            db,
            company,
            as_of,
            qbo_csv,
        } => run_opening(&db, &company, as_of, &qbo_csv).map(ok),
        Command::Boundary {
            db,
            company,
            replica,
            realm,
            snapshots,
        } => run_boundary(&db, &company, &replica, &realm, &snapshots).map(ok),
    }
}

fn actor() -> String {
    env::var("USER").unwrap_or_else(|_| "dan".to_string())
}

// -- init ---------------------------------------------------------------

fn run_init(
    db: &std::path::Path,
    company: &str,
    name: &str,
    realm: Option<&str>,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    ledger
        .create_company(company, name, realm, Utc::now())
        .map_err(runtime)?;
    println!("created company {company} ({name:?}) in {}", db.display());
    Ok(())
}

// -- import ---------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_import(
    db: &std::path::Path,
    company: &str,
    replica: &std::path::Path,
    realm: &RealmId,
    from: Option<NaiveDate>,
    to: Option<NaiveDate>,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let store = Store::open(replica).map_err(runtime)?;
    let options = ImportOptions { from, to };
    let report = pipeline::import_replica(&ledger, &store, realm, company, &options, Utc::now())
        .map_err(runtime)?;
    print!("{}", report.render_text());
    Ok(())
}

// -- tb ---------------------------------------------------------------------

fn run_tb(db: &std::path::Path, company: &str, as_of: NaiveDate) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let tb = report::trial_balance(&ledger, company, as_of).map_err(runtime)?;
    print!("{}", render_trial_balance(&tb, company, as_of));
    Ok(())
}

fn render_trial_balance(tb: &TrialBalance, company: &str, as_of: NaiveDate) -> String {
    let mut out = String::new();
    out.push_str(&format!("TRIAL BALANCE   {company}   as of {as_of}\n\n"));
    out.push_str(&format!(
        "{:<8}{:<32}{:>14}{:>14}{:>14}\n",
        "acct", "name", "debit", "credit", "balance"
    ));
    for row in &tb.rows {
        if row.debit.is_zero() && row.credit.is_zero() {
            continue;
        }
        out.push_str(&format!(
            "{:<8}{:<32}{:>14}{:>14}{:>14}\n",
            row.number,
            row.name,
            fmt_money(row.debit),
            fmt_money(row.credit),
            fmt_money(row.balance),
        ));
    }
    out.push_str(&format!(
        "\nTOTAL{:<35}{:>14}{:>14}\n",
        "",
        fmt_money(tb.total_debits),
        fmt_money(tb.total_credits),
    ));
    if tb.total_debits == tb.total_credits {
        out.push_str("balanced\n");
    } else {
        out.push_str("*** DOES NOT BALANCE ***\n");
    }
    if !tb.wrong_side.is_empty() {
        out.push_str("\naccounts on the wrong side of their normal balance:\n");
        for account in &tb.wrong_side {
            out.push_str(&format!("  - {}\n", account.0));
        }
    }
    out
}

// -- pnl ----------------------------------------------------------------

fn run_pnl(
    db: &std::path::Path,
    company: &str,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let pnl = report::profit_and_loss(&ledger, company, from, to).map_err(runtime)?;
    print!("{}", render_pnl(&pnl, company, from, to));
    Ok(())
}

fn render_pnl(pnl: &Pnl, company: &str, from: NaiveDate, to: NaiveDate) -> String {
    let mut out = String::new();
    out.push_str(&format!("PROFIT AND LOSS   {company}   {from} .. {to}\n\n"));
    out.push_str(&format!(
        "  income          : {}\n",
        fmt_money(pnl.income.total)
    ));
    out.push_str(&format!(
        "  COGS            : {}\n",
        fmt_money(pnl.cogs.total)
    ));
    out.push_str(&format!(
        "  gross margin    : {}\n",
        fmt_money(pnl.gross_margin)
    ));
    out.push_str(&format!(
        "  expense         : {}\n",
        fmt_money(pnl.expense.total)
    ));
    out.push_str(&format!(
        "  net income      : {}\n",
        fmt_money(pnl.net_income)
    ));
    out
}

// -- bs -----------------------------------------------------------------

fn run_bs(db: &std::path::Path, company: &str, as_of: NaiveDate) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let bs = report::balance_sheet(&ledger, company, as_of).map_err(runtime)?;
    print!("{}", render_bs(&bs, company, as_of));
    Ok(())
}

fn render_bs(bs: &BalanceSheet, company: &str, as_of: NaiveDate) -> String {
    let mut out = String::new();
    out.push_str(&format!("BALANCE SHEET   {company}   as of {as_of}\n\n"));
    out.push_str("ASSETS\n");
    for row in &bs.assets.rows {
        out.push_str(&format!(
            "  {:<40}{:>14}\n",
            row.name,
            fmt_money(row.balance)
        ));
    }
    out.push_str(&format!(
        "  {:<40}{:>14}\n\n",
        "TOTAL ASSETS",
        fmt_money(bs.assets.total)
    ));
    out.push_str("LIABILITIES\n");
    for row in &bs.liabilities.rows {
        out.push_str(&format!(
            "  {:<40}{:>14}\n",
            row.name,
            fmt_money(row.balance)
        ));
    }
    out.push_str(&format!(
        "  {:<40}{:>14}\n\n",
        "TOTAL LIABILITIES",
        fmt_money(bs.liabilities.total)
    ));
    out.push_str("EQUITY\n");
    for row in &bs.equity.rows {
        out.push_str(&format!(
            "  {:<40}{:>14}\n",
            row.name,
            fmt_money(row.balance)
        ));
    }
    out.push_str(&format!(
        "  {:<40}{:>14}\n\n",
        "TOTAL EQUITY",
        fmt_money(bs.equity.total)
    ));
    out.push_str(&format!(
        "TOTAL LIABILITIES AND EQUITY          {:>14}\n",
        fmt_money(bs.total_liabilities_and_equity)
    ));
    out
}

// -- tax ------------------------------------------------------------------

fn run_tax(
    db: &std::path::Path,
    company: &str,
    year: i32,
    quarter: u32,
    rate: Decimal,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let lines =
        report::sales_tax_lines(&ledger, company, (year, quarter), rate).map_err(runtime)?;
    print!("{}", render_tax(&lines, company, year, quarter, rate));
    Ok(())
}

fn render_tax(
    lines: &SalesTaxLines,
    company: &str,
    year: i32,
    quarter: u32,
    rate: Decimal,
) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "SALES TAX LIABILITY   {company}   {year} Q{quarter}   rate {rate}\n\n"
    ));
    out.push_str(&format!(
        "  A  total income      : {}\n",
        fmt_money(lines.a_total_income)
    ));
    out.push_str(&format!(
        "  B  tax collected      : {}\n",
        fmt_money(lines.b_tax_collected)
    ));
    out.push_str(&format!(
        "  C  taxable sales      : {}\n",
        fmt_money(lines.c_taxable_sales)
    ));
    out.push_str(&format!(
        "  D  non-taxable sales  : {}\n",
        fmt_money(lines.d_nontaxable_sales)
    ));
    match lines.e_line_level_taxable {
        Some(e) => out.push_str(&format!("  E  taxable, line level: {}\n", fmt_money(e))),
        None => out.push_str("  E  taxable, line level: n/a\n"),
    }
    // Non-zero is expected when C is derived from a rounded B (D23), not
    // necessarily an error — it is what Dan checks before filing.
    if let Some(variance) = lines.variance {
        out.push_str(&format!(
            "     variance (E - C)  : {}\n",
            fmt_money(variance)
        ));
    }
    out.push_str("\nST-50 mapping:\n");
    for (line, amount) in lines.st50 {
        out.push_str(&format!("  line {line}: {}\n", fmt_money(amount)));
    }
    out.push_str("  lines 4-9: 0.00\n");
    out
}

// -- close / reopen ---------------------------------------------------------

fn run_close(
    db: &std::path::Path,
    company: &str,
    period_end: NaiveDate,
    note: &str,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    ledger
        .close_period(company, period_end, &actor(), note, Utc::now())
        .map_err(runtime)?;
    println!("closed {company} through {period_end}");
    Ok(())
}

fn run_reopen(
    db: &std::path::Path,
    company: &str,
    period_end: NaiveDate,
    note: &str,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    ledger
        .reopen_period(company, period_end, &actor(), note, Utc::now())
        .map_err(runtime)?;
    println!("reopened {company} at {period_end}: {note}");
    Ok(())
}

// -- tbdiff -------------------------------------------------------------

fn run_tbdiff(
    db: &std::path::Path,
    company: &str,
    as_of: NaiveDate,
    qbo_csv: &std::path::Path,
) -> Result<ExitCode, CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let tb = report::trial_balance(&ledger, company, as_of).map_err(runtime)?;
    let qbo_rows = read_qbo_csv(qbo_csv).map_err(runtime)?;
    let diff = report::tb_diff(&tb, &qbo_rows, &TierRules::default()).map_err(runtime)?;
    print!("{}", diff.render_text(company, as_of));
    if diff.must_failures.is_empty() {
        Ok(ExitCode::SUCCESS)
    } else {
        Ok(ExitCode::from(1))
    }
}

/// `qbo_account_id,name,balance`, dollars-and-cents text in the third
/// column. The first line is always a header and is skipped. No quoting —
/// an internal reconciliation format, not a general CSV reader.
fn read_qbo_csv(path: &std::path::Path) -> Result<Vec<QboTbRow>, String> {
    let raw = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut rows = Vec::new();
    for (line_no, line) in raw.lines().enumerate().skip(1) {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').collect();
        let [qbo_account_id, name, balance] = fields[..] else {
            return Err(format!(
                "{}:{}: expected qbo_account_id,name,balance, got {line:?}",
                path.display(),
                line_no + 1
            ));
        };
        let decimal = Decimal::from_str(balance.trim()).map_err(|e| {
            format!(
                "{}:{}: invalid balance {balance:?}: {e}",
                path.display(),
                line_no + 1
            )
        })?;
        let balance =
            ledger_core::round_money(decimal, ledger_core::RoundingPolicy::MirroredAmount)
                .map_err(|e| {
                    format!(
                        "{}:{}: balance {balance:?} is not a QBO-mirrored two-decimal amount: {e}",
                        path.display(),
                        line_no + 1
                    )
                })?;
        rows.push(QboTbRow {
            qbo_account_id: qbo_account_id.trim().to_string(),
            name: name.trim().to_string(),
            balance,
        });
    }
    Ok(rows)
}

// -- opening --------------------------------------------------------------

fn run_opening(
    db: &std::path::Path,
    company: &str,
    as_of: NaiveDate,
    qbo_csv: &std::path::Path,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let rows = read_qbo_csv(qbo_csv).map_err(runtime)?;
    let report = pipeline::apply_opening_balance(&ledger, company, as_of, &rows, Utc::now())
        .map_err(runtime)?;
    let verdict = if report.skipped_unchanged {
        "skipped (unchanged)"
    } else if report.replaced {
        "replaced"
    } else {
        "posted"
    };
    println!(
        "opening balance {} for {company} as of {as_of}: {verdict}",
        report.document_id
    );
    Ok(())
}

// -- boundary ---------------------------------------------------------------

fn run_boundary(
    db: &std::path::Path,
    company: &str,
    replica: &std::path::Path,
    realm: &RealmId,
    snapshots_dir: &std::path::Path,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let store = Store::open(replica).map_err(runtime)?;
    let snapshots = read_snapshots_dir(snapshots_dir).map_err(runtime)?;
    let report = pipeline::boundary_walk(
        &ledger,
        &store,
        realm,
        company,
        &snapshots,
        &TierRules::default(),
        Utc::now(),
    )
    .map_err(runtime)?;
    print!("{}", report.render_text(company));
    Ok(())
}

/// Every `tb-YYYY.csv` file directly inside `dir`, each read with
/// [`read_qbo_csv`] — the same CSV shape `tbdiff` and `opening` use — and
/// paired with the year its filename names. Any other file in the directory
/// is ignored rather than rejected, so a stray `README` or a `.DS_Store`
/// does not block the walk.
fn read_snapshots_dir(dir: &std::path::Path) -> Result<Vec<(i32, Vec<QboTbRow>)>, String> {
    let mut out = Vec::new();
    let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = entry.path();
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(year_str) = file_name
            .strip_prefix(SNAPSHOT_PREFIX)
            .and_then(|s| s.strip_suffix(SNAPSHOT_SUFFIX))
        else {
            continue;
        };
        let year: i32 = year_str
            .parse()
            .map_err(|_| format!("{}: not tb-YYYY.csv", path.display()))?;
        out.push((year, read_qbo_csv(&path)?));
    }
    out.sort_by_key(|(year, _)| *year);
    Ok(out)
}

fn fmt_money(amount: Money) -> String {
    let minor = amount.minor();
    let sign = if minor < 0 { "-" } else { "" };
    let abs = minor.unsigned_abs();
    format!("{sign}{}.{:02}", abs / 100, abs % 100)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> Vec<String> {
        raw.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_is_a_usage_error() {
        assert!(parse(&[]).is_err());
    }

    #[test]
    fn help_is_a_usage_error_with_no_reason() {
        let err = parse(&args(&["--help"])).unwrap_err();
        assert_eq!(err.reason, None);
    }

    #[test]
    fn unknown_subcommand_is_reported() {
        let err = parse(&args(&["frobnicate"])).unwrap_err();
        assert!(err.reason.unwrap().contains("frobnicate"));
    }

    #[test]
    fn init_parses_required_and_optional_flags() {
        let command = parse(&args(&[
            "init",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--name",
            "Aquamentor LLC",
            "--realm",
            "1234567890123456",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Init {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                name: "Aquamentor LLC".to_string(),
                realm: Some("1234567890123456".to_string()),
            }
        );
    }

    #[test]
    fn init_without_realm_is_fine() {
        let command = parse(&args(&[
            "init",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--name",
            "Aquamentor LLC",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Init {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                name: "Aquamentor LLC".to_string(),
                realm: None,
            }
        );
    }

    #[test]
    fn init_missing_name_is_a_usage_error() {
        let err = parse(&args(&[
            "init",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--name"));
    }

    #[test]
    fn import_parses_optional_date_window() {
        let command = parse(&args(&[
            "import",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--replica",
            "r.sqlite",
            "--realm",
            "1234567890123456",
            "--from",
            "2026-01-01",
            "--to",
            "2026-12-31",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Import {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                replica: PathBuf::from("r.sqlite"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                from: NaiveDate::from_ymd_opt(2026, 1, 1),
                to: NaiveDate::from_ymd_opt(2026, 12, 31),
            }
        );
    }

    #[test]
    fn import_missing_realm_is_a_usage_error() {
        let err = parse(&args(&[
            "import",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--replica",
            "r.sqlite",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--realm"));
    }

    #[test]
    fn import_bad_realm_is_an_invalid_value() {
        let err = parse(&args(&[
            "import",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--replica",
            "r.sqlite",
            "--realm",
            "not-numeric",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--realm"));
    }

    #[test]
    fn tb_requires_as_of() {
        let err = parse(&args(&[
            "tb",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--as-of"));
    }

    #[test]
    fn tb_parses() {
        let command = parse(&args(&[
            "tb",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--as-of",
            "2026-06-30",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Tb {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                as_of: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
            }
        );
    }

    #[test]
    fn tb_bad_date_is_an_invalid_value() {
        let err = parse(&args(&[
            "tb",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--as-of",
            "not-a-date",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--as-of"));
    }

    #[test]
    fn pnl_requires_from_and_to() {
        let err = parse(&args(&[
            "pnl",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--from",
            "2026-01-01",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--to"));
    }

    #[test]
    fn tax_defaults_the_rate() {
        let command = parse(&args(&[
            "tax",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--year",
            "2026",
            "--quarter",
            "2",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Tax {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                year: 2026,
                quarter: 2,
                rate: Decimal::from_str("0.06625").unwrap(),
            }
        );
    }

    #[test]
    fn tax_accepts_an_explicit_rate() {
        let command = parse(&args(&[
            "tax",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--year",
            "2026",
            "--quarter",
            "1",
            "--rate",
            "0.07",
        ]))
        .unwrap();
        let Command::Tax { rate, .. } = command else {
            panic!("expected Tax");
        };
        assert_eq!(rate, Decimal::from_str("0.07").unwrap());
    }

    #[test]
    fn tax_rejects_a_quarter_outside_1_to_4() {
        let err = parse(&args(&[
            "tax",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--year",
            "2026",
            "--quarter",
            "5",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--quarter"));
    }

    #[test]
    fn close_and_reopen_require_period_end_and_note() {
        let command = parse(&args(&[
            "close",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--period-end",
            "2026-06-30",
            "--note",
            "June close",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Close {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                period_end: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
                note: "June close".to_string(),
            }
        );

        let err = parse(&args(&[
            "reopen",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--period-end",
            "2026-06-30",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--note"));
    }

    #[test]
    fn tbdiff_parses() {
        let command = parse(&args(&[
            "tbdiff",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--as-of",
            "2026-06-30",
            "--qbo-csv",
            "qbo.csv",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::TbDiff {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                as_of: NaiveDate::from_ymd_opt(2026, 6, 30).unwrap(),
                qbo_csv: PathBuf::from("qbo.csv"),
            }
        );
    }

    #[test]
    fn opening_parses() {
        let command = parse(&args(&[
            "opening",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--as-of",
            "2023-12-31",
            "--qbo-csv",
            "tb-2023.csv",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Opening {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                as_of: NaiveDate::from_ymd_opt(2023, 12, 31).unwrap(),
                qbo_csv: PathBuf::from("tb-2023.csv"),
            }
        );
    }

    #[test]
    fn boundary_parses_and_requires_realm() {
        let command = parse(&args(&[
            "boundary",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--replica",
            "r.sqlite",
            "--realm",
            "1234567890123456",
            "--snapshots",
            "snaps",
        ]))
        .unwrap();
        assert_eq!(
            command,
            Command::Boundary {
                db: PathBuf::from("l.sqlite"),
                company: "aquamentor".to_string(),
                replica: PathBuf::from("r.sqlite"),
                realm: RealmId::parse("1234567890123456").unwrap(),
                snapshots: PathBuf::from("snaps"),
            }
        );

        let err = parse(&args(&[
            "boundary",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--replica",
            "r.sqlite",
            "--snapshots",
            "snaps",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--realm"));
    }

    #[test]
    fn an_unknown_flag_is_reported_with_its_own_name() {
        let err = parse(&args(&[
            "tb",
            "--db",
            "l.sqlite",
            "--company",
            "aquamentor",
            "--as-of",
            "2026-06-30",
            "--bogus",
        ]))
        .unwrap_err();
        assert!(err.reason.unwrap().contains("--bogus"));
    }

    #[test]
    fn read_qbo_csv_skips_the_header_and_parses_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("qbo.csv");
        fs::write(
            &path,
            "qbo_account_id,name,balance\n79,Accounts Receivable,1200.00\n35,Checking,500.50\n",
        )
        .unwrap();
        let rows = read_qbo_csv(&path).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].qbo_account_id, "79");
        assert_eq!(rows[0].balance, Money::from_minor(120_000));
        assert_eq!(rows[1].balance, Money::from_minor(50_050));
    }

    #[test]
    fn fmt_money_matches_report_rendering() {
        assert_eq!(fmt_money(Money::from_minor(-105)), "-1.05");
        assert_eq!(fmt_money(Money::from_minor(500)), "5.00");
        assert_eq!(fmt_money(Money::ZERO), "0.00");
    }
}
