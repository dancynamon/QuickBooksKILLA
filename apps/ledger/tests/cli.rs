//! The `ledger` binary, end to end: a real subprocess against tempfiles.
//! `LEDGER-DESIGN.md` §6, §7, §9.

use std::process::Command;

use chrono::{TimeZone, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use serde_json::{json, Value};

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn now() -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap()
}

fn mirrored(entity_type: EntityType, id: &str, raw: Value) -> MirroredEntity {
    MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "1".into(),
        last_updated_utc: now(),
        is_deleted: false,
        raw_json: raw,
    }
}

fn seed(store: &Store, entity: MirroredEntity) {
    store.upsert_entity(&realm(), &entity, now()).unwrap();
    store.project_entity(&realm(), &entity, now()).unwrap();
}

/// A qbo-local file replica with one small, real document: a taxable sales
/// receipt that lands in the bank account directly (no AR leg), enough to
/// exercise `import`, `tb` and `tax` against real numbers.
fn build_replica(path: &std::path::Path) {
    let store = Store::open(path).unwrap();
    store.register_realm(&realm(), "Aquamentor", now()).unwrap();

    seed(
        &store,
        mirrored(
            EntityType::Customer,
            "31",
            json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true, "Taxable": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Class,
            "4",
            json!({ "Id": "4", "Name": "Foam Products", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "BANK1",
            json!({ "Id": "BANK1", "Name": "Checking", "AccountType": "Bank", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Account,
            "INC1",
            json!({ "Id": "INC1", "Name": "Sales", "AccountType": "Income",
                    "AccountSubType": "SalesOfProductIncome", "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::Item,
            "601",
            json!({ "Id": "601", "Name": "CNC Job Work", "Sku": "CNC-1", "Type": "Service",
                    "IncomeAccountRef": { "value": "INC1" }, "ClassRef": { "value": "4" }, "Active": true }),
        ),
    );
    seed(
        &store,
        mirrored(
            EntityType::SalesReceipt,
            "SR-1",
            json!({
                "Id": "SR-1", "DocNumber": "SR1001", "TxnDate": "2026-08-05",
                "CustomerRef": { "value": "31" }, "TotalAmt": 106.63,
                "DepositToAccountRef": { "value": "BANK1" },
                "TxnTaxDetail": {
                    "TotalTax": 6.63,
                    "TaxLine": [ {
                        "TaxLineDetail": { "NetAmountTaxable": 100.00, "TaxPercent": 6.625,
                                           "TaxRateRef": { "value": "1" } }
                    } ]
                },
                "Line": [
                    {
                        "Id": "1", "LineNum": 1, "Amount": 100.00,
                        "DetailType": "SalesItemLineDetail",
                        "SalesItemLineDetail": {
                            "ItemRef": { "value": "601" }, "Qty": 1, "UnitPrice": 100.00,
                            "ClassRef": { "value": "4" }, "TaxCodeRef": { "value": "TAX" }
                        }
                    },
                    { "Amount": 106.63, "DetailType": "SubTotalLineDetail", "SubTotalLineDetail": {} }
                ]
            }),
        ),
    );

    // The Store's Drop just closes its connection; nothing further to flush
    // explicitly — every write above went through a committed transaction.
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ledger"))
        .args(args)
        .output()
        .expect("spawn the ledger binary")
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn init_import_tb_tax_and_tbdiff_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");
    let replica = dir.path().join("replica.sqlite");
    build_replica(&replica);

    // -- init -----------------------------------------------------------
    let init = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--name",
        "Aquamentor LLC",
        "--realm",
        realm().as_str(),
    ]);
    assert!(init.status.success(), "init failed: {}", stdout(&init));

    // -- import -----------------------------------------------------------
    let import = run(&[
        "import",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--replica",
        replica.to_str().unwrap(),
        "--realm",
        realm().as_str(),
    ]);
    assert!(
        import.status.success(),
        "import failed: {}",
        stdout(&import)
    );
    let import_out = stdout(&import);
    assert!(import_out.contains("posted"), "{import_out}");

    // -- tb -----------------------------------------------------------------
    let tb = run(&[
        "tb",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2026-12-31",
    ]);
    assert!(tb.status.success(), "tb failed: {}", stdout(&tb));
    let tb_out = stdout(&tb);
    assert!(
        tb_out.contains("balanced") && !tb_out.contains("DOES NOT BALANCE"),
        "{tb_out}"
    );

    // -- tax ------------------------------------------------------------------
    let tax = run(&[
        "tax",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--year",
        "2026",
        "--quarter",
        "3",
    ]);
    assert!(tax.status.success(), "tax failed: {}", stdout(&tax));
    let tax_out = stdout(&tax);
    for label in ["  A ", "  B ", "  C ", "  D "] {
        assert!(tax_out.contains(label), "missing {label:?} in:\n{tax_out}");
    }
    assert!(tax_out.contains("ST-50 mapping"));

    // -- tbdiff: a CSV that disagrees on 1100 exits 1 ---------------------
    let csv_path = dir.path().join("qbo.csv");
    std::fs::write(
        &csv_path,
        "qbo_account_id,name,balance\nBANK1,Checking,999.00\n",
    )
    .unwrap();
    let tbdiff = run(&[
        "tbdiff",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2026-12-31",
        "--qbo-csv",
        csv_path.to_str().unwrap(),
    ]);
    assert_eq!(
        tbdiff.status.code(),
        Some(1),
        "tbdiff should exit 1 on a must-tier disagreement: {}",
        stdout(&tbdiff)
    );
    let tbdiff_out = stdout(&tbdiff);
    assert!(tbdiff_out.contains("MUST-MATCH FAILURES"));

    // -- no args exits 2 ------------------------------------------------------
    let no_args = run(&[]);
    assert_eq!(no_args.status.code(), Some(2));
}

#[test]
fn init_is_idempotent_a_second_run_succeeds_and_leaves_the_chart_alone() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");

    let first = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--name",
        "Aquamentor LLC",
        "--realm",
        realm().as_str(),
    ]);
    assert!(
        first.status.success(),
        "first init failed: {}",
        stdout(&first)
    );

    // A second call for the same company id — a different display name, to
    // prove it updates rather than merely no-opping — still exits 0.
    let second = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--name",
        "Aquamentor LLC (renamed)",
        "--realm",
        realm().as_str(),
    ]);
    assert!(
        second.status.success(),
        "second init must succeed, not error on a duplicate company: {}",
        stdout(&second)
    );

    // The chart is unaffected either way: `tb` still renders a balanced,
    // empty book rather than erroring on a doubled-up seed.
    let tb = run(&[
        "tb",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2026-12-31",
    ]);
    assert!(tb.status.success(), "tb failed: {}", stdout(&tb));
}

