//! The SurePayroll pay-run journal. `LEDGER-DESIGN.md` §12, D38,
//! `docs/PAYROLL.md`.
//!
//! Every employee, id and amount here is invented. Real payroll data never
//! enters this repository.

use std::process::Command;

use chrono::{NaiveDate, Utc};
use ledger::payroll::{self, PayrollConfig, PayrollError, RunStatus};
use ledger::report;
use ledger::store::Ledger;
use ledger_core::Money;

const COMPANY: &str = "aquamentor";

/// Three invented employees on one invented run. E-100 is mapped by id,
/// E-200 by department, E-300 by id onto a 6xxx wages account.
///
/// | employee | class | gross   | withheld | employer | net     |
/// |----------|-------|---------|----------|----------|---------|
/// | E-100    | foam  | 1150.00 | 207.35   | 127.08   | 942.65  |
/// | E-200    | chair |  800.00 | 161.20   |  61.20   | 638.80  |
/// | E-300    | sign  |  500.00 |  68.25   |  53.25   | 431.75  |
const RUN_CSV: &str = "\
run_id,pay_date,period_start,period_end,employee,department,section,item,amount
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,earning,REGULAR,\"1,000.00\"
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,earning,O/TIME,150.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employee_tax,FED WTH,90.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employee_tax,FICA,71.30
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employee_tax,MEDFICA,16.68
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employee_tax,STATE-NJ,25.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employee_tax,NJ SUI EE,4.37
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employer_tax,CO FICA,71.30
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employer_tax,CO MEDC,16.68
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employer_tax,FUTA,6.90
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,employer_tax,CO UNEM-NJ,32.20
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-100,,net_pay,NET PAY,942.65
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,earning,SALARY,800.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employee_tax,FED WTH,60.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employee_tax,FICA,49.60
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employee_tax,MEDFICA,11.60
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employee_deduction,401K,40.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employer_tax,CO FICA,49.60
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,employer_tax,CO MEDC,11.60
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-200,Shop,net_pay,NET PAY,638.80
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,earning,REGULAR,500.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,noncash_earning,USHL,25.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employee_tax,FED WTH,30.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employee_tax,FICA,31.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employee_tax,MEDFICA,7.25
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employer_tax,CO FICA,31.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employer_tax,CO MEDC,7.25
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,employer_deduction,401K MATCH,15.00
TEST-RUN-1,04/09/2027,03/27/2027,04/09/2027,E-300,,net_pay,NET PAY,431.75
";

const CONFIG: &str = r#"
[intuit]
client_id = "ignored by the payroll reader"

[payroll]
bank_account = "1100"

[payroll.employees]
"E-100" = { class = "foam" }
"E-300" = { class = "sign", wages_account = "6900" }

[payroll.departments]
"Shop" = { class = "chair" }
"#;

const DOCUMENT_ID: &str = "payroll:2027-04-09:TEST-RUN-1";

fn seeded() -> Ledger {
    let ledger = Ledger::open_in_memory().unwrap();
    ledger
        .create_company(COMPANY, "Test Co", None, Utc::now())
        .unwrap();
    ledger
}

fn config() -> PayrollConfig {
    PayrollConfig::parse(CONFIG).unwrap()
}

fn import(ledger: &Ledger, csv: &str) -> Result<Vec<payroll::RunOutcome>, PayrollError> {
    let runs = payroll::parse_csv(csv)?;
    payroll::import_runs(ledger, COMPANY, &runs, &config(), "test", Utc::now())
}

fn m(minor: i64) -> Money {
    Money::from_minor(minor)
}

fn date(y: i32, mo: u32, d: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, mo, d).unwrap()
}

fn balance(ledger: &Ledger, account: &str) -> Money {
    report::trial_balance(ledger, COMPANY, date(2027, 12, 31))
        .unwrap()
        .rows
        .into_iter()
        .find(|row| row.number == account)
        .map_or(Money::ZERO, |row| row.balance)
}

