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

use ledger::bank::{self, MatchRules, NewStatement};
use ledger::import::ImportOptions;
use ledger::pipeline;
use ledger::report::{self, BalanceSheet, Pnl, QboTbRow, SalesTaxLines, TierRules, TrialBalance};
use ledger::store::{Ledger, LedgerError};
use ledger::types::{AccountId, ClassId};
use ledger_core::Money;
use qbo_local::domain::RealmId;
use qbo_local::store::Store;

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
    BankImport {
        db: PathBuf,
        company: String,
        account: String,
        file: PathBuf,
        format: BankFormat,
        profile: BankProfileName,
        opening: Money,
        closing: Money,
        from: NaiveDate,
        to: NaiveDate,
    },
    BankMatch {
        db: PathBuf,
        company: String,
        statement: String,
    },
    BankConfirm {
        db: PathBuf,
        company: String,
        line: String,
        account: String,
        class: Option<String>,
    },
    BankClose {
        db: PathBuf,
        company: String,
        statement: String,
    },
    BankStatus {
        db: PathBuf,
        company: String,
        statement: String,
    },
}

/// `ledger bank import --format`. §10: the files the bank and Chase already
/// generate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BankFormat {
    Csv,
    Ofx,
}

/// `ledger bank import --profile`, csv only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BankProfileName {
    Chase,
    Generic,
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
    ledger bank import  --db PATH --company ID --account NUM --file PATH
                        [--format csv|ofx] [--profile chase|generic]
                        --opening X --closing Y --from YYYY-MM-DD --to YYYY-MM-DD
    ledger bank match   --db PATH --company ID --statement ID
    ledger bank confirm --db PATH --company ID --line ID --account NUM [--class ID]
    ledger bank close   --db PATH --company ID --statement ID
    ledger bank status  --db PATH --company ID --statement ID
    ledger --help

SUBCOMMANDS:
    init      Open (creating if absent) a ledger file and seed one company's
              chart and classes (§2, §3). Not idempotent — an existing
              company id errors.
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
    bank import   Read a CSV or OFX/QFX statement export into `bank_lines`
                  (§10). A line whose (account, external_id) is already on
                  file is skipped, never duplicated.
    bank match    Run the §10 match rules over a statement's unmatched
                  lines: exact, then settlement, then a proposal.
    bank confirm  Post a confirmed proposal as a `BankLine` document to the
                  given account (and optional class), and mark the line
                  matched.
    bank close    The per-statement close (§10): opening plus matched lines
                  must equal closing, with nothing left unmatched.
    bank status   Print a statement's lines: matched, and open proposals.

FLAGS:
    --db PATH            Path to the SQLite ledger file.
    --company ID         Company id (\"aquamentor\" or \"waterline\").
    --name \"Name\"         Company display name (init only).
    --realm ID            QBO realm id (init: optional; import: required).
    --replica PATH        Path to the qbo-local replica file (import only).
    --from YYYY-MM-DD     Start date (import: optional; pnl: required).
    --to YYYY-MM-DD       End date (import: optional; pnl: required).
    --as-of YYYY-MM-DD    As-of date (tb, bs, tbdiff).
    --year YYYY           Calendar year (tax only).
    --quarter 1..4        Calendar quarter (tax only).
    --rate R              Sales tax rate as a decimal fraction, default
                           0.06625 (tax only).
    --period-end YYYY-MM-DD  The period's last day (close, reopen).
    --note \"...\"          Required note (close, reopen).
    --qbo-csv PATH        CSV of qbo_account_id,name,balance (tbdiff only).
    --account NUM         Bank/card account number, e.g. 1100 (bank import,
                          bank confirm).
    --file PATH           Statement export to read (bank import only).
    --format csv|ofx      Statement file format, default csv (bank import).
    --profile chase|generic  CSV column layout, default chase (bank import).
    --opening X           Statement opening balance (bank import only).
    --closing Y           Statement closing balance (bank import only).
    --statement ID        Bank statement id (bank match, close, status).
    --line ID             Bank line id (bank confirm only).
    --class ID            Class to post the confirmed line under (bank
                          confirm only; optional).

EXIT CODES:
    0   ok
    1   runtime error, tbdiff found a must-match failure (§7), or a bank
        statement does not close (§10)
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

fn parse_money(raw: &str, flag: &str) -> Result<Money, UsageError> {
    let decimal = parse_decimal(raw, flag)?;
    ledger_core::round_money(decimal, ledger_core::RoundingPolicy::MirroredAmount)
        .map_err(|_| UsageError::invalid_value(flag, raw))
}

fn parse_bank_format(raw: &str, flag: &str) -> Result<BankFormat, UsageError> {
    match raw {
        "csv" => Ok(BankFormat::Csv),
        "ofx" => Ok(BankFormat::Ofx),
        _ => Err(UsageError::invalid_value(flag, raw)),
    }
}

fn parse_bank_profile(raw: &str, flag: &str) -> Result<BankProfileName, UsageError> {
    match raw {
        "chase" => Ok(BankProfileName::Chase),
        "generic" => Ok(BankProfileName::Generic),
        _ => Err(UsageError::invalid_value(flag, raw)),
    }
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
        "bank" => parse_bank(args),
        other => Err(UsageError::unknown_subcommand(other)),
    }
}

