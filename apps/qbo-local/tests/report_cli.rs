//! `qbo-local report` and `qbo-local snapshots`, end to end, replayed against
//! committed synthetic fixtures. `LEDGER-DESIGN.md` §6, §7.
//!
//! Names and figures are invented (`HANDOFF.md` §2.6).

use std::path::Path;
use std::process::{Command, Output};

const REALM: &str = "1234567890123456";

/// `apps/qbo-local/tests/fixtures/synthetic/reports/`, the fixtures the
/// report/snapshots commands below replay against.
fn fixtures_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/synthetic"))
}

fn qbo_local(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qbo-local"))
        .args(args)
        .output()
        .expect("failed to run the qbo-local binary")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn report_trial_balance_replays_and_writes_the_tbdiff_csv_shape() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("tb.csv");

    let output = qbo_local(&[
        "report",
        "--realm",
        REALM,
        "--name",
        "trial-balance",
        "--as-of",
        "2026-09-10",
        "--out",
        out.to_str().unwrap(),
        "--replay",
        fixtures_dir().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "report failed: {}", stderr(&output));

    let csv = std::fs::read_to_string(&out).unwrap();
    let mut lines = csv.lines();
    assert_eq!(lines.next(), Some("qbo_account_id,name,balance"));
    assert_eq!(lines.next(), Some("35,Checking,412884.19"));
    assert_eq!(lines.next(), Some("42,Accounts Receivable,12345.67"));
    assert_eq!(lines.next(), Some("61,Sales Tax Payable,-8412.06"));
    assert_eq!(lines.next(), Some("70,Owner Capital,-416817.80"));
    assert_eq!(lines.next(), None);

    assert!(stdout(&output).contains("4 rows"), "{}", stdout(&output));
}

#[test]
fn report_ar_aging_replays_and_ignores_the_grand_total_row() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("ar.csv");

    let output = qbo_local(&[
        "report",
        "--realm",
        REALM,
        "--name",
        "ar-aging",
        "--as-of",
        "2026-09-10",
        "--out",
        out.to_str().unwrap(),
        "--replay",
        fixtures_dir().to_str().unwrap(),
    ]);
    assert!(output.status.success(), "report failed: {}", stderr(&output));

    let csv = std::fs::read_to_string(&out).unwrap();
    let mut lines = csv.lines();
    assert_eq!(
        lines.next(),
        Some("entity_id,name,current,1-30,31-60,61-90,over_90,total")
    );
    assert_eq!(
        lines.next(),
        Some("31,Blue Harbor Swim Club,1200.00,300.00,0.00,0.00,0.00,1500.00")
    );
    assert_eq!(
        lines.next(),
        Some("44,Fairview County Parks & Rec.,0.00,0.00,830.00,0.00,0.00,830.00")
    );
    // The fixture's trailing "TOTAL" row must never show up as a third
    // customer.
    assert_eq!(lines.next(), None);
}

#[test]
fn report_without_live_or_replay_exits_2() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("tb.csv");
    let output = qbo_local(&[
        "report",
        "--realm",
        REALM,
        "--name",
        "trial-balance",
        "--as-of",
        "2026-09-10",
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        stderr(&output).contains("--live") && stderr(&output).contains("--replay"),
        "{}",
        stderr(&output)
    );
}

#[test]
fn report_also_records_when_record_dir_is_given() {
    let directory = tempfile::tempdir().unwrap();
    let out = directory.path().join("tb.csv");
    let record_dir = directory.path().join("recorded");

    let output = qbo_local(&[
        "report",
        "--realm",
        REALM,
        "--name",
        "trial-balance",
        "--as-of",
        "2026-09-10",
        "--out",
        out.to_str().unwrap(),
        "--replay",
        fixtures_dir().to_str().unwrap(),
        "--record",
        record_dir.to_str().unwrap(),
    ]);
    assert!(output.status.success(), "report failed: {}", stderr(&output));

    assert!(record_dir
        .join("reports/TrialBalance-none-2026-09-10-Accrual-none-none.json")
        .exists());
}

#[test]
fn snapshots_writes_one_tb_csv_per_year_from_replayed_fixtures() {
    let directory = tempfile::tempdir().unwrap();
    let out_dir = directory.path().join("snapshots");

    let output = qbo_local(&[
        "snapshots",
        "--realm",
        REALM,
        "--from-year",
        "2024",
        "--to-year",
        "2025",
        "--dir",
        out_dir.to_str().unwrap(),
        "--replay",
        fixtures_dir().to_str().unwrap(),
    ]);
    assert!(
        output.status.success(),
        "snapshots failed: {}",
        stderr(&output)
    );

    let tb_2024 = std::fs::read_to_string(out_dir.join("tb-2024.csv")).unwrap();
    assert_eq!(
        tb_2024,
        "qbo_account_id,name,balance\n35,Checking,350000.00\n70,Owner Capital,-350000.00\n"
    );
    let tb_2025 = std::fs::read_to_string(out_dir.join("tb-2025.csv")).unwrap();
    assert_eq!(
        tb_2025,
        "qbo_account_id,name,balance\n35,Checking,391204.55\n70,Owner Capital,-391204.55\n"
    );
}

#[test]
fn snapshots_rejects_from_year_after_to_year() {
    let directory = tempfile::tempdir().unwrap();
    let out_dir = directory.path().join("snapshots");
    let output = qbo_local(&[
        "snapshots",
        "--realm",
        REALM,
        "--from-year",
        "2026",
        "--to-year",
        "2024",
        "--dir",
        out_dir.to_str().unwrap(),
        "--replay",
        fixtures_dir().to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(stderr(&output).contains("--from-year"), "{}", stderr(&output));
}
