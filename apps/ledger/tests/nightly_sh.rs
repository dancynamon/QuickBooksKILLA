//! `tools/nightly.sh run`, end to end, on a scratch replica and a scratch
//! ledger (docs/NIGHTLY.md, "Switching it on"): a clean night, a stale
//! cursor, a dead daemon, a trial balance variance (and its explanation), an
//! aging variance and a failed step, each judged by the exit code, the
//! report JSON and whether a notification went out. Also: `install` refuses
//! without a terminal, and the dry run prints plists that lint.
//!
//! The real `ledger` and `ledger-aging` binaries run. `qbo-local` is a stub
//! that copies the replica for `snapshot` (printing the `snapshot : PATH`
//! line the real command prints) and serves a prepared CSV for `report`,
//! since QBO's side is what each scenario controls. A stand-in process whose
//! command line matches a per-test DAEMON_PATTERN plays the daemon. Nothing
//! here touches the live replica, launchd or the real daemon.
//!
//! Also runs `tools/test_nightly_verdict.py`. Needs `python3` and bash.
//! Every name and amount here is invented.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use chrono::{DateTime, Duration, TimeZone, Utc};
use qbo_local::domain::{EntityType, RealmId};
use qbo_local::store::{MirroredEntity, Store};
use qbo_local::sync::SyncCursor;
use serde_json::{json, Value};

const AS_OF: &str = "2026-08-05";
const AGING_HEADER: &str = "entity_id,name,current,1-30,31-60,61-90,over_90,total\n";
const CLEAN_TB: &str = "qbo_account_id,name,balance\nBANK1,Checking,250.00\nINC1,Sales,-250.00\n";

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf()
}

fn realm() -> RealmId {
    RealmId::parse("1234567890123456").unwrap()
}

fn seeded_at() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 5, 12, 0, 0).unwrap()
}

fn seed(store: &Store, entity_type: EntityType, id: &str, raw: Value) {
    let entity = MirroredEntity {
        entity_type,
        qbo_id: id.to_string(),
        sync_token: "1".into(),
        last_updated_utc: seeded_at(),
        is_deleted: false,
        raw_json: raw,
    };
    store.upsert_entity(&realm(), &entity, seeded_at()).unwrap();
    store.project_entity(&realm(), &entity, seeded_at()).unwrap();
}

/// One non-taxable sales receipt to the bank (the same book `cli.rs`'s
/// nightly tests use, so CLEAN_TB ties to the cent), with every entity
/// type's CDC cursor set to `cursor`.
fn build_replica(path: &Path, cursor: DateTime<Utc>) {
    let store = Store::open(path).unwrap();
    store.register_realm(&realm(), "Aquamentor", seeded_at()).unwrap();
    seed(&store, EntityType::Customer, "31",
        json!({ "Id": "31", "DisplayName": "Blue Harbor Swim Club", "Active": true, "Taxable": false }));
    seed(&store, EntityType::Class, "4", json!({ "Id": "4", "Name": "Foam Products", "Active": true }));
    seed(&store, EntityType::Account, "BANK1",
        json!({ "Id": "BANK1", "Name": "Checking", "AccountType": "Bank", "Active": true }));
    seed(&store, EntityType::Account, "INC1",
        json!({ "Id": "INC1", "Name": "Sales", "AccountType": "Income",
                "AccountSubType": "SalesOfProductIncome", "Active": true }));
    seed(&store, EntityType::Item, "601",
        json!({ "Id": "601", "Name": "CNC Job Work", "Sku": "CNC-1", "Type": "Service",
                "IncomeAccountRef": { "value": "INC1" }, "ClassRef": { "value": "4" }, "Active": true }));
    seed(&store, EntityType::SalesReceipt, "SR-1", json!({
        "Id": "SR-1", "DocNumber": "SR1001", "TxnDate": AS_OF,
        "CustomerRef": { "value": "31" }, "TotalAmt": 250.00,
        "DepositToAccountRef": { "value": "BANK1" },
        "Line": [{
            "Id": "1", "LineNum": 1, "Amount": 250.00, "DetailType": "SalesItemLineDetail",
            "SalesItemLineDetail": {
                "ItemRef": { "value": "601" }, "Qty": 1, "UnitPrice": 250.00,
                "ClassRef": { "value": "4" }, "TaxCodeRef": { "value": "NON" }
            }
        }]
    }));
    for entity_type in [EntityType::Customer, EntityType::Item, EntityType::SalesReceipt] {
        store
            .save_cursor(&SyncCursor {
                realm_id: realm(),
                entity_type,
                last_cdc_cursor: Some(cursor),
                last_full_sweep: Some(cursor),
            })
            .unwrap();
    }
}