#[test]
fn one_run_posts_one_flagged_balanced_entry_by_class() {
    let ledger = seeded();
    let outcomes = import(&ledger, RUN_CSV).unwrap();
    assert_eq!(outcomes.len(), 1);
    let outcome = &outcomes[0];
    assert_eq!(outcome.status, RunStatus::Posted);
    assert_eq!(outcome.document_id, DOCUMENT_ID);
    assert_eq!(outcome.totals.gross, m(245_000));
    assert_eq!(outcome.totals.employer_costs, m(24_153));
    assert_eq!(outcome.totals.liabilities, m(67_833));
    assert_eq!(outcome.totals.net_pay, m(201_320));
    assert_eq!(outcome.totals.noncash, m(2_500));

    let (entry, posted) = ledger.entry(COMPANY, &outcome.entry_id).unwrap();
    assert!(posted);
    assert!(entry.is_flagged, "§4: a journal entry is flagged");
    assert!(entry.is_balanced());
    assert_eq!(entry.entry_date, date(2027, 4, 9));
    assert_eq!(entry.total_debits().unwrap(), m(269_153));

    let debit = |account: &str, class: &str| {
        entry
            .lines
            .iter()
            .find(|l| {
                l.account.0 == account && l.class.as_ref().map(|c| c.0.as_str()) == Some(class)
            })
            .map(|l| l.debit)
    };
    // Wages plus the employer taxes on them, per class.
    assert_eq!(debit("5400", "foam"), Some(m(127_708)));
    assert_eq!(debit("5400", "chair"), Some(m(86_120)));
    assert_eq!(debit("6900", "sign"), Some(m(55_325)));

    assert_eq!(balance(&ledger, "2400"), m(-67_833));
    assert_eq!(balance(&ledger, "1100"), m(-201_320));

    // No person's name or pay in the book: memo and descriptions only name
    // the run and SurePayroll items.
    let payload = ledger
        .latest_document_payload(COMPANY, DOCUMENT_ID)
        .unwrap()
        .unwrap();
    for id in ["E-100", "E-200", "E-300", "Shop"] {
        assert!(!payload.contains(id), "{id} leaked into the document");
    }
}

#[test]
fn reimporting_the_same_run_posts_nothing() {
    let ledger = seeded();
    let first = import(&ledger, RUN_CSV).unwrap();
    let oplog = ledger.oplog_len(COMPANY).unwrap();
    let second = import(&ledger, RUN_CSV).unwrap();
    assert_eq!(second[0].status, RunStatus::AlreadyImported);
    assert_eq!(second[0].entry_id, first[0].entry_id);
    assert_eq!(ledger.oplog_len(COMPANY).unwrap(), oplog);
    assert_eq!(
        ledger.document_version_count(COMPANY, DOCUMENT_ID).unwrap(),
        1
    );
    assert_eq!(balance(&ledger, "1100"), m(-201_320));
}

#[test]
fn changed_figures_are_refused_until_the_entry_is_reversed() {
    let ledger = seeded();
    let first = import(&ledger, RUN_CSV).unwrap();
    let changed = RUN_CSV
        .replace(
            "E-300,,earning,REGULAR,500.00",
            "E-300,,earning,REGULAR,510.00",
        )
        .replace(
            "E-300,,net_pay,NET PAY,431.75",
            "E-300,,net_pay,NET PAY,441.75",
        );
    match import(&ledger, &changed) {
        Err(PayrollError::Changed { entry_id, .. }) => assert_eq!(entry_id, first[0].entry_id),
        other => panic!("expected Changed, got {other:?}"),
    }

    ledger
        .reverse_entry(COMPANY, &first[0].entry_id, date(2027, 4, 9), Utc::now())
        .unwrap();
    let again = import(&ledger, &changed).unwrap();
    assert_eq!(again[0].status, RunStatus::Posted);
    assert_eq!(
        ledger.document_version_count(COMPANY, DOCUMENT_ID).unwrap(),
        2
    );
    assert_eq!(balance(&ledger, "1100"), m(-202_320));
}