fn parse_bank(mut args: Args<'_>) -> Result<Command, UsageError> {
    let Some(sub) = args.next() else {
        return Err(UsageError::new(
            "bank requires a subcommand (import, match, confirm, close, status)",
        ));
    };
    match sub.as_str() {
        "import" => parse_bank_import(args),
        "match" => parse_bank_match(args),
        "confirm" => parse_bank_confirm(args),
        "close" => parse_bank_close(args),
        "status" => parse_bank_status(args),
        other => Err(UsageError::unknown_subcommand(&format!("bank {other}"))),
    }
}

#[allow(clippy::too_many_lines)]
fn parse_bank_import(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut account = None;
    let mut file = None;
    let mut format = None;
    let mut profile = None;
    let mut opening = None;
    let mut closing = None;
    let mut from = None;
    let mut to = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--account" => account = Some(next_value(&mut args, "--account")?),
            "--file" => file = Some(PathBuf::from(next_value(&mut args, "--file")?)),
            "--format" => {
                format = Some(parse_bank_format(
                    &next_value(&mut args, "--format")?,
                    "--format",
                )?)
            }
            "--profile" => {
                profile = Some(parse_bank_profile(
                    &next_value(&mut args, "--profile")?,
                    "--profile",
                )?)
            }
            "--opening" => {
                opening = Some(parse_money(
                    &next_value(&mut args, "--opening")?,
                    "--opening",
                )?)
            }
            "--closing" => {
                closing = Some(parse_money(
                    &next_value(&mut args, "--closing")?,
                    "--closing",
                )?)
            }
            "--from" => from = Some(parse_date(&next_value(&mut args, "--from")?, "--from")?),
            "--to" => to = Some(parse_date(&next_value(&mut args, "--to")?, "--to")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::BankImport {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        account: account.ok_or_else(|| UsageError::missing_flag("--account"))?,
        file: file.ok_or_else(|| UsageError::missing_flag("--file"))?,
        format: format.unwrap_or(BankFormat::Csv),
        profile: profile.unwrap_or(BankProfileName::Chase),
        opening: opening.ok_or_else(|| UsageError::missing_flag("--opening"))?,
        closing: closing.ok_or_else(|| UsageError::missing_flag("--closing"))?,
        from: from.ok_or_else(|| UsageError::missing_flag("--from"))?,
        to: to.ok_or_else(|| UsageError::missing_flag("--to"))?,
    })
}

fn parse_bank_match(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut statement = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--statement" => statement = Some(next_value(&mut args, "--statement")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::BankMatch {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        statement: statement.ok_or_else(|| UsageError::missing_flag("--statement"))?,
    })
}

fn parse_bank_confirm(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut line = None;
    let mut account = None;
    let mut class = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--line" => line = Some(next_value(&mut args, "--line")?),
            "--account" => account = Some(next_value(&mut args, "--account")?),
            "--class" => class = Some(next_value(&mut args, "--class")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::BankConfirm {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        line: line.ok_or_else(|| UsageError::missing_flag("--line"))?,
        account: account.ok_or_else(|| UsageError::missing_flag("--account"))?,
        class,
    })
}

fn parse_bank_close(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut statement = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--statement" => statement = Some(next_value(&mut args, "--statement")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::BankClose {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        statement: statement.ok_or_else(|| UsageError::missing_flag("--statement"))?,
    })
}