const STUB_QBO_LOCAL: &str = r#"#!/bin/bash
# Test stand-in for qbo-local: snapshot copies, report serves $STUB_QBO/<name>.csv.
set -e
cmd=$1; shift
while [ $# -gt 0 ]; do
  case $1 in
    --db) db=$2; shift 2 ;; --dir) dir=$2; shift 2 ;; --name) name=$2; shift 2 ;;
    --out) out=$2; shift 2 ;; *) shift ;;
  esac
done
case $cmd in
  snapshot) mkdir -p "$dir"; cp "$db" "$dir/replica-stub.sqlite"
            echo "snapshot : $dir/replica-stub.sqlite"; echo "pruned   : 0" ;;
  report) [ -f "$STUB_QBO/$name.csv" ] || { echo "stub: no $name report" >&2; exit 1; }
          cp "$STUB_QBO/$name.csv" "$out" ;;
  *) echo "stub: unexpected $cmd" >&2; exit 2 ;;
esac
"#;

/// A process whose command line `pgrep -f` matches, killed on drop.
struct FakeDaemon(Child);

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct Night {
    dir: tempfile::TempDir,
    pattern: String,
}

impl Night {
    fn new(cursor_age: Duration) -> Night {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        build_replica(&root.join("replica.sqlite"), Utc::now() - cursor_age);
        let init = Command::new(env!("CARGO_BIN_EXE_ledger"))
            .args(["init", "--db"])
            .arg(root.join("ledger.sqlite"))
            .args(["--company", "aquamentor", "--name", "Aquamentor LLC", "--realm", realm().as_str()])
            .output()
            .unwrap();
        assert!(init.status.success(), "ledger init: {}", String::from_utf8_lossy(&init.stderr));

        let stub = root.join("qbo-local-stub");
        fs::write(&stub, STUB_QBO_LOCAL).unwrap();
        chmod_x(&stub);
        let notify = root.join("notify-stub");
        fs::write(&notify, "#!/bin/bash\nprintf '%s | %s\\n' \"$1\" \"$2\" >>\"$(dirname \"$0\")/notifications.log\"\n").unwrap();
        chmod_x(&notify);
        fs::create_dir_all(root.join("qbo")).unwrap();

        let pattern = format!("qbo-local daemon --nightly-test {}", root.display());
        let conf = format!(
            "REALM={realm}\nCOMPANY=aquamentor\nLEDGER_DB=\"{r}/ledger.sqlite\"\nREPLICA=\"{r}/replica.sqlite\"\n\
             FROM=\nREPORT_DIR=\"{r}/reports\"\nKEEP=2\nQBO_LOCAL=\"{stub}\"\nLEDGER=\"{ledger}\"\n\
             LEDGER_AGING=\"{aging}\"\nQBO_CONFIG=\"{r}/config.toml\"\nQBO_REPORT_SOURCE=\"--replay {r}/qbo\"\n\
             EXPLAINED=\"{r}/explained.csv\"\nDAEMON_PATTERN=\"{pattern}\"\nNOTIFY_CMD=\"{notify}\"\n\
             DAEMON_LABEL=com.aquamentor.test.{tag}.daemon\nNIGHTLY_LABEL=com.aquamentor.test.{tag}.nightly\n\
             LAUNCH_AGENTS=\"{r}/LaunchAgents\"\n",
            realm = realm().as_str(),
            r = root.display(),
            stub = stub.display(),
            ledger = env!("CARGO_BIN_EXE_ledger"),
            aging = env!("CARGO_BIN_EXE_ledger-aging"),
            notify = notify.display(),
            tag = root.file_name().unwrap().to_string_lossy().replace('.', "-"),
        );
        fs::write(root.join("nightly.conf"), conf).unwrap();
        fs::write(root.join("config.toml"), "").unwrap();
        let night = Night { dir, pattern };
        night.qbo("trial-balance", CLEAN_TB);
        night.qbo("ar-aging", AGING_HEADER);
        night.qbo("ap-aging", AGING_HEADER);
        night
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn qbo(&self, name: &str, csv: &str) {
        fs::write(self.root().join("qbo").join(format!("{name}.csv")), csv).unwrap();
    }

    fn daemon(&self) -> FakeDaemon {
        let child = Command::new("bash")
            .arg("-c")
            .arg(format!("exec -a '{}' sleep 120", self.pattern))
            .spawn()
            .unwrap();
        // Until exec has happened, pgrep would not see the new command line.
        for _ in 0..100 {
            let seen = Command::new("pgrep").args(["-f", &self.pattern]).output().unwrap();
            if seen.status.success() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        FakeDaemon(child)
    }

    fn script(&self, args: &[&str], as_of: &str) -> Output {
        Command::new("bash")
            .arg(repo_root().join("tools/nightly.sh"))
            .args(args)
            .env("NIGHTLY_CONFIG", self.root().join("nightly.conf"))
            .env("STUB_QBO", self.root().join("qbo"))
            .env("AS_OF", as_of)
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn run(&self) -> (i32, Value) {
        self.run_as_of(AS_OF)
    }

    fn run_as_of(&self, as_of: &str) -> (i32, Value) {
        let out = self.script(&["run"], as_of);
        let code = out.status.code().unwrap_or(-1);
        let report = self.root().join("reports").join(format!("nightly-{as_of}.json"));
        let text = fs::read_to_string(&report).unwrap_or_else(|_| {
            panic!(
                "no report (exit {code}): {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            )
        });
        let json: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(json["exit_code"], code, "report and exit code disagree: {json}");
        (code, json)
    }

    fn notifications(&self) -> String {
        fs::read_to_string(self.root().join("notifications.log")).unwrap_or_default()
    }

    fn step_names(report: &Value) -> Vec<String> {
        report["steps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["name"].as_str().unwrap().to_string())
            .collect()
    }
}

fn chmod_x(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

fn reasons(report: &Value) -> String {
    report["reasons"].to_string()
}

#[test]
fn a_clean_night_is_green_writes_both_reports_and_stays_silent() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    let (code, report) = night.run();
    assert_eq!(code, 0, "{}", reasons(&report));
    assert_eq!(report["verdict"], "green");
    assert_eq!(report["tb"]["must_failures"], 0);
    assert_eq!(report["replica"]["fresh"], true);
    assert_eq!(report["aging"]["ar"]["differing"], 0);
    assert_eq!(
        Night::step_names(&report),
        ["config", "daemon", "snapshot", "freshness", "qbo-tb", "qbo-ar", "qbo-ap", "ledger-nightly", "ledger-aging"]
    );
    let md = fs::read_to_string(night.root().join("reports/nightly-2026-08-05.md")).unwrap();
    assert!(md.contains("2026-08-05: GREEN"), "{md}");
    assert!(night.root().join("reports/tbdiff/tbdiff-2026-08-05.csv").exists());
    assert_eq!(night.notifications(), "", "a green night posts nothing");
}

#[test]
fn a_stale_cursor_exits_3_notifies_and_never_replays() {
    let night = Night::new(Duration::hours(30));
    let _daemon = night.daemon();
    let (code, report) = night.run();
    assert_eq!(code, 3, "{}", reasons(&report));
    assert_eq!(report["replica"]["fresh"], false);
    assert!(reasons(&report).contains("replica stale"), "{}", reasons(&report));
    assert!(!Night::step_names(&report).contains(&"ledger-nightly".to_string()));
    let notes = night.notifications();
    assert!(notes.contains("RED (exit 3)") && notes.contains("replica stale"), "{notes}");
}

#[test]
fn a_dead_daemon_exits_3() {
    let night = Night::new(Duration::minutes(10));
    let (code, report) = night.run();
    assert_eq!(code, 3);
    assert_eq!(report["daemon"]["alive"], false);
    assert!(night.notifications().contains("daemon is not running"));
}

#[test]
fn a_trial_balance_variance_exits_1_until_a_recorded_decision_explains_it() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    night.qbo("trial-balance", "qbo_account_id,name,balance\nBANK1,Checking,240.00\nINC1,Sales,-250.00\n");
    let (code, report) = night.run();
    assert_eq!(code, 1, "{}", reasons(&report));
    assert!(reasons(&report).contains("Bank — Checking (1100) (must tier) off by 10.00"), "{}", reasons(&report));
    assert!(night.notifications().contains("RED (exit 1)"));

    fs::write(
        night.root().join("explained.csv"),
        "kind,key,delta,decision,note\ntb,Bank — Checking (1100),10.00,D34,invented for this test\n",
    )
    .unwrap();
    let (code, report) = night.run();
    assert_eq!(code, 0, "{}", reasons(&report));
}

#[test]
fn an_aging_variance_exits_1() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    night.qbo("ar-aging", &format!("{AGING_HEADER}31,Blue Harbor Swim Club,0.00,0.00,0.00,0.00,75.00,75.00\n"));
    let (code, report) = night.run();
    assert_eq!(code, 1);
    assert_eq!(report["aging"]["ar"]["rows"][0]["contact"], "31");
    assert_eq!(report["aging"]["ar"]["rows"][0]["delta"], "-75.00");
}

#[test]
fn a_failed_qbo_report_exits_4_and_skips_the_replay() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    fs::remove_file(night.root().join("qbo/trial-balance.csv")).unwrap();
    let (code, report) = night.run();
    assert_eq!(code, 4, "{}", reasons(&report));
    let nightly = report["steps"].as_array().unwrap().iter().find(|s| s["name"] == "ledger-nightly").unwrap();
    assert_eq!(nightly["status"], "skipped");
    assert!(night.notifications().contains("RED (exit 4)"));
}

#[test]
fn an_unexpected_script_error_still_reports_and_notifies() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    // Exits 0 but writes no CSVs, so the script's own `mv` fails under set -e.
    let conf = night.root().join("nightly.conf");
    let mut text = fs::read_to_string(&conf).unwrap();
    text.push_str("LEDGER_AGING=/usr/bin/true\n");
    fs::write(&conf, text).unwrap();
    let (code, report) = night.run();
    assert_eq!(code, 4, "{}", reasons(&report));
    assert!(reasons(&report).contains("step script failed"), "{}", reasons(&report));
    assert!(night.notifications().contains("RED (exit 4)"));
}

#[test]
fn only_the_newest_keep_nights_are_kept() {
    let night = Night::new(Duration::minutes(10));
    let _daemon = night.daemon();
    for as_of in ["2026-08-05", "2026-08-06", "2026-08-07"] {
        let (code, report) = night.run_as_of(as_of);
        assert_eq!(code, 0, "{as_of}: {}", reasons(&report));
    }
    let reports = night.root().join("reports");
    assert!(!reports.join("nightly-2026-08-05.md").exists());
    assert!(!reports.join("runs/2026-08-05").exists());
    assert!(reports.join("nightly-2026-08-07.json").exists());
    assert!(reports.join("tbdiff/tbdiff-2026-08-05.csv").exists(), "tbdiff files are never pruned");
}

#[test]
fn install_refuses_without_a_terminal_and_writes_nothing() {
    let night = Night::new(Duration::minutes(10));
    let out = night.script(&["install"], AS_OF);
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stdout));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Dan's step"));
    assert!(!night.root().join("LaunchAgents").exists());
}