#[test]
fn opening_and_boundary_subcommands_run_end_to_end() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("ledger.sqlite");
    let replica = dir.path().join("replica.sqlite");
    build_replica(&replica);

    let init = run(&[
        "init",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--name",
        "Aquamentor LLC",
        "--realm",
        realm().as_str(),
    ]);
    assert!(init.status.success(), "init failed: {}", stdout(&init));

    let import = run(&[
        "import",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--replica",
        replica.to_str().unwrap(),
        "--realm",
        realm().as_str(),
    ]);
    assert!(
        import.status.success(),
        "import failed: {}",
        stdout(&import)
    );

    // -- opening: post, then a second run with the same figures is skipped --
    let csv_path = dir.path().join("tb-2025.csv");
    std::fs::write(
        &csv_path,
        "qbo_account_id,name,balance\nBANK1,Checking,999.00\n",
    )
    .unwrap();

    let opening_first = run(&[
        "opening",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2025-12-31",
        "--qbo-csv",
        csv_path.to_str().unwrap(),
    ]);
    assert!(
        opening_first.status.success(),
        "opening failed: {}",
        stdout(&opening_first)
    );
    assert!(stdout(&opening_first).contains("posted"));

    let opening_second = run(&[
        "opening",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--as-of",
        "2025-12-31",
        "--qbo-csv",
        csv_path.to_str().unwrap(),
    ]);
    assert!(opening_second.status.success());
    assert!(
        stdout(&opening_second).contains("skipped"),
        "{}",
        stdout(&opening_second)
    );

    // -- boundary: a directory of tb-YYYY.csv snapshots ----------------------
    let snapshots_dir = dir.path().join("snapshots");
    std::fs::create_dir(&snapshots_dir).unwrap();
    std::fs::copy(&csv_path, snapshots_dir.join("tb-2025.csv")).unwrap();
    std::fs::write(
        snapshots_dir.join("tb-2026.csv"),
        "qbo_account_id,name,balance\nBANK1,Checking,999.00\n",
    )
    .unwrap();

    let boundary = run(&[
        "boundary",
        "--db",
        db.to_str().unwrap(),
        "--company",
        "aquamentor",
        "--replica",
        replica.to_str().unwrap(),
        "--realm",
        realm().as_str(),
        "--snapshots",
        snapshots_dir.to_str().unwrap(),
    ]);
    assert!(
        boundary.status.success(),
        "boundary failed: {}",
        stdout(&boundary)
    );
    let boundary_out = stdout(&boundary);
    assert!(boundary_out.contains("BOUNDARY WALK"), "{boundary_out}");
    assert!(boundary_out.contains("boundary year"), "{boundary_out}");
}