fn parse_bank_status(mut args: Args<'_>) -> Result<Command, UsageError> {
    let mut db = None;
    let mut company = None;
    let mut statement = None;
    while let Some(flag) = args.next() {
        match flag.as_str() {
            "--db" => db = Some(PathBuf::from(next_value(&mut args, "--db")?)),
            "--company" => company = Some(next_value(&mut args, "--company")?),
            "--statement" => statement = Some(next_value(&mut args, "--statement")?),
            other => return Err(UsageError::unknown_flag(other)),
        }
    }
    Ok(Command::BankStatus {
        db: db.ok_or_else(|| UsageError::missing_flag("--db"))?,
        company: company.ok_or_else(|| UsageError::missing_flag("--company"))?,
        statement: statement.ok_or_else(|| UsageError::missing_flag("--statement"))?,
    })
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
        Command::BankImport {
            db,
            company,
            account,
            file,
            format,
            profile,
            opening,
            closing,
            from,
            to,
        } => run_bank_import(
            &db, &company, &account, &file, format, profile, opening, closing, from, to,
        )
        .map(ok),
        Command::BankMatch {
            db,
            company,
            statement,
        } => run_bank_match(&db, &company, &statement).map(ok),
        Command::BankConfirm {
            db,
            company,
            line,
            account,
            class,
        } => run_bank_confirm(&db, &company, &line, &account, class.as_deref()).map(ok),
        Command::BankClose {
            db,
            company,
            statement,
        } => run_bank_close(&db, &company, &statement),
        Command::BankStatus {
            db,
            company,
            statement,
        } => run_bank_status(&db, &company, &statement).map(ok),
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

// -- bank -----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn run_bank_import(
    db: &std::path::Path,
    company: &str,
    account: &str,
    file: &std::path::Path,
    format: BankFormat,
    profile: BankProfileName,
    opening: Money,
    closing: Money,
    from: NaiveDate,
    to: NaiveDate,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let text = fs::read_to_string(file).map_err(|e| runtime(format!("{}: {e}", file.display())))?;
    let lines = match format {
        BankFormat::Csv => {
            let csv_profile = match profile {
                BankProfileName::Chase => bank::CsvProfile::chase(),
                BankProfileName::Generic => bank::CsvProfile::generic(),
            };
            bank::parse_csv(&text, &csv_profile).map_err(runtime)?
        }
        BankFormat::Ofx => bank::parse_ofx(&text).map_err(runtime)?,
    };

    let account_id = AccountId(account.to_string());
    let statement = NewStatement {
        period_start: from,
        period_end: to,
        opening_balance: opening,
        closing_balance: closing,
        lines,
    };
    let report = ledger
        .import_statement(company, &account_id, statement, Utc::now())
        .map_err(runtime)?;
    println!(
        "imported statement {} - inserted {}, skipped (duplicate) {}",
        report.statement_id, report.inserted, report.skipped_duplicate
    );
    Ok(())
}

fn run_bank_match(
    db: &std::path::Path,
    company: &str,
    statement: &str,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let report = ledger
        .match_statement(company, statement, &MatchRules::default(), Utc::now())
        .map_err(runtime)?;
    println!("exact matches      : {}", report.exact);
    println!("settlement matches : {}", report.settlement);
    println!("proposals          : {}", report.proposals.len());
    for proposal in &report.proposals {
        println!(
            "  - {} -> {} ({:?}, {})",
            proposal.line_id, proposal.suggested_account.0, proposal.kind, proposal.reason
        );
    }
    Ok(())
}

fn run_bank_confirm(
    db: &std::path::Path,
    company: &str,
    line: &str,
    account: &str,
    class: Option<&str>,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let account_id = AccountId(account.to_string());
    let class_id = class.map(|c| ClassId(c.to_string()));
    let entry_id = ledger
        .confirm_proposal(company, line, &account_id, class_id, Utc::now())
        .map_err(runtime)?;
    println!("confirmed {line} -> posted entry {entry_id}");
    Ok(())
}

fn run_bank_close(
    db: &std::path::Path,
    company: &str,
    statement: &str,
) -> Result<ExitCode, CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    match ledger.close_statement(company, statement, Utc::now()) {
        Ok(close) => {
            println!(
                "closed statement {} at {}",
                close.statement_id, close.closed_at
            );
            Ok(ExitCode::SUCCESS)
        }
        Err(LedgerError::StatementDoesNotClose {
            difference,
            unmatched,
        }) => {
            println!(
                "statement {statement} does not close: off by {} with {unmatched} unmatched line(s)",
                fmt_money(difference)
            );
            Ok(ExitCode::from(1))
        }
        Err(err) => Err(runtime(err)),
    }
}

fn run_bank_status(
    db: &std::path::Path,
    company: &str,
    statement: &str,
) -> Result<(), CommandError> {
    let ledger = Ledger::open(db).map_err(runtime)?;
    let lines = ledger
        .list_bank_lines(company, statement)
        .map_err(runtime)?;

    println!("BANK STATEMENT   {statement}\n");
    println!(
        "{:<38}{:<12}{:>12}  {:<10}description",
        "line", "posted", "amount", "match"
    );
    for line in &lines {
        println!(
            "{:<38}{:<12}{:>12}  {:<10}{}",
            line.line_id,
            line.posted_on,
            fmt_money(line.amount),
            line.match_kind.as_deref().unwrap_or("unmatched"),
            line.description,
        );
    }

    let matched = lines
        .iter()
        .filter(|line| {
            matches!(
                line.match_kind.as_deref(),
                Some("exact") | Some("settlement") | Some("manual")
            )
        })
        .count();
    let proposed: Vec<_> = lines
        .iter()
        .filter(|line| line.match_kind.as_deref() == Some("proposed"))
        .collect();
    let unresolved = lines
        .iter()
        .filter(|line| line.match_kind.is_none())
        .count();
    println!(
        "\nmatched: {matched}   proposals: {}   unresolved (not yet matched): {unresolved}",
        proposed.len()
    );

    if !proposed.is_empty() {
        println!("\nPROPOSALS:");
        for line in proposed {
            let (account, reason) = bank::suggest_account(&line.description);
            println!(
                "  - {} {} suggests {} ({reason})",
                line.line_id,
                fmt_money(line.amount),
                account.0
            );
        }
    }
    Ok(())
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