#[test]
fn the_dry_run_prints_two_plists_that_lint_and_no_realm_placeholder() {
    let night = Night::new(Duration::minutes(10));
    let out = night.script(&["install", "--dry-run"], AS_OF);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(!text.contains("@REALM@") && !text.contains("@ROOT@"), "{text}");
    assert!(text.contains("launchctl bootstrap gui/"), "{text}");
    let plists: Vec<&str> = text
        .split("# ---- ")
        .skip(1)
        .map(|chunk| {
            let body = &chunk[chunk.find('\n').unwrap() + 1..];
            &body[..body.find("</plist>").unwrap() + "</plist>".len()]
        })
        .collect();
    assert_eq!(plists.len(), 2);
    assert!(plists[0].contains(&format!("<string>{}</string>", realm().as_str())));
    assert!(plists[1].contains("<integer>30</integer>"));
    if Path::new("/usr/bin/plutil").exists() {
        for (i, plist) in plists.iter().enumerate() {
            let path = night.root().join(format!("p{i}.plist"));
            fs::write(&path, plist).unwrap();
            let lint = Command::new("/usr/bin/plutil").arg("-lint").arg(&path).output().unwrap();
            assert!(lint.status.success(), "{}", String::from_utf8_lossy(&lint.stdout));
        }
    }
}

#[test]
fn the_verdict_unit_tests_pass() {
    let out = Command::new("python3")
        .args(["-m", "unittest", "tools/test_nightly_verdict.py"])
        .current_dir(repo_root())
        .output()
        .expect("python3 on PATH");
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}