#[test]
fn an_unmapped_employee_refuses_the_whole_file() {
    let ledger = seeded();
    let second_run = "TEST-RUN-2,04/23/2027,,,E-999,Office,earning,REGULAR,100.00\n";
    let csv = format!("{RUN_CSV}{second_run}");
    match import(&ledger, &csv) {
        Err(PayrollError::Unmapped { run_id, unmapped }) => {
            assert_eq!(run_id, "TEST-RUN-2");
            assert_eq!(unmapped.len(), 1);
            assert!(unmapped[0].contains("E-999") && unmapped[0].contains("Office"));
        }
        other => panic!("expected Unmapped, got {other:?}"),
    }
    assert_eq!(
        ledger.document_version_count(COMPANY, DOCUMENT_ID).unwrap(),
        0
    );
    assert_eq!(ledger.oplog_len(COMPANY).unwrap(), 0);
}

#[test]
fn a_net_pay_that_does_not_tie_to_the_cent_is_refused() {
    let ledger = seeded();
    let off = RUN_CSV.replace("NET PAY,638.80", "NET PAY,638.81");
    match import(&ledger, &off) {
        Err(PayrollError::NetPayMismatch {
            stated, computed, ..
        }) => {
            assert_eq!(stated, "638.81");
            assert_eq!(computed, "638.80");
        }
        other => panic!("expected NetPayMismatch, got {other:?}"),
    }
    assert_eq!(ledger.oplog_len(COMPANY).unwrap(), 0);
}

#[test]
fn a_check_date_in_a_closed_period_is_refused() {
    let ledger = seeded();
    ledger
        .close_period(
            COMPANY,
            date(2027, 4, 30),
            "test",
            "April closed",
            Utc::now(),
        )
        .unwrap();
    let err = import(&ledger, RUN_CSV).unwrap_err();
    assert!(err.to_string().contains("closed period"), "{err}");
    assert_eq!(
        ledger.document_version_count(COMPANY, DOCUMENT_ID).unwrap(),
        0
    );
}

#[test]
fn an_unknown_class_in_the_mapping_leaves_nothing_behind() {
    let ledger = seeded();
    let config = PayrollConfig::parse(&CONFIG.replace("\"foam\"", "\"widgets\"")).unwrap();
    let runs = payroll::parse_csv(RUN_CSV).unwrap();
    let err =
        payroll::import_runs(&ledger, COMPANY, &runs, &config, "test", Utc::now()).unwrap_err();
    assert!(err.to_string().contains("widgets"), "{err}");
    assert_eq!(
        ledger.document_version_count(COMPANY, DOCUMENT_ID).unwrap(),
        0
    );
}

#[test]
fn the_cli_imports_once_and_prints_no_employee() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.db");
    let csv = dir.path().join("run.csv");
    let config = dir.path().join("config.toml");
    std::fs::write(&csv, RUN_CSV).unwrap();
    std::fs::write(&config, CONFIG).unwrap();
    {
        let ledger = Ledger::open(&db).unwrap();
        ledger
            .create_company(COMPANY, "Test Co", None, Utc::now())
            .unwrap();
    }
    let run = |extra: &[&str]| {
        let mut args = vec![
            "payroll",
            "import",
            csv.to_str().unwrap(),
            "--db",
            db.to_str().unwrap(),
            "--company",
            COMPANY,
            "--config",
            config.to_str().unwrap(),
        ];
        args.extend_from_slice(extra);
        Command::new(env!("CARGO_BIN_EXE_ledger"))
            .args(&args)
            .output()
            .unwrap()
    };

    let dry = run(&["--dry-run"]);
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let dry_out = String::from_utf8_lossy(&dry.stdout);
    assert!(dry_out.contains("nothing written"));
    assert_eq!(
        Ledger::open(&db).unwrap().oplog_len(COMPANY).unwrap(),
        0,
        "a dry run writes nothing"
    );

    let first = run(&[]);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let out = String::from_utf8_lossy(&first.stdout);
    assert!(out.contains(&format!("{DOCUMENT_ID} posted")), "{out}");
    assert!(out.contains("2013.20"), "{out}");
    for id in ["E-100", "E-200", "E-300"] {
        assert!(!out.contains(id) && !dry_out.contains(id));
    }

    let second = run(&[]);
    assert!(second.status.success());
    assert!(String::from_utf8_lossy(&second.stdout).contains("already_imported"));
}
